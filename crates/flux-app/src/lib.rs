//! The shared Flux application controller.
//!
//! This crate is the orchestration layer the rest of the workspace was
//! designed around: `flux-library` deliberately knows nothing about
//! queues, `flux-audio`'s [`Player`] knows nothing about files, and the
//! playback engine knows nothing about libraries. The [`Controller`]
//! here is the single place that wires them together, so the GUI and
//! TUI share identical behavior and neither reimplements glue logic.
//!
//! # State ownership (exactly one authority per concern)
//!
//! - [`Library`] owns the track collection and stable [`TrackId`]s.
//! - [`Player`] owns every playback *decision*: queue contents/order,
//!   current selection, playing/paused/stopped, repeat mode. The
//!   controller never second-guesses it; after every `Player` mutation
//!   it simply syncs the engine to whatever `Player` now says.
//! - The engine (`flux_audio::engine::EngineHandle`) owns decoding and
//!   the output device. It is a mechanism, not a model: the controller
//!   tells it what to load/play and consumes its events.
//! - The controller itself owns only coordination state: the volume
//!   (`Player` has none), which track it last asked the engine to load
//!   (a sync ledger, not a second selection), the storage backend
//!   (below), scan bookkeeping, and the last status/error strings for
//!   display.
//! - The track snapshot (`tracks()`) is a display cache refreshed
//!   whenever the library changes; it decides nothing.
//!
//! # Storage backends and library lifecycle
//!
//! The canonical self-contained library is the normal runtime backend
//! ([`StorageMode::Canonical`]). The Controller owns the validated
//! [`CanonicalLibrary`] and exposes a display snapshot plus stable IDs;
//! canonical files remain the authority. Create/open/switch build and validate
//! the candidate first, stop playback, load that library's queue, and never
//! carry track IDs or queue selections across libraries.
//!
//! Canonical mode:
//!
//! - resolves playback paths from the library root only;
//! - copies every import into the library (no external references);
//! - infers import metadata from filenames/folders and never reads tags;
//! - commits through the canonical transaction journal and validates paths;
//! - keeps artist/album renames identity-preserving;
//! - plans and executes copy-first single-track ownership transfers with
//!   explicit destination releases and journal-owned cleanup.
//!
//! Legacy (`Controller::new`, `switch_to_legacy_index`) and portable schema-v3
//! (`new_portable`, `switch_to_portable`) constructors remain for explicit
//! migration compatibility and existing tests. The GUI/TUI defaults and all
//! normal front-end setup/migration flows use canonical mode; the old backends
//! are not officially reachable as normal runtime destinations.
//!
//! Migration ([`plan_canonical_migration`] →
//! [`execute_canonical_migration`]) is explicit and read-only on the source.
//! It inventories a legacy index or portable schema-v3 root, builds and
//! validates a separate canonical destination, and activates only after the
//! caller confirms. Startup never migrates or overwrites old data.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use std::fmt;

use flux_audio::Player;
use flux_audio::decode::StreamInfo;
pub use flux_audio::engine::WAVEFORM_WINDOW;
use flux_audio::engine::{EngineEvent, EngineHandle};
use flux_core::{
    ArtistId, PlaybackState, Queue, QueueEntry, QueueEntryId, RepeatMode, Track, TrackId,
};
use flux_library::canonical::{
    CanonicalAlbum, CanonicalError, CanonicalLibrary, CanonicalPlaylist, TrackRating,
};
use flux_library::portable::{
    CATALOG_BACKUP_FILE_NAME, CATALOG_FILE_NAME, FLUX_DIR_NAME, OpenedStore, PortableRefreshReport,
    PortableStore, reconcile_portable_store, refresh_library_from_portable_store,
    sync_library_to_portable_store,
};
use flux_library::{
    Library, LibraryLock, LibraryLockError, PortableSyncReport, QueueStore, ScanReport,
};

pub mod import;

pub use import::{
    AppliedImportItem, CandidateStatus, ImportApplyReport, ImportCandidate, ImportCredit,
    ImportDecision, ImportItemState, ImportPlan, ImportProgress, ImportRequest, ImportStage,
    ImportStorage, PreparedImport, PreparedImportItem, apply_import_plan, prepare_import_files,
    prepare_import_plan,
};

pub use flux_library::canonical::{
    AlbumId, ArtistDeletionAlbum, ArtistDeletionDecision, ArtistDeletionDestination,
    ArtistDeletionPlan, ArtistDeletionReport, ArtistDeletionTrack, CanonicalImportedTrack,
    OwnershipDestination, OwnershipTransferPlan, OwnershipTransferReport, PlaylistId, ReleaseType,
};
pub use flux_library::canonical_migrate::{
    CanonicalMigrationError, CanonicalMigrationIssue, CanonicalMigrationPlan,
    CanonicalMigrationReport, CanonicalSourceFormat,
};
pub use flux_library::migrate::{MigrationError, MigrationPlan, MigrationReport, MigrationStage};
pub use flux_library::{
    Artist, Availability, MetadataOverrides, PortableStoreError, ReconcileOutcome, ReconcileReport,
    RecoveryInfo, RelPath, RelinkCandidates, RelinkError, StoreError, is_supported_audio_path,
};

/// Whether real device audio is available to this controller's engine.
///
/// A controller is fully usable for browsing/scanning unless the
/// backend is ready; only playback commands fail, with the reason
/// surfaced as the error message. This exists so a build without an
/// audio backend and a machine whose device failed to initialize are
/// both reported honestly and distinguishably instead of pretending to
/// play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStatus {
    /// The engine drives a working output (a real device, or a
    /// deliberately injected headless backend in tests).
    Ready,
    /// This binary was compiled without an audio-output backend.
    BuildWithoutBackend,
    /// An output backend exists in this build, but device
    /// initialization failed. The string is the user-facing reason.
    DeviceUnavailable(String),
}

impl AudioStatus {
    /// Whether playback commands can succeed.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// The user-facing reason playback is unavailable, if it is.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Ready => None,
            Self::BuildWithoutBackend => {
                Some("this build has no audio output backend (compiled without the `cpal` feature)")
            }
            Self::DeviceUnavailable(reason) => Some(reason),
        }
    }
}

/// Which persistence backend a [`Controller`] uses. Chosen by which
/// constructor built the controller; afterwards it changes only via
/// the explicit, validated `switch_to_*` methods — there is no
/// implicit switching, and a failure in one backend can never make
/// the controller write to the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageMode {
    /// The legacy version-1 JSON index (the long-standing default).
    Legacy,
    /// The legacy portable schema-v3 store (migration compatibility only in new sessions).
    Portable,
    /// The canonical self-contained Flux library.
    Canonical,
}

/// A complete track metadata edit from a user-facing form.
///
/// `None` means "no Flux override for this field" for album, track number,
/// and disc number (the embedded tag or fallback shows again). For the
/// title, `None` falls back to the source title but the display title is
/// still guaranteed non-empty. `artist_id` assigns one of the catalogue's
/// stable artists; `None` removes the relationship and leaves any legacy
/// text override in place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMetadataEdit {
    /// User-visible title override, or `None` to use source metadata.
    pub title: Option<String>,
    /// Stable artist identity, or `None` to remove the relationship.
    pub artist_id: Option<ArtistId>,
    /// Album title override.
    pub album: Option<String>,
    /// Track number override.
    pub track_number: Option<u32>,
    /// Disc number override.
    pub disc_number: Option<u32>,
}

/// One path that an import request could not add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportIssue {
    path: PathBuf,
    reason: String,
}

impl ImportIssue {
    /// The rejected source path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A user-facing reason.
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// The outcome of one [`Controller::import_files`] request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Newly indexed tracks.
    pub added: usize,
    /// Existing indexed tracks whose source tags were refreshed.
    pub updated: usize,
    /// Files already represented by the same path (no duplicate entry).
    pub deduplicated: usize,
    /// Copies made into the selected portable library.
    pub copied: Vec<PathBuf>,
    /// Sources that were rejected individually; successful paths still
    /// imported.
    pub issues: Vec<ImportIssue>,
}

impl ImportReport {
    /// Number of files considered (successful plus rejected).
    #[must_use]
    pub fn considered(&self) -> usize {
        self.added + self.updated + self.deduplicated + self.issues.len()
    }
}

/// Why a controller could not open its initial legacy library.
///
/// A failure here is fatal for that startup attempt: no empty library
/// is substituted, and no write is enabled without ownership.
#[derive(Debug)]
pub enum ControllerOpenError {
    /// Exclusive ownership of the legacy library could not be acquired.
    Lock(LibraryLockError),
    /// Canonical library opening/creation failed.
    Canonical(String),
}

impl fmt::Display for ControllerOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lock(err) => write!(f, "{err}"),
            Self::Canonical(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for ControllerOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Lock(err) => Some(err),
            Self::Canonical(_) => None,
        }
    }
}

impl From<LibraryLockError> for ControllerOpenError {
    fn from(err: LibraryLockError) -> Self {
        Self::Lock(err)
    }
}

/// Why a canonical library could not be opened or created.
#[derive(Debug)]
pub enum CanonicalOpenError {
    /// Supplied root is missing or not a directory.
    RootInvalid {
        /// Supplied root.
        root: PathBuf,
        /// Detail.
        reason: String,
    },
    /// Canonical validation failed.
    Store(CanonicalError),
    /// Exclusive ownership could not be acquired.
    Lock(LibraryLockError),
}

impl fmt::Display for CanonicalOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootInvalid { root, reason } => {
                write!(
                    f,
                    "canonical library root {} is not usable: {reason}",
                    root.display()
                )
            }
            Self::Store(err) => write!(f, "could not open the canonical library: {err}"),
            Self::Lock(err) => write!(f, "could not reserve the canonical library: {err}"),
        }
    }
}
impl std::error::Error for CanonicalOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Lock(err) => Some(err),
            Self::RootInvalid { .. } => None,
        }
    }
}
impl From<CanonicalError> for CanonicalOpenError {
    fn from(err: CanonicalError) -> Self {
        Self::Store(err)
    }
}
impl From<LibraryLockError> for CanonicalOpenError {
    fn from(err: LibraryLockError) -> Self {
        Self::Lock(err)
    }
}

/// Why [`Controller::new_portable`] could not open its library.
/// Construction fails as a whole: there is no controller, no empty-
/// library fallback, and no legacy fallback in these cases.
#[derive(Debug)]
pub enum PortableOpenError {
    /// The supplied library root does not exist, is not a directory,
    /// or cannot be canonicalized.
    RootInvalid {
        /// The library root the caller supplied.
        root: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The portable store failed to open or validate (including: no
    /// store exists at the root — opening never creates one — or
    /// neither catalogue validates).
    Store(PortableStoreError),
    /// Exclusive ownership of the portable library could not be acquired.
    Lock(LibraryLockError),
}

impl fmt::Display for PortableOpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootInvalid { root, reason } => write!(
                f,
                "portable library root {} is not usable: {reason}",
                root.display()
            ),
            Self::Store(err) => write!(f, "could not open the portable library: {err}"),
            Self::Lock(err) => write!(f, "could not reserve the portable library: {err}"),
        }
    }
}

impl std::error::Error for PortableOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Lock(err) => Some(err),
            Self::RootInvalid { .. } => None,
        }
    }
}

impl From<PortableStoreError> for PortableOpenError {
    fn from(err: PortableStoreError) -> Self {
        Self::Store(err)
    }
}

impl From<LibraryLockError> for PortableOpenError {
    fn from(err: LibraryLockError) -> Self {
        Self::Lock(err)
    }
}

/// The result of [`create_portable_library`]: a brand-new, verified
/// empty portable library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedLibrary {
    root: PathBuf,
    library_id: String,
}

impl CreatedLibrary {
    /// The library root (canonical) the store was created under.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The new library's stable identity.
    #[must_use]
    pub fn library_id(&self) -> &str {
        &self.library_id
    }
}

/// Why creating a portable library (or switching to a newly created
/// one) failed. Destination contents are never modified on any of
/// these paths: refusal is total, exactly as in migration.
#[derive(Debug)]
pub enum CreateLibraryError {
    /// The supplied root does not exist, is not a directory, or
    /// cannot be canonicalized. (Creation never invents directories
    /// from a typed path.)
    RootInvalid {
        /// The root the caller supplied.
        root: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The root already contains a valid portable library.
    AlreadyInitialized {
        /// The root the caller supplied.
        root: PathBuf,
    },
    /// The root contains a partial or invalid portable store (`.flux`
    /// leftovers without a valid catalogue). Every file is preserved;
    /// creation refuses automatic replacement or repair.
    PartialOrInvalid {
        /// The root the caller supplied.
        root: PathBuf,
        /// What was found (and why it does not validate).
        detail: String,
    },
    /// The storage layer failed while creating or reopening.
    Store(PortableStoreError),
    /// The created store did not verify as a fresh empty library
    /// when reopened. Whatever was created is preserved on disk.
    Verification {
        /// The root the caller supplied.
        root: PathBuf,
        /// What did not match a fresh empty library.
        detail: String,
    },
    /// Another Flux process owns the destination.
    Lock(LibraryLockError),
    /// Canonical creation/reopen failed.
    Canonical(CanonicalError),
    /// A scan is in progress on the controller that was asked to
    /// create-and-switch; nothing was created. (Only returned by
    /// [`Controller::switch_to_new_portable_library`].)
    ScanInProgress,
}

impl fmt::Display for CreateLibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootInvalid { root, reason } => {
                write!(f, "library root {} is not usable: {reason}", root.display())
            }
            Self::AlreadyInitialized { root } => write!(
                f,
                "{} already contains a Flux portable library; refusing to overwrite it",
                root.display()
            ),
            Self::PartialOrInvalid { root, detail } => write!(
                f,
                "{} contains a partial or invalid portable store ({}); \
                 refusing automatic replacement — its files were left untouched",
                root.display(),
                detail
            ),
            Self::Store(err) => write!(f, "could not create the portable library: {err}"),
            Self::Verification { root, detail } => write!(
                f,
                "the library created at {} did not verify as a fresh empty library: {detail}",
                root.display()
            ),
            Self::Lock(err) => write!(f, "{err}"),
            Self::Canonical(err) => write!(f, "could not create the canonical library: {err}"),
            Self::ScanInProgress => {
                write!(
                    f,
                    "a library scan is in progress; try again when it finishes"
                )
            }
        }
    }
}

impl std::error::Error for CreateLibraryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Lock(err) => Some(err),
            Self::Canonical(err) => Some(err),
            _ => None,
        }
    }
}

impl From<CanonicalError> for CreateLibraryError {
    fn from(err: CanonicalError) -> Self {
        Self::Canonical(err)
    }
}

impl From<PortableStoreError> for CreateLibraryError {
    fn from(err: PortableStoreError) -> Self {
        Self::Store(err)
    }
}

impl From<PortableOpenError> for CreateLibraryError {
    fn from(err: PortableOpenError) -> Self {
        match err {
            PortableOpenError::RootInvalid { root, reason } => Self::RootInvalid { root, reason },
            PortableOpenError::Store(err) => Self::Store(err),
            PortableOpenError::Lock(err) => Self::Lock(err),
        }
    }
}

/// Why switching the active library failed. On every variant the
/// previously active library, backend, queue, and playback state are
/// completely unchanged — the candidate was never installed.
#[derive(Debug)]
pub enum SwitchError {
    /// A scan is in progress (the library is owned by the scan
    /// thread); retry when it finishes.
    ScanInProgress,
    /// The portable candidate could not be opened or validated.
    Portable(PortableOpenError),
    /// The legacy candidate index could not be loaded or validated.
    Legacy(StoreError),
    /// Exclusive ownership of the legacy candidate could not be acquired.
    Lock(LibraryLockError),
    /// The canonical candidate could not be opened.
    Canonical(CanonicalOpenError),
}

impl fmt::Display for SwitchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ScanInProgress => {
                write!(
                    f,
                    "a library scan is in progress; try again when it finishes"
                )
            }
            Self::Portable(err) => write!(f, "could not switch libraries: {err}"),
            Self::Legacy(err) => write!(f, "could not switch libraries: {err}"),
            Self::Lock(err) => write!(f, "could not switch libraries: {err}"),
            Self::Canonical(err) => write!(f, "could not switch libraries: {err}"),
        }
    }
}

impl std::error::Error for SwitchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ScanInProgress => None,
            Self::Portable(err) => Some(err),
            Self::Legacy(err) => Some(err),
            Self::Lock(err) => Some(err),
            Self::Canonical(err) => Some(err),
        }
    }
}

impl From<CanonicalOpenError> for SwitchError {
    fn from(err: CanonicalOpenError) -> Self {
        Self::Canonical(err)
    }
}

impl From<PortableOpenError> for SwitchError {
    fn from(err: PortableOpenError) -> Self {
        Self::Portable(err)
    }
}

/// The active persistence backend (the storage counterpart of
/// [`StorageMode`], with the location it persists to).
enum StorageBackend {
    Legacy {
        index_path: Option<PathBuf>,
    },
    /// `root` is canonical: library paths and scan paths are built
    /// from it so path identity is spelling-consistent.
    Portable {
        root: PathBuf,
    },
    /// Canonical self-contained library. All normal reads/writes go through
    /// the canonical store held beside the display snapshot.
    Canonical {
        root: PathBuf,
    },
}

/// Message used wherever a write is refused because the portable
/// store is in the recovered read-only state.
const PORTABLE_RECOVERY_READ_ONLY: &str = "The portable library was recovered from its backup \
     catalogue and is read-only. Acknowledge the recovery to enable saving and scanning.";

/// Message used when the portable store object itself is gone (its
/// scan thread vanished and reopening failed): saves cannot pretend
/// to succeed.
const PORTABLE_STORE_UNAVAILABLE: &str = "The portable library store is unavailable because it \
     could not be reopened after the failed scan; changes cannot be saved.";

/// Message used when an existing legacy index could not be read at
/// startup. Scanning and saving are blocked until the user explicitly
/// preserves and replaces it, so a transient read failure or a
/// repairable corrupt file is never overwritten by an empty library.
const LEGACY_INDEX_WRITE_PROTECTED: &str = "The saved library index could not be read. Flux will \
     not overwrite it; scanning and saving are disabled until you explicitly preserve and replace it.";

/// Defensive message for a persistence backend without its ownership
/// lease. Constructors and switches always pair the two; if this ever
/// fires, refusing the write is safer than proceeding unprotected.
const LIBRARY_NOT_OWNED: &str = "This Flux window does not own the active library, so it will not \
     write to it. Close and reopen the library.";

/// Notifications produced by [`Controller::poll`]. Front-ends treat
/// these as "re-read controller state now" signals; `Error`/`Status`
/// also carry the message (mirrored in
/// [`last_error`](Controller::last_error) /
/// [`last_status`](Controller::last_status)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerEvent {
    /// The track collection changed (scan finished, index loaded…).
    LibraryChanged,
    /// The playback queue's entries, order, or current entry changed.
    QueueChanged,
    /// Playback state, selection, or stream info changed.
    PlaybackChanged,
    /// An informational message for the user.
    Status(String),
    /// A user-facing error. Playback state has already been made
    /// consistent (stopped) by the time this is reported.
    Error(String),
}

/// How the scan worker persists the scanned library — the active
/// backend travels with the work so a scan can only ever write to
/// the backend the controller was constructed with.
enum ScanPersistence {
    /// No persistence (legacy mode without an index path).
    None,
    /// Save the version-1 index at this path.
    Legacy(PathBuf),
    /// Sync into and commit this portable store. The store is
    /// `Send` and single-writer: the worker owns it for the scan's
    /// duration exactly as it owns the `Library`. (Boxed: the store
    /// is a large value to move through the channel types, and it
    /// only changes hands at scan start/finish.)
    Portable(Box<PortableStore>),
    /// Portable mode, but the store object was lost with a previous
    /// scan thread and could not be reopened; saving must fail
    /// visibly instead of pretending to persist.
    PortableUnavailable,
}

/// The payload handed to the scan worker thread: everything it owns
/// while scanning, recoverable whole from a failed handoff.
struct ScanWork {
    library: Library,
    roots: Vec<PathBuf>,
    persistence: ScanPersistence,
    /// Keep the OS lease alive even if the controller is dropped while
    /// this worker is still scanning or publishing.
    lease: Option<Arc<LibraryLock>>,
}

/// The result of one background scan, delivered to the controller.
struct ScanOutcome {
    library: Library,
    report: ScanReport,
    /// The persistence backend, handed back (the portable store
    /// returns to controller ownership here, including any pending
    /// uncommitted changes after a failed commit).
    persistence: ScanPersistence,
    /// `Some` when the scan thread also tried to save.
    saved: Option<Result<(), String>>,
    /// The portable sync outcome, when the portable save ran.
    sync_report: Option<PortableSyncReport>,
}

/// Creates the engine a Flux application should use, together with its
/// honest [`AudioStatus`].
///
/// With the `cpal` feature, this opens the default output device; a
/// device/initialization failure returns a headless null engine plus
/// [`AudioStatus::DeviceUnavailable`]. A build without the feature
/// returns [`AudioStatus::BuildWithoutBackend`]. In both cases the
/// application can still browse while telling the user exactly why
/// playback will not work. The null engine is never presented as a
/// real device — playback commands check the status first.
#[must_use]
pub fn default_engine() -> (EngineHandle, AudioStatus) {
    #[cfg(feature = "cpal")]
    {
        match EngineHandle::spawn_cpal_default() {
            Ok(engine) => return (engine, AudioStatus::Ready),
            Err(err) => {
                let (engine, _control) = EngineHandle::spawn_null(48_000, 2);
                return (
                    engine,
                    AudioStatus::DeviceUnavailable(format!("audio output unavailable: {err}")),
                );
            }
        }
    }
    #[allow(unreachable_code)]
    {
        let (engine, _control) = EngineHandle::spawn_null(48_000, 2);
        (engine, AudioStatus::BuildWithoutBackend)
    }
}

/// The default library-index path: `library-index.json` inside the
/// per-user data directory resolved by `flux-platform` (for example
/// `~/.local/share/flux/library-index.json` on Linux). `None` when no
/// data directory can be resolved, in which case the application
/// simply runs without persistence.
#[must_use]
pub fn default_index_path() -> Option<PathBuf> {
    flux_platform::app_dirs("flux").map(|dirs| dirs.data_dir().join("library-index.json"))
}

/// Default canonical library root inside the per-user data directory.
#[must_use]
pub fn default_canonical_root() -> Option<PathBuf> {
    flux_platform::app_dirs("flux").map(|dirs| dirs.data_dir().join("FluxLibrary"))
}

/// Builds a migration plan from the legacy index at `legacy_index`
/// to a new portable library at `destination_root`, without writing
/// anything. This is an explicit, caller-requested operation: no
/// constructor, scan, or save in this crate ever invokes migration
/// on its own, and executing a plan never changes any running
/// controller's backend (constructing a portable controller for the
/// result is a separate, explicit step —
/// [`Controller::new_portable`]).
///
/// # Errors
///
/// Every S3 refusal is preserved verbatim as a [`MigrationError`]:
/// unreadable/invalid legacy index, unusable destination root, or a
/// destination that already contains an initialized or partial
/// portable store.
/// Plans a canonical migration from either a legacy index file or a portable
/// schema-v3 library directory. The source is read-only and the caller must
/// choose a separate, still-absent destination root.
pub fn plan_canonical_migration(
    source: &Path,
    destination: &Path,
) -> Result<CanonicalMigrationPlan, CanonicalMigrationError> {
    if source.is_file() {
        return flux_library::canonical_migrate::plan_legacy_index(source, destination);
    }
    if source.is_dir() {
        if source.join("catalog.flux").is_file() {
            return Err(CanonicalMigrationError::UnsupportedSource(
                "the source is already a canonical library".to_owned(),
            ));
        }
        if source.join(".flux").join("catalog.flux").is_file() {
            return flux_library::canonical_migrate::plan_portable_v3(source, destination);
        }
        return Err(CanonicalMigrationError::UnsupportedSource(
            "the directory contains neither a portable schema-v3 catalogue nor a canonical library"
                .to_owned(),
        ));
    }
    Err(CanonicalMigrationError::UnsupportedSource(format!(
        "{} does not exist or is neither a legacy index file nor a library directory",
        source.display()
    )))
}

/// Executes a previously planned canonical migration after validation and
/// staging. The source remains untouched.
pub fn execute_canonical_migration(
    plan: &CanonicalMigrationPlan,
) -> Result<CanonicalMigrationReport, CanonicalMigrationError> {
    plan.execute()
}

pub fn plan_legacy_migration(
    legacy_index: &Path,
    destination_root: &Path,
) -> Result<MigrationPlan, MigrationError> {
    flux_library::migrate::plan_migration(legacy_index, destination_root)
}

/// Plans and executes a legacy-index migration in one explicit call
/// (see [`plan_legacy_migration`]). Returns the verified
/// [`MigrationReport`]; on failure the legacy index is untouched and
/// any partial destination artifacts are preserved, with the
/// [`MigrationError`] naming the stage and whether a destination
/// store was created.
pub fn migrate_legacy_library(
    legacy_index: &Path,
    destination_root: &Path,
) -> Result<MigrationReport, MigrationError> {
    flux_library::migrate::migrate_legacy_index(legacy_index, destination_root)
}

/// Executes a previously built [`MigrationPlan`] (see
/// [`plan_legacy_migration`]) — the step a UI runs only after the
/// user has reviewed the plan and explicitly confirmed. Dropping a
/// plan instead is a complete cancellation: planning wrote nothing.
/// Executing never switches any controller's active library;
/// activating the result is a separate
/// [`Controller::switch_to_portable`] call.
pub fn execute_legacy_migration(plan: &MigrationPlan) -> Result<MigrationReport, MigrationError> {
    flux_library::migrate::execute_migration(plan)
}

/// Creates a new, empty portable library at the explicitly supplied
/// `root` (which must already exist as a directory), publishes its
/// initial catalogue through the portable store's own creation API,
/// and verifies the result by reopening it.
///
/// Refusals mirror migration's destination policy and never modify
/// anything: a valid existing library is
/// [`CreateLibraryError::AlreadyInitialized`]; `.flux` leftovers that
/// do not validate (partial or damaged store) are
/// [`CreateLibraryError::PartialOrInvalid`] with every file
/// preserved. No controller is involved and nothing is switched —
/// pair with [`Controller::switch_to_portable`] (or the one-call
/// [`Controller::switch_to_new_portable_library`]) to start using it.
/// Creates and verifies an empty canonical library at `root`.
///
/// `root` may be absent or empty directories. Existing non-empty data is
/// refused; the source is never repaired or overwritten automatically.
pub fn create_canonical_library(root: &Path) -> Result<CreatedLibrary, CreateLibraryError> {
    let created = CanonicalLibrary::create(root)?;
    let library_id = created.library_id().to_owned();
    let root = created.root().to_path_buf();
    drop(created);
    let reopened = CanonicalLibrary::open(&root)?;
    if !reopened.artists().next().is_none()
        || !reopened.albums().next().is_none()
        || !reopened.tracks().next().is_none()
        || reopened.library_id() != library_id
    {
        return Err(CreateLibraryError::Verification {
            root,
            detail: "the created canonical library did not reopen as the same empty library"
                .to_owned(),
        });
    }
    Ok(CreatedLibrary { root, library_id })
}

/// Creates and verifies an empty portable library at `root`.
pub fn create_portable_library(root: &Path) -> Result<CreatedLibrary, CreateLibraryError> {
    let root_invalid = |reason: String| CreateLibraryError::RootInvalid {
        root: root.to_owned(),
        reason,
    };
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(root_invalid("it exists but is not a directory".to_owned())),
        Err(err) => {
            return Err(root_invalid(format!(
                "it does not exist or cannot be inspected: {err}"
            )));
        }
    }
    let canonical = std::fs::canonicalize(root)
        .map_err(|err| root_invalid(format!("it cannot be canonicalized: {err}")))?;

    check_creatable_destination(root, &canonical)?;
    let _lease =
        LibraryLock::acquire_portable_root(&canonical).map_err(CreateLibraryError::Lock)?;
    // The first pass gives precise destination errors; this second pass
    // is authoritative under exclusive ownership.
    check_creatable_destination(root, &canonical)?;

    let created = PortableStore::create(&canonical)?;
    let library_id = created.library_id().to_owned();
    drop(created);

    // Success means a *verified* fresh library: reopen it exactly as
    // a future open would see it.
    let opened = PortableStore::open(&canonical)?;
    let verification = |detail: String| CreateLibraryError::Verification {
        root: root.to_owned(),
        detail,
    };
    if opened.recovery.is_some() {
        return Err(verification(
            "the new store opens only through backup recovery".to_owned(),
        ));
    }
    if !opened.store.is_empty()
        || opened.store.generation() != 1
        || opened.store.next_track_id() != 1
    {
        return Err(verification(format!(
            "reopened store has {} track(s), generation {}, next ID {} (expected 0 / 1 / 1)",
            opened.store.len(),
            opened.store.generation(),
            opened.store.next_track_id()
        )));
    }
    if opened.store.library_id() != library_id {
        return Err(verification(
            "the reopened store has a different library identity".to_owned(),
        ));
    }

    Ok(CreatedLibrary {
        root: canonical,
        library_id,
    })
}

/// The destination gate for creation — the same policy S3 migration
/// applies to its destination, expressed through the public store
/// API plus one directory listing: an existing valid library is
/// "already initialized"; any other pre-existing `.flux` content is
/// partial/invalid and must be preserved, never repaired over. A
/// completely empty `.flux/` directory holds no data and does not
/// block creation.
fn check_creatable_destination(
    root: &Path,
    canonical_root: &Path,
) -> Result<(), CreateLibraryError> {
    let partial = |detail: String| CreateLibraryError::PartialOrInvalid {
        root: root.to_owned(),
        detail,
    };
    let flux_dir = canonical_root.join(FLUX_DIR_NAME);
    match std::fs::symlink_metadata(&flux_dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(partial(format!(
            "the .flux storage directory cannot be inspected: {err}"
        ))),
        Ok(metadata) if !metadata.is_dir() => Err(partial(
            ".flux exists but is not a directory (or is a symlink); \
             refusing to treat it as storage"
                .to_owned(),
        )),
        Ok(_) => {
            if flux_dir.join(CATALOG_FILE_NAME).exists()
                || flux_dir.join(CATALOG_BACKUP_FILE_NAME).exists()
            {
                match PortableStore::open(canonical_root) {
                    Ok(_) => Err(CreateLibraryError::AlreadyInitialized {
                        root: root.to_owned(),
                    }),
                    Err(err) => Err(partial(format!("the existing store does not open: {err}"))),
                }
            } else {
                match std::fs::read_dir(&flux_dir) {
                    Ok(mut entries) => {
                        if entries.next().is_some() {
                            Err(partial(
                                ".flux contains files but no catalogue — the remains of \
                                 an interrupted store creation or a damaged store"
                                    .to_owned(),
                            ))
                        } else {
                            Ok(())
                        }
                    }
                    Err(err) => Err(partial(format!(
                        "the .flux storage directory cannot be listed: {err}"
                    ))),
                }
            }
        }
    }
}

/// Whether the engine can actually decode this track's file, by
/// extension (the contents are validated when the track is opened).
/// Front-ends use this to mark indexed-but-unplayable tracks instead
/// of letting users discover it by failure.
#[must_use]
pub fn is_track_playable(track: &Track) -> bool {
    flux_audio::decode::is_playable(track.source().path())
}

/// Formats a duration as `m:ss` (or `h:mm:ss`) for display.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The Flux application controller. See the crate-level docs for the
/// state-ownership contract.
pub struct Controller {
    /// The library, except while a scan thread owns it (`None` then).
    library: Option<Library>,
    /// Display/lookup snapshot of the library, refreshed on change.
    tracks: Vec<(TrackId, Track)>,
    player: Player,
    engine: EngineHandle,
    audio: AudioStatus,
    /// Sync ledger: the track this controller last asked the engine to
    /// load. Not a selection — `Player` owns that; this only decides
    /// "resume vs. load" when syncing.
    engine_loaded: Option<TrackId>,
    /// The load session of `engine_loaded`. Every explicit load gets a
    /// new monotonic session from the engine, so delayed events from an
    /// older attempt (even for the same track) cannot affect the
    /// current one.
    engine_session: Option<u64>,
    /// Stream facts for `engine_loaded`, once the engine confirms it.
    stream_info: Option<StreamInfo>,
    volume: f32,
    /// The persistence backend, fixed at construction.
    storage: StorageBackend,
    /// The queue file for the active library, when persistence is
    /// available. The queue is separate from the catalogue and tags.
    queue_store: Option<QueueStore>,
    /// The existing queue file was malformed or unreadable. Its bytes are
    /// preserved and this session never writes over it.
    queue_write_protected: bool,
    /// True while the queue is still the library-derived default and may
    /// receive newly scanned tracks. Cleared permanently by structural
    /// queue edits.
    queue_auto_library: bool,
    /// Exclusive OS ownership of the active library's catalogue. Held
    /// for the controller's lifetime and cloned into any scan worker so
    /// a dying UI cannot release the lock while background code writes.
    library_lock: Option<Arc<LibraryLock>>,
    /// An existing legacy index could not be loaded at startup. Writes
    /// to that path (scan saves and explicit saves) are refused until
    /// `replace_unreadable_legacy_index` preserves it and clears this.
    legacy_index_write_protected: bool,
    /// The portable store in portable mode — `None` in legacy/canonical mode,
    /// while a scan thread owns it, or if it was lost with a scan
    /// thread and could not be reopened (saves then fail visibly).
    portable_store: Option<PortableStore>,
    /// Canonical store in canonical mode. This is the runtime authority for
    /// entities and paths; `library`/`tracks` are presentation snapshots.
    canonical_store: Option<CanonicalLibrary>,
    /// Pending backup-recovery details in portable mode, until
    /// explicitly acknowledged.
    portable_recovery: Option<RecoveryInfo>,
    /// Directories scanned this session, for `refresh`.
    roots: Vec<PathBuf>,
    scan_rx: Option<Receiver<ScanOutcome>>,
    outbox: Vec<ControllerEvent>,
    last_error: Option<String>,
    last_status: Option<String>,
}

/// Initial state for a controller, assembled by the fallible
/// constructors once the candidate library and its ownership lease are
/// known.
struct LibrarySeed {
    library: Library,
    storage: StorageBackend,
    library_lock: Option<Arc<LibraryLock>>,
    portable_store: Option<PortableStore>,
    canonical_store: Option<CanonicalLibrary>,
    portable_recovery: Option<RecoveryInfo>,
    roots: Vec<PathBuf>,
    queue: Queue,
    queue_store: Option<QueueStore>,
    queue_write_protected: bool,
    queue_auto_library: bool,
}

impl Controller {
    /// Creates a controller around an already-spawned engine.
    ///
    /// If `index_path` names an existing file, the library index is
    /// loaded from it (this reads and parses a file, so call this
    /// before starting the UI event loop). A missing file simply
    /// starts an empty library and remains writable. An
    /// unreadable/corrupt existing file starts an empty library, records
    /// a user-facing error, and enters legacy write-protection: scans
    /// and saves are refused until
    /// [`replace_unreadable_legacy_index`](Self::replace_unreadable_legacy_index)
    /// preserves the old file. The library's persisted queue is loaded;
    /// when none exists, a default whole-library queue is derived. A
    /// malformed queue file is reported and preserved, and queue writes
    /// are disabled for that session.
    ///
    /// # Errors
    ///
    /// [`ControllerOpenError::Lock`] when another Flux process owns the
    /// index (or the exclusive lock cannot be created). No library is
    /// loaded, no empty library is substituted, and no write occurs.
    pub fn new(
        engine: EngineHandle,
        audio: AudioStatus,
        index_path: Option<PathBuf>,
    ) -> Result<Self, ControllerOpenError> {
        // Acquire ownership before reading: another Flux process may be
        // publishing this index right now.
        let library_lock = match &index_path {
            Some(path) => Some(Arc::new(LibraryLock::acquire_legacy_index(path)?)),
            None => None,
        };
        let (library, last_error, write_protected) = match &index_path {
            Some(path) if path.exists() => match Library::load_from_path(path) {
                Ok(library) => (library, None, false),
                Err(err) => (
                    Library::new(),
                    Some(format!(
                        "Could not load the saved library index ({}): {err}. \
                         The file is preserved; scanning and saving are disabled \
                         until you explicitly replace it.",
                        path.display()
                    )),
                    true,
                ),
            },
            _ => (Library::new(), None, false),
        };
        let queue_store = library_lock
            .as_ref()
            .map(|lock| QueueStore::new(legacy_queue_path(lock.identity())));
        let (queue, queue_auto_library, queue_write_protected, queue_error) =
            load_queue(queue_store.as_ref(), &library);
        let mut controller = Self::assemble(
            engine,
            audio,
            LibrarySeed {
                library,
                storage: StorageBackend::Legacy { index_path },
                library_lock,
                portable_store: None,
                canonical_store: None,
                portable_recovery: None,
                roots: Vec::new(),
                queue,
                queue_store,
                queue_write_protected,
                queue_auto_library,
            },
        );
        controller.legacy_index_write_protected = write_protected;
        if let Some(error) = last_error {
            controller.report_error(error);
        }
        if let Some(error) = queue_error {
            controller.report_error(error);
        }
        Ok(controller)
    }

    /// Creates a controller whose persistence backend is the portable
    /// `.flux` store at `library_root` (supplied explicitly by the
    /// caller — never guessed). The in-memory library is
    /// reconstructed from the store: stable IDs, the next-ID counter
    /// with its retired gaps, effective metadata, and missing
    /// (unavailable) records are all preserved.
    ///
    /// Unlike the legacy constructor this is strict and fallible: a
    /// nonexistent or non-directory root, a root with no portable
    /// store, or a store whose catalogues do not validate all fail
    /// with [`PortableOpenError`] — no controller is produced, no
    /// empty library is substituted, no legacy index is consulted,
    /// and no store is created. (The engine is dropped on the error
    /// path, which shuts it down cleanly.)
    ///
    /// If the store opens from its backup catalogue, the controller
    /// is usable for browsing/playback but read-only for persistence
    /// (see [`portable_recovery`](Self::portable_recovery)) until
    /// [`acknowledge_portable_recovery`](Self::acknowledge_portable_recovery)
    /// is called explicitly; that state is also reported in
    /// [`last_error`](Self::last_error) at construction.
    pub fn new_portable(
        engine: EngineHandle,
        audio: AudioStatus,
        library_root: &Path,
    ) -> Result<Self, PortableOpenError> {
        let (root, lock, opened, library, skipped) = open_portable_candidate(library_root)?;
        let recovery = opened.recovery;
        let queue_store = Some(QueueStore::new(portable_queue_path(&root)));
        let (queue, queue_auto_library, queue_write_protected, queue_error) =
            load_queue(queue_store.as_ref(), &library);

        let mut controller = Self::assemble(
            engine,
            audio,
            LibrarySeed {
                library,
                storage: StorageBackend::Portable { root: root.clone() },
                library_lock: Some(Arc::new(lock)),
                portable_store: Some(opened.store),
                canonical_store: None,
                portable_recovery: recovery,
                roots: vec![root],
                queue,
                queue_store,
                queue_write_protected,
                queue_auto_library,
            },
        );
        if skipped > 0 {
            controller.report_status(skipped_unrepresentable_message(skipped));
        }
        if controller.portable_recovery.is_some() {
            controller.report_error(PORTABLE_RECOVERY_READ_ONLY.to_owned());
        }
        if let Some(error) = queue_error {
            controller.report_error(error);
        }
        Ok(controller)
    }

    /// Opens an existing canonical library as the runtime authority.
    pub fn new_canonical(
        engine: EngineHandle,
        audio: AudioStatus,
        library_root: &Path,
    ) -> Result<Self, CanonicalOpenError> {
        let root = canonical_library_root(library_root)?;
        let lock = LibraryLock::acquire_portable_root(&root)?;
        let store = CanonicalLibrary::open(&root)?;
        let library = store.runtime_library();
        let queue_store = Some(QueueStore::new(canonical_queue_path(&root)));
        let (queue, queue_auto_library, queue_write_protected, queue_error) =
            load_queue(queue_store.as_ref(), &library);
        let mut controller = Self::assemble(
            engine,
            audio,
            LibrarySeed {
                library,
                storage: StorageBackend::Canonical { root: root.clone() },
                library_lock: Some(Arc::new(lock)),
                portable_store: None,
                canonical_store: Some(store),
                portable_recovery: None,
                roots: Vec::new(),
                queue,
                queue_store,
                queue_write_protected,
                queue_auto_library,
            },
        );
        if let Some(error) = queue_error {
            controller.report_error(error);
        }
        controller.report_status(format!("Opened canonical library at {}.", root.display()));
        Ok(controller)
    }

    /// Opens the default canonical library, creating an empty one on first
    /// run. Existing legacy/portable data is never migrated automatically.
    pub fn open_default() -> Result<Self, ControllerOpenError> {
        let root = default_canonical_root().ok_or_else(|| {
            ControllerOpenError::Canonical(
                "no per-user data directory is available for a canonical library".to_owned(),
            )
        })?;
        if !root.exists() {
            if let Some(parent) = root.parent() {
                std::fs::create_dir_all(parent).map_err(|err| {
                    ControllerOpenError::Canonical(format!(
                        "could not create {}: {err}",
                        parent.display()
                    ))
                })?;
            }
            create_canonical_library(&root).map_err(|err| {
                ControllerOpenError::Canonical(format!(
                    "could not create the default canonical library: {err}"
                ))
            })?;
        }
        let (engine, audio) = default_engine();
        Self::new_canonical(engine, audio, &root)
            .map_err(|err| ControllerOpenError::Canonical(err.to_string()))
    }

    /// Shared construction once the initial library and backend are
    /// known: snapshot, playback model, queue, and engine volume are
    /// set up identically for both backends.
    fn assemble(engine: EngineHandle, audio: AudioStatus, seed: LibrarySeed) -> Self {
        let LibrarySeed {
            library,
            storage,
            library_lock,
            portable_store,
            canonical_store,
            portable_recovery,
            roots,
            queue,
            queue_store,
            queue_write_protected,
            queue_auto_library,
        } = seed;
        let mut player = Player::new();
        *player.queue_mut() = queue;
        let controller = Self {
            tracks: snapshot(&library),
            library: Some(library),
            player,
            engine,
            audio,
            engine_loaded: None,
            engine_session: None,
            stream_info: None,
            volume: 1.0,
            storage,
            queue_store,
            queue_write_protected,
            queue_auto_library,
            library_lock,
            legacy_index_write_protected: false,
            portable_store,
            canonical_store,
            portable_recovery,
            roots,
            scan_rx: None,
            outbox: Vec::new(),
            last_error: None,
            last_status: None,
        };
        controller.engine.set_volume(controller.volume);
        controller
    }

    // ------------------------------------------------------ accessors ---

    /// All tracks in library order (the display snapshot).
    #[must_use]
    pub fn tracks(&self) -> &[(TrackId, Track)] {
        &self.tracks
    }

    /// Looks up one track by ID.
    #[must_use]
    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks
            .iter()
            .find(|(t, _)| *t == id)
            .map(|(_, track)| track)
    }

    /// Tracks matching a case-insensitive substring search over title,
    /// artist, and album (same rule as `Library::search`; an empty
    /// query matches everything).
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<(TrackId, &Track)> {
        let needle = query.trim().to_lowercase();
        self.tracks
            .iter()
            .filter(|(_, track)| {
                needle.is_empty()
                    || track.title().to_lowercase().contains(&needle)
                    || track
                        .artist()
                        .is_some_and(|a| a.to_lowercase().contains(&needle))
                    || track
                        .album()
                        .is_some_and(|a| a.to_lowercase().contains(&needle))
            })
            .map(|(id, track)| (*id, track))
            .collect()
    }

    /// All stable artist identities in the active portable catalogue,
    /// ascending by ID. Empty in legacy mode or when no portable store is
    /// available. Duplicate display names are intentionally preserved.
    #[must_use]
    pub fn artists(&self) -> Vec<Artist> {
        if let Some(store) = self.canonical_store.as_ref() {
            return store
                .artists()
                .map(|artist| Artist::new(artist.id, artist.name.clone(), artist.aliases.clone()))
                .collect();
        }
        self.portable_store
            .as_ref()
            .map(|store| store.artists().cloned().collect())
            .unwrap_or_default()
    }

    /// Looks up one stable artist identity.
    #[must_use]
    pub fn artist(&self, id: ArtistId) -> Option<Artist> {
        if let Some(store) = self.canonical_store.as_ref() {
            return store
                .artist(id)
                .map(|artist| Artist::new(artist.id, artist.name.clone(), artist.aliases.clone()));
        }
        self.portable_store
            .as_ref()
            .and_then(|store| store.artist(id).cloned())
    }

    /// The stable artist relationship for a track, when one is assigned.
    #[must_use]
    pub fn track_artist_id(&self, id: TrackId) -> Option<ArtistId> {
        if let Some(store) = self.canonical_store.as_ref() {
            return store.track(id).map(|track| track.owner_artist_id);
        }
        self.portable_store
            .as_ref()
            .and_then(|store| store.get(id))
            .and_then(|track| track.artist_id())
    }

    /// Every credited artist for a canonical track, owner first.
    #[must_use]
    pub fn track_credits(&self, id: TrackId) -> Vec<ArtistId> {
        self.canonical_store
            .as_ref()
            .and_then(|store| store.track_credits(id))
            .map(ToOwned::to_owned)
            .unwrap_or_default()
    }

    /// Canonical albums in stable ID order.
    #[must_use]
    pub fn albums(&self) -> Vec<CanonicalAlbum> {
        self.canonical_store
            .as_ref()
            .map(|store| store.albums().cloned().collect())
            .unwrap_or_default()
    }

    /// The canonical album owning a track.
    #[must_use]
    pub fn track_album_id(&self, id: TrackId) -> Option<AlbumId> {
        self.canonical_store
            .as_ref()
            .and_then(|store| store.track(id))
            .map(|track| track.album_id)
    }

    /// Track IDs in canonical album order.
    #[must_use]
    pub fn album_tracks(&self, id: AlbumId) -> Vec<TrackId> {
        self.canonical_store
            .as_ref()
            .and_then(|store| store.album(id))
            .map(|album| album.track_order.clone())
            .unwrap_or_default()
    }

    /// Canonical playlists in stable ID order.
    #[must_use]
    pub fn playlists(&self) -> Vec<CanonicalPlaylist> {
        self.canonical_store
            .as_ref()
            .map(|store| store.playlists().cloned().collect())
            .unwrap_or_default()
    }

    /// The canonical rating/favorite value for a track.
    #[must_use]
    pub fn rating(&self, id: TrackId) -> Option<TrackRating> {
        self.canonical_store
            .as_ref()
            .and_then(|store| store.rating(id))
    }

    /// Stores a rating/favorite value through the canonical engine.
    pub fn set_rating(
        &mut self,
        id: TrackId,
        rating: Option<u8>,
        favorite: bool,
    ) -> Result<(), String> {
        let Some(store) = self.canonical_store.as_mut() else {
            return Err("Ratings require a canonical library.".to_owned());
        };
        store
            .set_rating(id, rating, favorite)
            .map_err(|err| format!("Could not save rating: {err}"))?;
        Ok(())
    }

    /// Creates or updates a canonical playlist.
    pub fn set_playlist(
        &mut self,
        id: Option<PlaylistId>,
        name: &str,
        track_ids: Vec<TrackId>,
    ) -> Result<PlaylistId, String> {
        let Some(store) = self.canonical_store.as_mut() else {
            return Err("Playlists require a canonical library.".to_owned());
        };
        store
            .set_playlist(id, name, track_ids)
            .map_err(|err| format!("Could not save playlist: {err}"))
    }

    /// Stores optional managed LRC text for a track.
    pub fn set_lyrics(&mut self, id: TrackId, lyrics: Option<&str>) -> Result<(), String> {
        let Some(store) = self.canonical_store.as_mut() else {
            return Err("Lyrics require a canonical library.".to_owned());
        };
        store
            .set_lyrics(id, lyrics)
            .map_err(|err| format!("Could not save lyrics: {err}"))
    }

    /// Reads a canonical track's managed lyrics text, when present.
    #[must_use]
    pub fn track_lyrics(&self, id: TrackId) -> Option<String> {
        let path = self.canonical_store.as_ref()?.track_lyrics_path(id)?;
        std::fs::read_to_string(path).ok()
    }

    /// Last-known availability of a catalogue track. `None` in legacy mode
    /// or for unknown IDs.
    #[must_use]
    pub fn track_availability(&self, id: TrackId) -> Option<Availability> {
        if let Some(store) = self.canonical_store.as_ref() {
            return store.track(id).map(|_| {
                if store.track_available(id) {
                    Availability::Present
                } else {
                    Availability::Missing
                }
            });
        }
        self.portable_store
            .as_ref()
            .and_then(|store| store.get(id))
            .map(|track| track.availability())
    }

    /// The playback model's state.
    #[must_use]
    pub fn player_state(&self) -> PlaybackState {
        self.player.state()
    }

    /// The queue's entries, in playback order. Duplicate tracks appear as
    /// distinct entries with their own identities.
    #[must_use]
    pub fn queue_entries(&self) -> &[QueueEntry] {
        self.player.queue().entries()
    }

    /// The number of entries in the queue.
    #[must_use]
    pub fn queue_len(&self) -> usize {
        self.player.queue().len()
    }

    /// The stable identity of the current queue entry.
    #[must_use]
    pub fn current_queue_entry_id(&self) -> Option<QueueEntryId> {
        self.player.current_entry_id()
    }

    /// The current queue entry.
    #[must_use]
    pub fn current_queue_entry(&self) -> Option<QueueEntry> {
        self.player.queue().current_entry()
    }

    /// The position of an entry in the queue.
    #[must_use]
    pub fn queue_entry_index(&self, entry: QueueEntryId) -> Option<usize> {
        self.player.queue().entry_index(entry)
    }

    /// The currently selected track's ID, if any.
    #[must_use]
    pub fn current_id(&self) -> Option<TrackId> {
        self.player.current_track()
    }

    /// The currently selected track, if any.
    #[must_use]
    pub fn current_track(&self) -> Option<&Track> {
        self.current_id().and_then(|id| self.track(id))
    }

    /// The active repeat mode.
    #[must_use]
    pub fn repeat_mode(&self) -> RepeatMode {
        self.player.repeat_mode()
    }

    /// Elapsed playback position of the loaded track (from the engine;
    /// zero when nothing is loaded).
    #[must_use]
    pub fn position(&self) -> Duration {
        self.engine.position()
    }

    /// The latest real output samples for the oscilloscope.
    ///
    /// These are post-gain, mono device-rate samples captured in the
    /// audio callback through a non-blocking tap. The UI should call this
    /// from its paint timer, never from the audio thread.
    #[must_use]
    pub fn waveform(&self) -> [f32; WAVEFORM_WINDOW] {
        self.engine.waveform()
    }

    /// The known duration of the current track, preferring the decoded
    /// stream's facts over the catalogue metadata.
    #[must_use]
    pub fn current_duration(&self) -> Option<Duration> {
        self.stream_info
            .and_then(|info| info.duration)
            .or_else(|| self.current_track().and_then(Track::duration))
    }

    /// Seeks the current track to `position`, clamped to its known
    /// duration.
    ///
    /// The engine reopens and pre-rolls its decoder off the UI thread;
    /// playback position is updated immediately and playback state is
    /// preserved. A no-op with a clear error when no track or duration is
    /// known. Returns the effective clamped position.
    pub fn seek(&mut self, position: Duration) -> Result<Duration, String> {
        let Some(id) = self.current_id() else {
            let message = "Nothing is selected to seek.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let Some(duration) = self.current_duration() else {
            let message = "This track has no known duration to seek within.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        if duration.is_zero() {
            let message = "This track has zero duration.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        }

        let target = position.min(duration);
        let had_loaded_track = self.engine_loaded == Some(id);
        if !had_loaded_track && let Err(err) = self.load_into_engine(id, false) {
            self.report_error(err.clone());
            return Err(err);
        }
        let session = self.engine.seek(target);
        self.engine_session = Some(session);
        if self.player.state() == PlaybackState::Playing && !had_loaded_track {
            self.engine.play();
        }
        Ok(target)
    }

    /// The current volume, `0.0..=1.0`.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Whether a background library scan is running.
    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.scan_rx.is_some()
    }

    /// Whether (and why not) device audio is available.
    #[must_use]
    pub fn audio_status(&self) -> &AudioStatus {
        &self.audio
    }

    /// The persistence backend this controller uses (fixed at
    /// construction).
    #[must_use]
    pub fn storage_mode(&self) -> StorageMode {
        match &self.storage {
            StorageBackend::Legacy { .. } => StorageMode::Legacy,
            StorageBackend::Portable { .. } => StorageMode::Portable,
            StorageBackend::Canonical { .. } => StorageMode::Canonical,
        }
    }

    /// The legacy index path in legacy mode (`None` in portable mode,
    /// or when running without persistence).
    #[must_use]
    pub fn legacy_index_path(&self) -> Option<&Path> {
        match &self.storage {
            StorageBackend::Legacy { index_path } => index_path.as_deref(),
            StorageBackend::Portable { .. } | StorageBackend::Canonical { .. } => None,
        }
    }

    /// Whether this controller holds the ownership lease for a
    /// persistent backend. A session-only legacy library has nothing to
    /// protect and counts as owned.
    fn active_library_is_owned(&self) -> bool {
        match &self.storage {
            StorageBackend::Legacy { index_path: None } => true,
            StorageBackend::Legacy {
                index_path: Some(_),
            }
            | StorageBackend::Portable { .. }
            | StorageBackend::Canonical { .. } => self.library_lock.is_some(),
        }
    }

    fn ownership_guard(&self) -> Result<(), String> {
        if self.active_library_is_owned() {
            Ok(())
        } else {
            Err(LIBRARY_NOT_OWNED.to_owned())
        }
    }

    /// Whether the active legacy index could not be read and is being
    /// protected from overwrite. While true, scans and saves are
    /// refused until [`replace_unreadable_legacy_index`](Self::replace_unreadable_legacy_index)
    /// explicitly preserves the old file.
    #[must_use]
    pub fn legacy_index_write_protected(&self) -> bool {
        self.legacy_index_write_protected
    }

    /// The library root in portable mode (canonical), `None` in
    /// legacy mode.
    #[must_use]
    pub fn portable_root(&self) -> Option<&Path> {
        match &self.storage {
            StorageBackend::Portable { root } | StorageBackend::Canonical { root } => Some(root),
            StorageBackend::Legacy { .. } => None,
        }
    }

    /// The active canonical library root, when canonical mode is active.
    #[must_use]
    pub fn canonical_root(&self) -> Option<&Path> {
        match &self.storage {
            StorageBackend::Canonical { root } => Some(root),
            _ => None,
        }
    }

    /// Pending backup-recovery details in portable mode: while this
    /// is `Some`, the store is read-only and scans/saves refuse until
    /// [`acknowledge_portable_recovery`](Self::acknowledge_portable_recovery)
    /// is called.
    #[must_use]
    pub fn portable_recovery(&self) -> Option<&RecoveryInfo> {
        self.portable_recovery.as_ref()
    }

    /// Explicitly acknowledges a portable backup recovery: the
    /// validated backup catalogue is restored as the primary and the
    /// store becomes writable again (see
    /// `flux_library::PortableStore::acknowledge_recovery`). This is
    /// the only way out of the recovered read-only state — it never
    /// happens as a side effect of scanning or saving.
    ///
    /// # Errors
    ///
    /// A user-facing message in legacy mode, while a scan is in
    /// progress, when no recovery is pending, or when the store
    /// operation itself fails.
    pub fn acknowledge_portable_recovery(&mut self) -> Result<(), String> {
        self.ownership_guard()?;
        if matches!(self.storage, StorageBackend::Legacy { .. }) {
            return Err(
                "This controller uses the legacy library index; there is no portable \
                 recovery to acknowledge."
                    .to_owned(),
            );
        }
        if self.is_scanning() {
            return Err(
                "A scan is in progress; acknowledge the recovery when it finishes.".to_owned(),
            );
        }
        let Some(store) = self.portable_store.as_mut() else {
            return Err(PORTABLE_STORE_UNAVAILABLE.to_owned());
        };
        if !store.is_recovered_read_only() {
            return Err("The portable library is not in a recovered read-only state.".to_owned());
        }
        store
            .acknowledge_recovery()
            .map_err(|err| format!("Could not acknowledge the portable library recovery: {err}"))?;
        self.portable_recovery = None;
        self.persist_queue();
        self.report_status(
            "Portable library recovery acknowledged; scanning and saving are enabled.".to_owned(),
        );
        Ok(())
    }

    // --------------------------------------------------- switching ---

    /// Switches the active library to an existing canonical library.
    pub fn switch_to_canonical(&mut self, library_root: &Path) -> Result<(), SwitchError> {
        if self.is_scanning() {
            self.report_error(SwitchError::ScanInProgress.to_string());
            return Err(SwitchError::ScanInProgress);
        }
        let root = canonical_library_root(library_root).map_err(|err| {
            let err = SwitchError::from(err);
            self.report_error(err.to_string());
            err
        })?;
        if self.canonical_root() == Some(root.as_path()) {
            self.report_status(format!(
                "Already using the canonical library at {}.",
                root.display()
            ));
            return Ok(());
        }
        let lock = LibraryLock::acquire_portable_root(&root).map_err(|err| {
            let err = SwitchError::Canonical(CanonicalOpenError::Lock(err));
            self.report_error(err.to_string());
            err
        })?;
        let store = CanonicalLibrary::open(&root).map_err(|err| {
            let err = SwitchError::Canonical(CanonicalOpenError::Store(err));
            self.report_error(err.to_string());
            err
        })?;
        let library = store.runtime_library();
        let queue_store = Some(QueueStore::new(canonical_queue_path(&root)));
        let (queue, queue_auto_library, queue_write_protected, queue_error) =
            load_queue(queue_store.as_ref(), &library);
        self.install_library(LibrarySeed {
            library,
            storage: StorageBackend::Canonical { root: root.clone() },
            library_lock: Some(Arc::new(lock)),
            portable_store: None,
            canonical_store: Some(store),
            portable_recovery: None,
            roots: Vec::new(),
            queue,
            queue_store,
            queue_write_protected,
            queue_auto_library,
        });
        self.report_status(format!(
            "Switched to the canonical library at {}.",
            root.display()
        ));
        if let Some(error) = queue_error {
            self.report_error(error);
        }
        Ok(())
    }

    /// Creates a new canonical library and switches to it only after creation
    /// and reopening verify.
    pub fn switch_to_new_canonical_library(
        &mut self,
        library_root: &Path,
    ) -> Result<CreatedLibrary, CreateLibraryError> {
        if self.is_scanning() {
            return Err(CreateLibraryError::ScanInProgress);
        }
        let created = create_canonical_library(library_root)?;
        self.switch_to_canonical(created.root()).map_err(|err| {
            CreateLibraryError::Verification {
                root: library_root.to_owned(),
                detail: format!("created library could not be activated: {err}"),
            }
        })?;
        self.report_status(format!(
            "Created and switched to the canonical library at {}.",
            created.root().display()
        ));
        Ok(created)
    }

    /// Switches the active library to the existing portable library
    /// at `library_root`. The candidate is fully opened and
    /// validated first; only then is it installed — so on any
    /// [`SwitchError`] the current library, backend, queue, and
    /// playback state are completely unchanged, and the audio engine
    /// is never restarted (the same engine serves both libraries).
    ///
    /// On success playback stops and the destination library's own
    /// queue is loaded: track IDs are unique only within one library,
    /// so the old selection is discarded rather than ever being
    /// reinterpreted. Unsaved changes of the outgoing library (for
    /// example a portable sync whose commit previously failed) are
    /// discarded — switching never writes to the old backend as a
    /// side effect. A backup-recovered candidate installs in the
    /// usual read-only state (see
    /// [`portable_recovery`](Self::portable_recovery)). The outcome
    /// is also mirrored into [`last_error`](Self::last_error) /
    /// [`last_status`](Self::last_status) for front-ends.
    pub fn switch_to_portable(&mut self, library_root: &Path) -> Result<(), SwitchError> {
        if self.is_scanning() {
            self.report_error(SwitchError::ScanInProgress.to_string());
            return Err(SwitchError::ScanInProgress);
        }
        let root = match canonical_portable_root(library_root) {
            Ok(root) => root,
            Err(err) => {
                let err = SwitchError::from(err);
                self.report_error(err.to_string());
                return Err(err);
            }
        };
        if self.portable_root() == Some(root.as_path()) {
            self.report_status(format!(
                "Already using the portable library at {}.",
                root.display()
            ));
            return Ok(());
        }
        let (lock, opened, library, skipped) = match open_portable_candidate_at(&root) {
            Ok(candidate) => candidate,
            Err(err) => {
                let err = SwitchError::from(err);
                self.report_error(err.to_string());
                return Err(err);
            }
        };
        let recovery = opened.recovery;
        let queue_store = Some(QueueStore::new(portable_queue_path(&root)));
        let (queue, queue_auto_library, queue_write_protected, queue_error) =
            load_queue(queue_store.as_ref(), &library);
        self.install_library(LibrarySeed {
            library,
            storage: StorageBackend::Portable { root: root.clone() },
            library_lock: Some(Arc::new(lock)),
            portable_store: Some(opened.store),
            canonical_store: None,
            portable_recovery: recovery,
            roots: vec![root.clone()],
            queue,
            queue_store,
            queue_write_protected,
            queue_auto_library,
        });
        if skipped > 0 {
            self.report_status(skipped_unrepresentable_message(skipped));
        }
        if self.portable_recovery.is_some() {
            self.report_error(PORTABLE_RECOVERY_READ_ONLY.to_owned());
        } else {
            self.report_status(format!(
                "Switched to the portable library at {}.",
                root.display()
            ));
        }
        if let Some(error) = queue_error {
            self.report_error(error);
        }
        Ok(())
    }

    /// Creates a new portable library at `library_root` (see
    /// [`create_portable_library`]) and, only after it verifies,
    /// switches the active library to it (see
    /// [`switch_to_portable`](Self::switch_to_portable) for the
    /// switch semantics). If creation refuses or fails, the active
    /// library is unchanged and nothing was created over existing
    /// data.
    pub fn switch_to_new_portable_library(
        &mut self,
        library_root: &Path,
    ) -> Result<CreatedLibrary, CreateLibraryError> {
        if self.is_scanning() {
            self.report_error(CreateLibraryError::ScanInProgress.to_string());
            return Err(CreateLibraryError::ScanInProgress);
        }
        let created = match create_portable_library(library_root) {
            Ok(created) => created,
            Err(err) => {
                self.report_error(err.to_string());
                return Err(err);
            }
        };
        // The just-verified store is opened again through the normal
        // validated switch path — deliberately not a shortcut.
        match self.switch_to_portable(created.root()) {
            Ok(()) => {
                self.report_status(format!(
                    "Created and switched to the new portable library at {}.",
                    created.root().display()
                ));
                Ok(created)
            }
            Err(SwitchError::Portable(err)) => Err(CreateLibraryError::from(err)),
            Err(other) => {
                // ScanInProgress was pre-checked above and Legacy
                // cannot arise from a portable switch; keep the
                // error honest rather than unreachable!()-ing.
                Err(CreateLibraryError::Verification {
                    root: library_root.to_owned(),
                    detail: format!("the created library could not be activated: {other}"),
                })
            }
        }
    }

    /// Switches the active library to the legacy version-1 index at
    /// `index_path` (or to no persistence when `None`). An existing
    /// index is fully loaded and validated before anything changes;
    /// a corrupt one fails with [`SwitchError::Legacy`] and the
    /// current library stays active (no empty-library substitution —
    /// that leniency is a *startup* behavior of
    /// [`Controller::new`], not of switching). A missing index file
    /// means an empty legacy library, matching legacy startup.
    /// Playback stops and the destination index's own persisted queue
    /// is loaded, as with every switch.
    pub fn switch_to_legacy_index(
        &mut self,
        index_path: Option<PathBuf>,
    ) -> Result<(), SwitchError> {
        if self.is_scanning() {
            self.report_error(SwitchError::ScanInProgress.to_string());
            return Err(SwitchError::ScanInProgress);
        }
        if let (
            StorageBackend::Legacy {
                index_path: Some(current),
            },
            Some(candidate),
        ) = (&self.storage, &index_path)
            && same_library_path(current, candidate)
        {
            self.report_status("Already using that legacy library index.".to_owned());
            return Ok(());
        }

        // Acquire ownership before reading; scanning is refused above,
        // so the outgoing backend has no worker that could still write.
        let candidate_lock = match &index_path {
            Some(path) => match LibraryLock::acquire_legacy_index(path) {
                Ok(lock) => Some(Arc::new(lock)),
                Err(err) => {
                    let err = SwitchError::Lock(err);
                    self.report_error(err.to_string());
                    return Err(err);
                }
            },
            None => None,
        };
        let library = match &index_path {
            Some(path) if path.exists() => match Library::load_from_path(path) {
                Ok(library) => library,
                Err(err) => {
                    let err = SwitchError::Legacy(err);
                    self.report_error(err.to_string());
                    return Err(err);
                }
            },
            _ => Library::new(),
        };
        let queue_store = candidate_lock
            .as_ref()
            .map(|lock| QueueStore::new(legacy_queue_path(lock.identity())));
        let (queue, queue_auto_library, queue_write_protected, queue_error) =
            load_queue(queue_store.as_ref(), &library);
        let shown = index_path
            .as_ref()
            .map(|path| format!(" at {}", path.display()))
            .unwrap_or_default();
        self.install_library(LibrarySeed {
            library,
            storage: StorageBackend::Legacy { index_path },
            library_lock: candidate_lock,
            portable_store: None,
            canonical_store: None,
            portable_recovery: None,
            roots: Vec::new(),
            queue,
            queue_store,
            queue_write_protected,
            queue_auto_library,
        });
        self.report_status(format!("Switched to the legacy library index{shown}."));
        if let Some(error) = queue_error {
            self.report_error(error);
        }
        Ok(())
    }

    /// The single commit point of a library switch: stops playback,
    /// forgets the engine's loaded track, discards the outgoing
    /// library/store, and installs the already-validated candidate and
    /// its independently persisted queue. The outgoing queue selection
    /// is never carried over. Volume and repeat mode are user
    /// preferences, not library state, and are kept.
    fn install_library(&mut self, seed: LibrarySeed) {
        let LibrarySeed {
            library,
            storage,
            library_lock,
            portable_store,
            canonical_store,
            portable_recovery,
            roots,
            queue,
            queue_store,
            queue_write_protected,
            queue_auto_library,
        } = seed;
        self.player.stop();
        self.engine.stop();
        self.engine_loaded = None;
        self.engine_session = None;
        self.stream_info = None;

        self.tracks = snapshot(&library);
        self.library = Some(library);
        self.storage = storage;
        self.library_lock = library_lock;
        self.legacy_index_write_protected = false;
        self.portable_store = portable_store;
        self.canonical_store = canonical_store;
        self.portable_recovery = portable_recovery;
        self.roots = roots;
        self.queue_store = queue_store;
        self.queue_write_protected = queue_write_protected;
        self.queue_auto_library = queue_auto_library;
        *self.player.queue_mut() = queue;

        self.outbox.push(ControllerEvent::LibraryChanged);
        self.outbox.push(ControllerEvent::QueueChanged);
        self.outbox.push(ControllerEvent::PlaybackChanged);
    }

    // ---------------------------------------------- reconciliation ---

    /// Reconciles the active portable library's persistent catalogue
    /// with the filesystem (see
    /// [`flux_library::reconcile_portable_store`] for the exact
    /// semantics): availability and file fingerprints are refreshed
    /// for **every** catalogue record — including ones not currently
    /// loaded in memory — missing records are retained, inspection
    /// failures change nothing, and possible new locations for
    /// missing files are reported as candidates, never applied
    /// automatically (see
    /// [`relink_portable_track`](Self::relink_portable_track)).
    ///
    /// The result is published through the normal portable save path
    /// (sync + commit); if publication fails, the error is returned
    /// (and mirrored into [`last_error`](Self::last_error)), the
    /// previously committed catalogue stays intact on disk, and the
    /// reconciled state remains pending in memory so a later
    /// [`save_index`](Self::save_index) retries it. On success the
    /// runtime view is refreshed from the store — in-memory-only
    /// tracks are carried over, the queue selection is preserved
    /// (IDs never change), and playback is not interrupted (a
    /// relinked currently-playing track keeps playing its already
    /// loaded audio; the new path applies from the next load).
    ///
    /// Refused (with no change) while a scan is running, in legacy
    /// mode, and while a recovered store awaits acknowledgement.
    /// Never runs implicitly: not at startup, not after scans.
    pub fn reconcile_portable_library(&mut self) -> Result<ReconcileReport, String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        let Some(store) = self.portable_store.as_mut() else {
            let message = PORTABLE_STORE_UNAVAILABLE.to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let report = match reconcile_portable_store(store) {
            Ok(report) => report,
            Err(err) => {
                let message = format!("Could not reconcile the portable library: {err}");
                self.report_error(message.clone());
                return Err(message);
            }
        };
        self.publish_portable()?;
        self.refresh_from_store();
        self.report_status(format!(
            "Reconciled {} track(s): {} unchanged, {} changed, {} now present again, \
             {} missing, {} could not be inspected.",
            report.inspected_count(),
            report.unchanged_count() + report.established_count(),
            report.changed_count(),
            report.reappeared_count(),
            report.missing_count(),
            report.inspection_error_count() + report.fingerprint_unavailable_count(),
        ));
        Ok(report)
    }

    /// Relinks one missing portable track to a chosen candidate
    /// path (typically from a [`ReconcileReport`]), after the
    /// storage layer revalidates it: the candidate must be an
    /// unowned, present regular file whose size+mtime fingerprint
    /// exactly equals the stored one and whose extension matches
    /// the recorded path (see
    /// [`flux_library::relink_portable_track`]). Filename similarity
    /// never suffices. On success the track keeps its ID and
    /// metadata, the change is published through the normal save
    /// path, and the runtime view is refreshed; on any refusal
    /// nothing changes.
    pub fn relink_portable_track(
        &mut self,
        id: TrackId,
        candidate: &RelPath,
    ) -> Result<(), String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        let Some(store) = self.portable_store.as_mut() else {
            let message = PORTABLE_STORE_UNAVAILABLE.to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        if let Err(err) = flux_library::relink_portable_track(store, id, candidate) {
            let message = err.to_string();
            self.report_error(message.clone());
            return Err(message);
        }
        self.publish_portable()?;
        self.refresh_from_store();
        self.report_status(format!(
            "Relinked track {id} to {}.",
            candidate.display_lossy()
        ));
        Ok(())
    }

    /// Creates a new stable artist in the active portable catalogue.
    /// Duplicate display names are allowed.
    ///
    /// The catalogue is committed immediately; no audio tag is touched.
    pub fn create_artist(&mut self, name: &str) -> Result<ArtistId, String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        if let Some(store) = self.canonical_store.as_mut() {
            let id = match store.create_artist(name, Vec::new()) {
                Ok(id) => id,
                Err(err) => {
                    let message = format!("Could not create artist: {err}");
                    self.report_error(message.clone());
                    return Err(message);
                }
            };
            self.refresh_from_canonical();
            self.outbox.push(ControllerEvent::LibraryChanged);
            self.report_status(format!("Created artist {} (ID {}).", name.trim(), id));
            return Ok(id);
        }
        let Some(store) = self.portable_store.as_mut() else {
            let message = PORTABLE_STORE_UNAVAILABLE.to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let id = match store.create_artist(name) {
            Ok(id) => id,
            Err(err) => {
                let message = format!("Could not create artist: {err}");
                self.report_error(message.clone());
                return Err(message);
            }
        };
        self.commit_portable_changes()?;
        self.refresh_from_store();
        self.outbox.push(ControllerEvent::LibraryChanged);
        self.report_status(format!("Created artist {} (ID {}).", name.trim(), id));
        Ok(id)
    }

    /// Renames one stable artist. Every related track displays the new
    /// canonical name; their overrides and audio tags are untouched.
    pub fn rename_artist(&mut self, id: ArtistId, name: &str) -> Result<(), String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        if let Some(store) = self.canonical_store.as_mut() {
            if let Err(err) = store.rename_artist(id, name) {
                let message = format!("Could not rename artist: {err}");
                self.report_error(message.clone());
                return Err(message);
            }
            self.refresh_from_canonical();
            self.outbox.push(ControllerEvent::LibraryChanged);
            self.report_status(format!("Renamed artist {} to {}.", id, name.trim()));
            return Ok(());
        }
        let Some(store) = self.portable_store.as_mut() else {
            let message = PORTABLE_STORE_UNAVAILABLE.to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        if let Err(err) = store.rename_artist(id, name) {
            let message = format!("Could not rename artist: {err}");
            self.report_error(message.clone());
            return Err(message);
        }
        self.commit_portable_changes()?;
        self.refresh_from_store();
        self.outbox.push(ControllerEvent::LibraryChanged);
        self.report_status(format!("Renamed artist {} to {}.", id, name.trim()));
        Ok(())
    }

    /// Renames an album, preserving its stable ID, directory and every track
    /// reference.
    pub fn rename_album(&mut self, id: AlbumId, title: &str) -> Result<(), String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        let Some(store) = self.canonical_store.as_mut() else {
            let message = "Album operations require a canonical library.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        if let Err(err) = store.rename_album(id, title) {
            let message = format!("Could not rename album: {err}");
            self.report_error(message.clone());
            return Err(message);
        }
        self.refresh_from_canonical();
        self.outbox.push(ControllerEvent::LibraryChanged);
        self.report_status(format!("Renamed album {} to {}.", id, title.trim()));
        Ok(())
    }

    /// Replaces a canonical track's ordered credits. The physical owner must
    /// remain in the list; this never moves audio and never changes ownership.
    pub fn update_track_credits(
        &mut self,
        id: TrackId,
        credited_artist_ids: Vec<ArtistId>,
    ) -> Result<(), String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        let Some(store) = self.canonical_store.as_ref() else {
            let message = "Credits require a canonical library.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let Some(track) = store.track(id).cloned() else {
            let message = format!("Track {id} is not in the canonical library.");
            self.report_error(message.clone());
            return Err(message);
        };
        let store = self.canonical_store.as_mut().expect("canonical store");
        if let Err(err) = store.update_track_metadata(
            id,
            track.title,
            credited_artist_ids,
            track.track_number,
            track.disc_number,
            track.duration_ms,
        ) {
            let message = format!("Could not update credits: {err}");
            self.report_error(message.clone());
            return Err(message);
        }
        self.refresh_from_canonical();
        self.outbox.push(ControllerEvent::LibraryChanged);
        self.report_status(format!("Updated credits for track {id}."));
        Ok(())
    }

    /// Builds a read-only ownership-transfer plan for UI review.
    pub fn plan_track_ownership_transfer(
        &self,
        id: TrackId,
        new_owner: ArtistId,
        destination: OwnershipDestination,
    ) -> Result<OwnershipTransferPlan, String> {
        let Some(store) = self.canonical_store.as_ref() else {
            return Err("Ownership transfer requires a canonical library.".to_owned());
        };
        store
            .plan_ownership_transfer(id, new_owner, destination)
            .map_err(|err| format!("Could not plan ownership transfer: {err}"))
    }

    /// Executes a validated ownership-transfer plan. TrackId, credits,
    /// playlist/rating/queue references and album order are preserved.
    pub fn transfer_track_ownership(
        &mut self,
        plan: &OwnershipTransferPlan,
    ) -> Result<OwnershipTransferReport, String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        let Some(store) = self.canonical_store.as_mut() else {
            let message = "Ownership transfer requires a canonical library.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let report = store
            .execute_ownership_transfer(plan)
            .map_err(|err| format!("Could not transfer ownership: {err}"))?;
        self.refresh_from_canonical();
        self.outbox.push(ControllerEvent::LibraryChanged);
        self.report_status(format!(
            "Track {} transferred from {} to {}.",
            report.track_id, report.from_owner, report.to_owner
        ));
        Ok(report)
    }

    /// Builds a generation-tagged artist-deletion preview. Every affected
    /// track starts unresolved; execution is refused until the user resolves
    /// transfers, credit removals and explicit deletions.
    pub fn plan_artist_deletion(&self, id: ArtistId) -> Result<ArtistDeletionPlan, String> {
        let Some(store) = self.canonical_store.as_ref() else {
            return Err("Artist deletion planning requires a canonical library.".to_owned());
        };
        store
            .plan_artist_deletion(id)
            .map_err(|err| format!("Could not plan artist deletion: {err}"))
    }

    /// Executes a fully resolved artist-deletion plan through one canonical
    /// transaction. Queue entries for explicitly deleted tracks are removed;
    /// playback stops if the current track is deleted. All unaffected queue
    /// entries and stable track IDs remain.
    pub fn execute_artist_deletion(
        &mut self,
        plan: &ArtistDeletionPlan,
    ) -> Result<ArtistDeletionReport, String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        let deleted_tracks = plan.deleted_track_ids();
        let current = self.current_id();
        let current_deleted = current.is_some_and(|id| deleted_tracks.contains(&id));
        let Some(store) = self.canonical_store.as_mut() else {
            let message = "Artist deletion requires a canonical library.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let report = match store.execute_artist_deletion(plan) {
            Ok(report) => report,
            Err(err) => {
                let message = format!("Could not delete artist: {err}");
                self.report_error(message.clone());
                return Err(message);
            }
        };
        let mut queue_changed = false;
        for id in &deleted_tracks {
            queue_changed |= self.player.queue_mut().remove(*id) > 0;
        }
        if current_deleted {
            self.player.stop();
            self.engine.stop();
            self.engine_loaded = None;
            self.engine_session = None;
            self.stream_info = None;
        }
        self.refresh_from_canonical();
        if queue_changed {
            self.after_queue_change();
        }
        self.outbox.push(ControllerEvent::LibraryChanged);
        if current_deleted {
            self.outbox.push(ControllerEvent::PlaybackChanged);
        }
        self.report_status(format!(
            "Deleted artist {}: {} transferred, {} credit(s) removed, {} track(s) deleted.",
            report.artist_id,
            report.transferred,
            report.credits_removed,
            report.deleted_tracks.len()
        ));
        Ok(report)
    }

    /// Applies a full user metadata edit to one portable track without
    /// touching its audio file or embedded tags.
    ///
    /// The existing overrides are updated field by field, so fields the
    /// edit does not carry (notably `album_artist`, which the current UI
    /// does not expose) are preserved. When an artist ID is supplied, the
    /// stable relationship is set and the legacy text artist override is
    /// removed; `None` clears the relationship but leaves any legacy text
    /// override in place.
    pub fn edit_track_metadata(
        &mut self,
        id: TrackId,
        edit: TrackMetadataEdit,
    ) -> Result<(), String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        if self.canonical_store.is_some() {
            let Some(store) = self.canonical_store.as_ref() else {
                unreachable!()
            };
            let Some(track) = store.track(id).cloned() else {
                let message = format!("Track {id} is not in the canonical library.");
                self.report_error(message.clone());
                return Err(message);
            };
            if let Some(artist_id) = edit.artist_id
                && artist_id != track.owner_artist_id
            {
                let message = "Changing the physical owner is not available yet; use an                                ownership-transfer operation instead."
                    .to_owned();
                self.report_error(message.clone());
                return Err(message);
            }
            let title = edit
                .title
                .as_deref()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(&track.title)
                .to_owned();
            let track_number = edit.track_number;
            let disc_number = edit.disc_number;
            let credits = track.credited_artist_ids.clone();
            let album_change = edit
                .album
                .as_deref()
                .map(str::trim)
                .filter(|album| !album.is_empty())
                .is_some_and(|album| {
                    store
                        .album(track.album_id)
                        .is_none_or(|current| current.title != album)
                });
            let store = self.canonical_store.as_mut().expect("canonical store");
            if album_change
                && let Some(album) = edit
                    .album
                    .as_deref()
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                && let Err(err) = store.rename_album(track.album_id, album)
            {
                let message = format!("Could not rename album: {err}");
                self.report_error(message.clone());
                return Err(message);
            }
            if let Err(err) = store.update_track_metadata(
                id,
                title,
                credits,
                track_number,
                disc_number,
                track.duration_ms,
            ) {
                let message = format!("Could not update track metadata: {err}");
                self.report_error(message.clone());
                return Err(message);
            }
            self.refresh_from_canonical();
            self.outbox.push(ControllerEvent::LibraryChanged);
            self.report_status(format!("Updated metadata for track {id}."));
            return Ok(());
        }
        let Some(store) = self.portable_store.as_mut() else {
            let message = PORTABLE_STORE_UNAVAILABLE.to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let Some(stored) = store.get(id) else {
            let message = format!("Track {id} is not in the portable catalogue.");
            self.report_error(message.clone());
            return Err(message);
        };
        if let Some(artist_id) = edit.artist_id
            && store.artist(artist_id).is_none()
        {
            let message = format!("Artist {artist_id} is not in this library.");
            self.report_error(message.clone());
            return Err(message);
        }
        let mut overrides = stored.overrides().clone();
        match edit.title {
            Some(value) if !value.trim().is_empty() => overrides.set_title(value.trim().to_owned()),
            _ => overrides.revert_title(),
        }
        match edit.album {
            Some(value) if !value.trim().is_empty() => overrides.set_album(value.trim().to_owned()),
            _ => overrides.revert_album(),
        }
        match edit.track_number {
            Some(value) => overrides.set_track_number(value),
            None => overrides.revert_track_number(),
        }
        match edit.disc_number {
            Some(value) => overrides.set_disc_number(value),
            None => overrides.revert_disc_number(),
        }
        if edit.artist_id.is_some() {
            overrides.revert_artist();
        }
        if let Err(err) = store.set_overrides(id, overrides) {
            let message = format!("Could not update track metadata: {err}");
            self.report_error(message.clone());
            return Err(message);
        }
        if let Err(err) = store.set_track_artist(id, edit.artist_id) {
            let message = format!("Could not update the track artist: {err}");
            self.report_error(message.clone());
            return Err(message);
        }
        self.commit_portable_changes()?;
        self.refresh_from_store();
        self.outbox.push(ControllerEvent::LibraryChanged);
        self.report_status(format!("Updated metadata for track {id}."));
        Ok(())
    }

    /// Imports individual audio files into the active library.
    ///
    /// Canonical mode always copies every source into the managed canonical
    /// album directory and never reads audio tags: title, artist, album and
    /// track number are inferred conservatively from the filename and parent
    /// folders. `copy` is retained only for the migration-compatible portable
    /// backend; canonical mode ignores it and never creates a reference.
    /// Sources are never moved, deleted, or overwritten. Identical content is
    /// deduplicated by canonical fingerprint. Individual bad paths are
    /// reported and do not abort the batch.
    pub fn import_files(&mut self, paths: &[PathBuf], copy: bool) -> Result<ImportReport, String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        if self.canonical_store.is_some() {
            return self.import_files_canonical(paths, copy);
        }
        let Some(root) = self.portable_root().map(Path::to_path_buf) else {
            let message = PORTABLE_STORE_UNAVAILABLE.to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        if self.library.is_none() {
            let message =
                "A library scan is in progress; import files when it finishes.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        }

        let mut report = ImportReport::default();
        let mut effective_paths: Vec<PathBuf> = Vec::new();
        for source in paths {
            match self.prepare_import_source(source, &root, copy, &mut report) {
                Ok(path) => effective_paths.push(path),
                Err(reason) => report.issues.push(ImportIssue {
                    path: source.clone(),
                    reason,
                }),
            }
        }

        if !effective_paths.is_empty() {
            let Some(library) = self.library.as_mut() else {
                unreachable!("library existence was checked above");
            };
            for path in &effective_paths {
                let scan = library.scan(&[path.as_path()]);
                report.added += scan.added();
                report.updated += scan.updated();
                report.deduplicated += scan.deduplicated();
                for issue in scan.skipped().iter().chain(scan.failed()) {
                    report.issues.push(ImportIssue {
                        path: issue.path().to_path_buf(),
                        reason: issue.reason().to_owned(),
                    });
                }
            }
            self.publish_portable()?;
            self.refresh_from_store();
            self.sync_default_queue();
            self.outbox.push(ControllerEvent::LibraryChanged);
        }

        let message = format!(
            "Import complete: {} added, {} updated, {} already indexed, {} copied, {} issue(s).",
            report.added,
            report.updated,
            report.deduplicated,
            report.copied.len(),
            report.issues.len()
        );
        self.report_status(message);
        Ok(report)
    }

    fn prepare_import_source(
        &self,
        source: &Path,
        root: &Path,
        copy: bool,
        report: &mut ImportReport,
    ) -> Result<PathBuf, String> {
        let metadata = std::fs::symlink_metadata(source)
            .map_err(|err| format!("cannot inspect the file: {err}"))?;
        if metadata.file_type().is_symlink() {
            return Err("symbolic links are never imported; use the real file path".to_owned());
        }
        if !metadata.is_file() {
            return Err("not a regular file".to_owned());
        }
        if !flux_library::is_supported_audio_path(source) {
            return Err("unsupported or missing audio extension".to_owned());
        }
        let canonical = std::fs::canonicalize(source)
            .map_err(|err| format!("cannot resolve the file path: {err}"))?;
        if canonical.starts_with(root) {
            return Ok(canonical);
        }
        if !copy {
            return Err(format!(
                "the file is outside this portable library ({}); enable Copy to import it",
                root.display()
            ));
        }
        let destination = copy_into_library(root, &canonical)?;
        if destination.was_copied {
            report.copied.push(destination.path.clone());
        }
        Ok(destination.path)
    }

    /// Commits catalogue changes without re-syncing the runtime snapshot
    /// into tags first. Used by direct user edits (metadata/artist), where
    /// the store is authoritative and the old runtime metadata must not be
    /// mistaken for a scan refresh.
    fn commit_portable_changes(&mut self) -> Result<(), String> {
        let Some(store) = self.portable_store.as_mut() else {
            let message = PORTABLE_STORE_UNAVAILABLE.to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        if let Err(err) = store.commit() {
            let message = format!("Could not save the portable catalogue: {err}");
            self.report_error(message.clone());
            return Err(message);
        }
        Ok(())
    }

    /// Shared preconditions for portable catalogue maintenance
    /// (reconcile/relink): no scan in flight (the store is owned by
    /// the scan worker then), portable backend active, no
    /// unacknowledged recovery.
    fn portable_maintenance_guard(&self) -> Result<(), String> {
        if self.is_scanning() {
            return Err("A library scan is in progress; try again when it finishes.".to_owned());
        }
        self.ownership_guard()?;
        if matches!(self.storage, StorageBackend::Legacy { .. }) {
            return Err(
                "This applies to portable libraries; the active library uses the legacy index."
                    .to_owned(),
            );
        }
        if self.portable_recovery.is_some() {
            return Err(PORTABLE_RECOVERY_READ_ONLY.to_owned());
        }
        Ok(())
    }

    /// Publishes pending portable state through the one save path
    /// (sync + commit), reporting any failure. Reconciliation's
    /// availability/fingerprint updates survive the sync untouched
    /// by design.
    fn publish_portable(&mut self) -> Result<(), String> {
        let (Some(library), Some(store)) = (self.library.as_ref(), self.portable_store.as_mut())
        else {
            let message = PORTABLE_STORE_UNAVAILABLE.to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        match persist_portable(library, store) {
            Ok(_) => Ok(()),
            Err(message) => {
                self.report_error(message.clone());
                Err(message)
            }
        }
    }

    /// Rebuilds the runtime library from the active portable store,
    /// carrying over in-memory-only tracks and preserving the queue
    /// selection (same library, same IDs — unlike a library switch).
    fn rebuilt_from_store(&self) -> Option<PortableRefreshReport> {
        let current = self.library.as_ref()?;
        let store = self.portable_store.as_ref()?;
        Some(refresh_library_from_portable_store(current, store))
    }

    /// Installs an already-merged runtime library without changing
    /// playback state or the user-managed queue. Stable track IDs mean
    /// queue entries keep referring to the same tracks; newly discovered
    /// tracks are appended only while the queue is still auto-derived.
    fn install_library_snapshot(&mut self, library: Library) {
        self.tracks = snapshot(&library);
        self.library = Some(library);
    }

    /// Refreshes the runtime library/snapshot from the portable store
    /// after catalogue maintenance, preserving the queue selection and
    /// reporting any tracks that could not be represented or carried
    /// over.
    /// Imports one canonical track with explicit reviewed credits. This is the
    /// application-level entry point used by tests and future non-GUI clients;
    /// metadata must never come from audio tags.
    pub fn import_canonical_track(
        &mut self,
        imported: CanonicalImportedTrack,
    ) -> Result<TrackId, String> {
        if let Err(message) = self.portable_maintenance_guard() {
            self.report_error(message.clone());
            return Err(message);
        }
        let Some(store) = self.canonical_store.as_mut() else {
            return Err("Canonical import requires a canonical library.".to_owned());
        };
        let id = store
            .import_track(imported)
            .map_err(|err| format!("Could not import track: {err}"))?;
        self.refresh_from_canonical();
        self.outbox.push(ControllerEvent::LibraryChanged);
        Ok(id)
    }

    /// Recursively imports supported audio files from a folder. Canonical
    /// mode uses filename/folder inference and copies every file; sources are
    /// never moved. Symlinks are not followed.
    pub fn import_folder(&mut self, root: &Path, recursive: bool) -> Result<ImportReport, String> {
        let mut files = Vec::new();
        let mut issues = Vec::new();
        collect_audio_files(root, recursive, &mut files, &mut issues)?;
        let mut report = self.import_files(&files, true)?;
        report.issues.extend(issues);
        Ok(report)
    }

    fn import_files_canonical(
        &mut self,
        paths: &[PathBuf],
        _copy: bool,
    ) -> Result<ImportReport, String> {
        let Some(store) = self.canonical_store.as_mut() else {
            let message = "The canonical library is unavailable.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let mut report = ImportReport::default();
        for source in paths {
            if !is_supported_audio_path(source) {
                report.issues.push(ImportIssue {
                    path: source.clone(),
                    reason: "unsupported or missing audio extension".to_owned(),
                });
                continue;
            }
            let Some(inferred) = infer_canonical_import(source) else {
                report.issues.push(ImportIssue {
                    path: source.clone(),
                    reason: "the source file cannot be read".to_owned(),
                });
                continue;
            };
            let fingerprint = match flux_library::canonical::ContentFingerprint::of_file(source) {
                Ok(fingerprint) => fingerprint,
                Err(err) => {
                    report.issues.push(ImportIssue {
                        path: source.clone(),
                        reason: format!("could not fingerprint source: {err}"),
                    });
                    continue;
                }
            };
            if store.track_by_fingerprint(fingerprint).is_some() {
                report.deduplicated += 1;
                continue;
            }
            match store.import_track(inferred) {
                Ok(id) => {
                    report.added += 1;
                    if let Some(path) = store.track_audio_path(id) {
                        report.copied.push(path);
                    }
                }
                Err(err) => report.issues.push(ImportIssue {
                    path: source.clone(),
                    reason: err.to_string(),
                }),
            }
        }
        self.refresh_from_canonical();
        self.sync_default_queue();
        self.outbox.push(ControllerEvent::LibraryChanged);
        if report.added > 0 || report.deduplicated > 0 {
            self.report_status(format!(
                "Canonical import: {} added, {} duplicate(s), {} issue(s).",
                report.added,
                report.deduplicated,
                report.issues.len()
            ));
        }
        Ok(report)
    }

    fn refresh_from_canonical(&mut self) {
        let Some(store) = self.canonical_store.as_ref() else {
            return;
        };
        let library = store.runtime_library();
        self.tracks = snapshot(&library);
        self.library = Some(library);
    }

    fn refresh_from_store(&mut self) {
        let Some(report) = self.rebuilt_from_store() else {
            return;
        };
        let PortableRefreshReport {
            library,
            skipped_store_records,
            dropped_memory_only,
        } = report;
        self.install_library_snapshot(library);
        self.outbox.push(ControllerEvent::LibraryChanged);
        if let Some(note) = refresh_skip_note(skipped_store_records, &dropped_memory_only) {
            self.report_status(note);
        }
    }

    /// Decoded-stream facts for the loaded track, once known.
    #[must_use]
    pub fn stream_info(&self) -> Option<StreamInfo> {
        self.stream_info
    }

    /// The most recent user-facing error, until another replaces it.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// The most recent informational status message.
    #[must_use]
    pub fn last_status(&self) -> Option<&str> {
        self.last_status.as_deref()
    }

    // ------------------------------------------------------- commands ---

    /// Plays a library track immediately.
    ///
    /// If the track already has queue entries, the first one is selected
    /// (duplicates remain individually addressable through
    /// [`play_queue_entry`](Self::play_queue_entry)). Otherwise the track
    /// is appended to the queue and selected. The queue order is not
    /// otherwise changed, and the entry is persisted when possible.
    pub fn play_track(&mut self, id: TrackId) {
        let Some(title) = self.track(id).map(|track| track.title().to_owned()) else {
            self.report_error("That track is no longer in the library.".to_owned());
            return;
        };
        let before_entry = self.current_queue_entry_id();
        let before_len = self.queue_len();
        let entry = {
            let queue = self.player.queue_mut();
            match queue.entry_for_track(id) {
                Some(entry) => entry.id(),
                None => queue.push(id),
            }
        };
        if let Err(err) = self.player.play_entry(entry) {
            self.report_error(format!("Cannot play \"{title}\": {err}"));
            return;
        }
        if before_len != self.queue_len() {
            self.queue_auto_library = false;
            self.after_queue_change();
        } else if before_entry != self.current_queue_entry_id() {
            self.after_queue_change();
        }
        self.sync_engine(true);
    }

    /// Plays one specific queue entry, including a particular duplicate.
    pub fn play_queue_entry(&mut self, entry: QueueEntryId) -> Result<(), String> {
        if self.player.queue().entry(entry).is_none() {
            let message = format!("Queue entry {entry} is no longer in the queue.");
            self.report_error(message.clone());
            return Err(message);
        }
        let before = self.current_queue_entry_id();
        self.player
            .play_entry(entry)
            .map_err(|err| format!("Cannot play that queue entry: {err}"))?;
        if before != self.current_queue_entry_id() {
            self.after_queue_change();
        }
        self.sync_engine(true);
        Ok(())
    }

    /// Appends a library track to the queue without changing playback.
    pub fn enqueue_track(&mut self, id: TrackId) -> Result<QueueEntryId, String> {
        let Some(title) = self.track(id).map(|track| track.title().to_owned()) else {
            let message = "That track is no longer in the library.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let entry = self.player.queue_mut().push(id);
        self.queue_auto_library = false;
        self.after_queue_change();
        self.report_status(format!("Added \"{title}\" to the queue."));
        Ok(entry)
    }

    /// Inserts a library track immediately after the current entry without
    /// starting it. With an empty queue it becomes the current entry.
    pub fn play_next_track(&mut self, id: TrackId) -> Result<QueueEntryId, String> {
        let Some(title) = self.track(id).map(|track| track.title().to_owned()) else {
            let message = "That track is no longer in the library.".to_owned();
            self.report_error(message.clone());
            return Err(message);
        };
        let entry = self.player.queue_mut().insert_next(id);
        self.queue_auto_library = false;
        self.after_queue_change();
        self.report_status(format!("Will play \"{title}\" next."));
        Ok(entry)
    }

    /// Removes one queue entry by its stable identity.
    ///
    /// Removing the current entry stops playback and selects the following
    /// entry (or the new last entry); removing any other entry leaves the
    /// current track and playback state untouched.
    pub fn remove_queue_entry(&mut self, entry: QueueEntryId) -> Result<bool, String> {
        let was_current = self.current_queue_entry_id() == Some(entry);
        let Some(removed) = self.player.queue_mut().remove_entry(entry) else {
            return Ok(false);
        };
        if was_current {
            self.player.stop();
            self.engine.stop();
            self.engine_loaded = None;
            self.engine_session = None;
            self.stream_info = None;
            self.outbox.push(ControllerEvent::PlaybackChanged);
        }
        self.queue_auto_library = false;
        self.after_queue_change();
        self.report_status(format!("Removed track {} from the queue.", removed.track()));
        Ok(true)
    }

    /// Moves one queue entry to a target position, clamping to the queue.
    /// The current track and playback state are unaffected.
    pub fn move_queue_entry(
        &mut self,
        entry: QueueEntryId,
        target_index: usize,
    ) -> Result<bool, String> {
        if self.player.queue().entry(entry).is_none() {
            return Ok(false);
        }
        let moved = self.player.queue_mut().move_entry(entry, target_index);
        if moved {
            self.queue_auto_library = false;
            self.after_queue_change();
        }
        Ok(moved)
    }

    /// Moves one queue entry by a relative offset, clamping to the queue.
    pub fn move_queue_entry_by(
        &mut self,
        entry: QueueEntryId,
        delta: isize,
    ) -> Result<bool, String> {
        if self.player.queue().entry(entry).is_none() {
            return Ok(false);
        }
        let moved = self.player.queue_mut().move_entry_by(entry, delta);
        if moved {
            self.queue_auto_library = false;
            self.after_queue_change();
        }
        Ok(moved)
    }

    /// Moves one queue entry to insertion gap `gap` (`0..=len`, where
    /// `0` is before the first entry and `len` is after the last).
    ///
    /// Playback, the current entry identity, repeat mode, and the queue
    /// persistence guarantees are unchanged except for the new order.
    /// Invalid gaps or absent entries are no-ops.
    pub fn move_queue_entry_to_gap(
        &mut self,
        entry: QueueEntryId,
        gap: usize,
    ) -> Result<bool, String> {
        if gap > self.queue_len() || self.player.queue().entry(entry).is_none() {
            return Ok(false);
        }
        let moved = self.player.queue_mut().move_entry_to_gap(entry, gap);
        if moved {
            self.queue_auto_library = false;
            self.after_queue_change();
        }
        Ok(moved)
    }

    /// Starts or resumes playback of the current selection.
    pub fn play(&mut self) {
        match self.player.play() {
            Ok(()) => self.sync_engine(false),
            Err(err) => self.report_error(format!("Nothing to play: {err}")),
        }
    }

    /// Pauses playback (no-op unless playing).
    pub fn pause(&mut self) {
        self.player.pause();
        self.sync_engine(false);
    }

    /// Toggles between playing and paused (from stopped: plays).
    pub fn toggle_play_pause(&mut self) {
        if self.player.state() == PlaybackState::Playing {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Stops playback. The selection is retained.
    pub fn stop(&mut self) {
        self.player.stop();
        self.sync_engine(false);
    }

    /// Skips to the next track; at the end of the queue (repeat off)
    /// playback stops. While paused, the new track is loaded paused.
    pub fn next(&mut self) {
        let before = self.current_queue_entry_id();
        self.player.next_track();
        if before != self.current_queue_entry_id() {
            self.after_queue_change();
        }
        self.sync_engine(true);
    }

    /// Skips to the previous track; on the first track, restarts it.
    pub fn previous(&mut self) {
        let before = self.current_queue_entry_id();
        self.player.previous_track();
        if before != self.current_queue_entry_id() {
            self.after_queue_change();
        }
        self.sync_engine(true);
    }

    /// Sets the repeat mode used for skips and end-of-track handling.
    pub fn set_repeat_mode(&mut self, mode: RepeatMode) {
        self.player.set_repeat_mode(mode);
    }

    /// Cycles repeat: Off → All → One → Off.
    pub fn cycle_repeat_mode(&mut self) {
        let next = match self.player.repeat_mode() {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        self.set_repeat_mode(next);
        self.report_status(format!("Repeat: {}", repeat_label(next)));
    }

    /// Sets the volume (clamped to `0.0..=1.0`) and applies it to the
    /// engine immediately.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        };
        self.engine.set_volume(self.volume);
    }

    /// Adjusts the volume by a delta (keyboard-friendly).
    pub fn adjust_volume(&mut self, delta: f32) {
        self.set_volume(self.volume + delta);
    }

    /// Begins a background scan of `root` (recursive). Completion (and
    /// the refreshed track list) arrives via [`poll`](Self::poll);
    /// playback in progress is unaffected. A second scan while one is
    /// running is reported as an error and ignored. A `root` that does
    /// not exist or is not a folder is likewise reported as an error
    /// and ignored — no scan starts and no library or index state
    /// changes. When the scan finishes, the updated library is saved
    /// to the active backend (legacy index path, if any, or the
    /// portable store) on the scan thread.
    ///
    /// In portable mode a scan is also refused — same "reported and
    /// ignored" contract — while the store is in the recovered
    /// read-only state (a scan's completion would have to write), and
    /// for folders outside the library root: a portable library can
    /// only store files inside its own root, so scanning its root (or
    /// a subfolder) is the whole model.
    pub fn begin_scan(&mut self, root: PathBuf) {
        self.begin_scan_roots(vec![root]);
    }

    /// Re-scans every directory scanned this session (picking up files
    /// added since). With no prior scan this session, reports a status
    /// message instead. Roots are not persisted in the version-1
    /// index, so refresh is session-scoped by design. (In portable
    /// mode the library root itself is the session's initial scan
    /// root, so refresh always has somewhere to look.)
    pub fn refresh(&mut self) {
        if let StorageBackend::Canonical { root } = &self.storage {
            let root = root.clone();
            match CanonicalLibrary::open(&root) {
                Ok(store) => {
                    self.canonical_store = Some(store);
                    self.refresh_from_canonical();
                    self.outbox.push(ControllerEvent::LibraryChanged);
                    self.report_status("Refreshed the canonical library from disk.".to_owned());
                }
                Err(err) => {
                    self.report_error(format!("Could not refresh canonical library: {err}"))
                }
            }
            return;
        }
        if self.roots.is_empty() {
            self.report_status("Nothing to refresh yet — scan a folder first.".to_owned());
            return;
        }
        let roots = self.roots.clone();
        self.begin_scan_roots(roots);
    }

    /// Saves the library to the active backend now (blocking;
    /// intended for shutdown — scans already save automatically on
    /// completion). Named for its legacy-index origin; in portable
    /// mode it syncs and commits the portable store. A failed save
    /// loses nothing: the in-memory library (and, portably, the
    /// synced-but-uncommitted store state) is retained, so a later
    /// save retries the same changes.
    ///
    /// # Errors
    ///
    /// A user-facing message if a scan is in progress, the write
    /// fails, or (portable) the store is read-only pending recovery
    /// acknowledgement or unavailable. In legacy mode without an
    /// index path this is a no-op success.
    pub fn save_index(&mut self) -> Result<(), String> {
        self.ownership_guard()?;
        let Some(library) = self.library.as_ref() else {
            return Err(match self.storage {
                StorageBackend::Legacy { .. } => {
                    "A scan is in progress; the index will be saved when it finishes."
                }
                StorageBackend::Portable { .. } => {
                    "A scan is in progress; the portable library will be saved when it finishes."
                }
                StorageBackend::Canonical { .. } => {
                    "A canonical library operation is in progress; try again when it finishes."
                }
            }
            .into());
        };
        match &self.storage {
            StorageBackend::Legacy { index_path } => {
                if self.legacy_index_write_protected {
                    return Err(LEGACY_INDEX_WRITE_PROTECTED.to_owned());
                }
                match index_path {
                    Some(path) => save_library(library, path),
                    None => Ok(()),
                }
            }
            StorageBackend::Portable { .. } => {
                if self
                    .portable_store
                    .as_ref()
                    .is_some_and(|store| store.is_recovered_read_only())
                {
                    return Err(PORTABLE_RECOVERY_READ_ONLY.to_owned());
                }
                match self.portable_store.as_mut() {
                    Some(store) => persist_portable(library, store).map(|_| ()),
                    None => Err(PORTABLE_STORE_UNAVAILABLE.to_owned()),
                }
            }
            StorageBackend::Canonical { .. } => Ok(()),
        }
    }

    /// Explicitly resolves a protected unreadable legacy index: renames
    /// the existing file to a sibling `<name>.unreadable` (or
    /// `.unreadable-N` if taken), preserves its bytes, clears the write
    /// protection, and enables scanning and saving again.
    ///
    /// This is the only way out of the protected state. Nothing is
    /// deleted and nothing is overwritten; if the rename fails, the
    /// file is untouched and protection remains active.
    ///
    /// # Errors
    ///
    /// A user-facing message when no protected legacy index exists, a
    /// scan is running, or the file cannot be renamed. In the last case
    /// the original file is left intact and protected.
    pub fn replace_unreadable_legacy_index(&mut self) -> Result<PathBuf, String> {
        self.ownership_guard()?;
        if self.is_scanning() {
            return Err("A library scan is in progress; try again when it finishes.".to_owned());
        }
        if !self.legacy_index_write_protected {
            return Err(
                "The active library index is not write-protected; there is nothing to replace."
                    .to_owned(),
            );
        }
        let StorageBackend::Legacy {
            index_path: Some(index_path),
        } = &self.storage
        else {
            return Err(
                "The protected index is not a legacy library index with a file path.".to_owned(),
            );
        };
        let index_path = index_path.clone();
        if !index_path.exists() {
            self.legacy_index_write_protected = false;
            self.last_error = None;
            self.persist_queue();
            self.report_status(format!(
                "The unreadable library index at {} no longer exists; scanning and saving are enabled.",
                index_path.display()
            ));
            return Ok(index_path);
        }

        let backup = preserved_legacy_index_path(&index_path)?;
        std::fs::rename(&index_path, &backup).map_err(|err| {
            format!(
                "Could not preserve the unreadable library index at {}: {err}. \
                 The file was left untouched and remains protected.",
                index_path.display()
            )
        })?;

        self.legacy_index_write_protected = false;
        self.last_error = None;
        self.persist_queue();
        self.report_status(format!(
            "Preserved the unreadable library index as {}. Scanning and saving are enabled.",
            backup.display()
        ));
        Ok(backup)
    }

    /// Clean shutdown: waits for any running scan to finish (its save
    /// included), then stops the audio engine and joins its worker.
    pub fn shutdown(mut self) {
        if let Some(rx) = self.scan_rx.take()
            && let Ok(outcome) = rx.recv()
        {
            self.library = Some(outcome.library);
            if let ScanPersistence::Portable(store) = outcome.persistence {
                self.portable_store = Some(*store);
            }
        }
        self.engine.shutdown();
    }

    // ------------------------------------------------------------ poll ---

    /// Drains pending notifications: queued command results, scan
    /// completion, and engine events (end-of-track advancement and
    /// playback errors are handled here). Never blocks; call it on a
    /// short timer from the UI event loop.
    pub fn poll(&mut self) -> Vec<ControllerEvent> {
        let mut events = std::mem::take(&mut self.outbox);

        // `Done` is boxed because a `ScanOutcome` carries the whole
        // `Library` (a large value to move through every `poll`);
        // boxing it only on the rare scan-finished poll keeps this
        // per-call enum small.
        enum ScanPoll {
            Done(Box<ScanOutcome>),
            Lost,
        }
        let scan_state = self.scan_rx.as_ref().map(|rx| match rx.try_recv() {
            Ok(outcome) => Some(ScanPoll::Done(Box::new(outcome))),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(ScanPoll::Lost),
        });
        match scan_state {
            Some(Some(ScanPoll::Done(outcome))) => {
                self.scan_rx = None;
                self.finish_scan(*outcome);
                events.push(ControllerEvent::LibraryChanged);
            }
            Some(Some(ScanPoll::Lost)) => {
                self.scan_rx = None;
                let message = if self.library.is_none() {
                    // The scan thread owned the library and is gone
                    // without reporting back (e.g. it panicked), so the
                    // in-memory collection cannot come back from the
                    // thread; recover what can faithfully be recovered.
                    if self.recover_from_lost_scan() {
                        match self.storage {
                            StorageBackend::Legacy { .. } => {
                                "The library scan ended unexpectedly; the library \
                                 was restored from the saved index."
                            }
                            StorageBackend::Portable { .. } => {
                                "The library scan ended unexpectedly; the library \
                                 was restored from the portable library files."
                            }
                            StorageBackend::Canonical { .. } => {
                                "The library scan ended unexpectedly; the canonical \
                                 library was reopened."
                            }
                        }
                    } else {
                        match self.storage {
                            StorageBackend::Legacy { .. } => {
                                "The library scan ended unexpectedly, and no saved \
                                 index was available to restore the library from."
                            }
                            StorageBackend::Portable { .. } => {
                                "The library scan ended unexpectedly, and the \
                                 portable library could not be reopened to \
                                 restore the library from."
                            }
                            StorageBackend::Canonical { .. } => {
                                "The library scan ended unexpectedly, and the \
                                 canonical library could not be reopened."
                            }
                        }
                    }
                } else {
                    "The library scan ended unexpectedly."
                };
                self.report_error(message.to_owned());
            }
            _ => {}
        }

        let engine_events: Vec<EngineEvent> = {
            let receiver = self.engine.events();
            let mut drained = Vec::new();
            while let Ok(event) = receiver.try_recv() {
                drained.push(event);
            }
            drained
        };
        for event in engine_events {
            self.handle_engine_event(event, &mut events);
        }

        events.append(&mut self.outbox);
        events
    }

    /// Handles exactly one engine event. Session correlation lives here
    /// so delayed events can be ignored deterministically and tests can
    /// inject controlled event ordering without sleeping.
    fn handle_engine_event(&mut self, event: EngineEvent, events: &mut Vec<ControllerEvent>) {
        match event {
            EngineEvent::Loaded { session, id, info } => {
                if self.engine_session == Some(session) && self.engine_loaded == Some(id) {
                    self.stream_info = Some(info);
                    events.push(ControllerEvent::PlaybackChanged);
                }
            }
            EngineEvent::Started { session, .. } | EngineEvent::Paused { session, .. } => {
                if self.engine_session == Some(session) {
                    events.push(ControllerEvent::PlaybackChanged);
                }
            }
            EngineEvent::Stopped => events.push(ControllerEvent::PlaybackChanged),
            EngineEvent::TrackEnded { session, id } => {
                if self.engine_session == Some(session)
                    && self.engine_loaded == Some(id)
                    && self.player.current_track() == Some(id)
                    && self.player.state() == PlaybackState::Playing
                {
                    // Advance (or stop at the end), then reconcile the
                    // engine to the model. At the end of the queue this
                    // clears the sync ledger so a later Play performs a
                    // fresh load instead of restarting an old session.
                    let before = self.current_queue_entry_id();
                    self.player.on_track_ended();
                    if before != self.current_queue_entry_id() {
                        self.after_queue_change();
                    }
                    self.sync_engine(true);
                    events.push(ControllerEvent::PlaybackChanged);
                }
            }
            EngineEvent::Error { session, id, error } => {
                // A session-qualified error is current only when it
                // belongs to the load attempt the controller is still
                // tracking. None is a global output failure and always
                // matters.
                if session.is_some() && session != self.engine_session {
                    return;
                }
                let message = match id.and_then(|i| self.track(i)) {
                    Some(track) => {
                        format!("Playback failed for \"{}\": {error}", track.title())
                    }
                    None => format!("Audio error: {error}"),
                };
                self.player.stop();
                self.engine.stop();
                self.engine_loaded = None;
                self.engine_session = None;
                self.stream_info = None;
                self.report_error(message);
                events.push(ControllerEvent::PlaybackChanged);
            }
        }
    }

    // -------------------------------------------------------- internals ---

    fn queue_persistence_allowed(&self) -> bool {
        !self.queue_write_protected
            && !self.legacy_index_write_protected
            && self.portable_recovery.is_none()
    }

    /// Publishes a queue mutation to front-ends and persists it when the
    /// active library state permits writes. A malformed loaded queue file
    /// or a protected/recovered catalogue leaves the original file
    /// untouched.
    fn after_queue_change(&mut self) {
        self.persist_queue();
        self.outbox.push(ControllerEvent::QueueChanged);
    }

    fn persist_queue(&mut self) {
        if !self.queue_persistence_allowed() {
            return;
        }
        let Some(store) = &self.queue_store else {
            return;
        };
        let result = store.save(self.player.queue(), self.queue_auto_library);
        if let Err(error) = result {
            let message = format!(
                "Could not save the playback queue ({}): {error}. \
                 The queue remains in memory and will be retried on the next change.",
                store.path().display()
            );
            self.report_error(message);
        }
    }

    /// While the queue is still the library-derived default, append tracks
    /// discovered by a scan. Existing entries, identities, and the current
    /// selection are untouched. Once the user structurally edits the queue
    /// this never runs again.
    fn sync_default_queue(&mut self) {
        if !self.queue_auto_library {
            return;
        }
        let ids: Vec<TrackId> = self.tracks.iter().map(|(id, _)| *id).collect();
        let mut present: std::collections::HashSet<TrackId> = self
            .player
            .queue()
            .entries()
            .iter()
            .map(|entry| entry.track())
            .collect();
        let mut added = false;
        for id in ids {
            if present.insert(id) {
                self.player.queue_mut().push(id);
                added = true;
            }
        }
        if added {
            self.after_queue_change();
        }
    }

    /// The one place engine commands are derived from `Player` state.
    /// `force_reload` distinguishes "the user asked for (possibly the
    /// same) track explicitly — start it from the top" from "resume
    /// whatever is loaded".
    fn sync_engine(&mut self, force_reload: bool) {
        match self.player.state() {
            PlaybackState::Playing => {
                let Some(id) = self.player.current_track() else {
                    self.engine.stop();
                    self.engine_loaded = None;
                    return;
                };
                if self.engine_loaded == Some(id) && !force_reload {
                    self.engine.play();
                } else if let Err(message) = self.load_into_engine(id, true) {
                    self.fail_playback(message);
                }
            }
            PlaybackState::Paused => match self.player.current_track() {
                Some(id) if self.engine_loaded == Some(id) => self.engine.pause(),
                Some(id) => {
                    if let Err(message) = self.load_into_engine(id, false) {
                        self.fail_playback(message);
                    }
                }
                None => {
                    self.engine.stop();
                    self.engine_loaded = None;
                }
            },
            PlaybackState::Stopped => {
                self.engine.stop();
                self.engine_loaded = None;
                self.engine_session = None;
                self.stream_info = None;
            }
        }
    }

    /// Resolves `id` to a file and asks the engine to load it.
    /// Synchronous failures (no audio, unknown track, unsupported
    /// format) are returned for the caller to surface; decode/IO
    /// failures arrive asynchronously as engine error events.
    fn load_into_engine(&mut self, id: TrackId, autoplay: bool) -> Result<(), String> {
        if let Some(reason) = self.audio.reason() {
            return Err(reason.to_owned());
        }
        let Some(track) = self.track(id) else {
            return Err("That track is no longer in the library.".to_owned());
        };
        let path = track.source().path().to_path_buf();
        let title = track.title().to_owned();
        if !flux_audio::decode::is_playable(&path) {
            return Err(format!(
                "\"{title}\" cannot be played: Flux plays WAV, FLAC, MP3, AAC, Ogg Vorbis, \
                 Opus, and M4A/MP4 (AAC) files. The file stays in the library, indexed."
            ));
        }
        let session = self.engine.load(id, path);
        self.engine_loaded = Some(id);
        self.engine_session = Some(session);
        self.stream_info = None;
        if autoplay {
            self.engine.play();
        }
        Ok(())
    }

    /// Common failure path: stop the model and the engine, forget the
    /// load, and tell the user. State is consistent afterwards.
    fn fail_playback(&mut self, message: String) {
        self.player.stop();
        self.engine.stop();
        self.engine_loaded = None;
        self.engine_session = None;
        self.stream_info = None;
        self.report_error(message);
    }

    fn begin_scan_roots(&mut self, roots: Vec<PathBuf>) {
        if self.scan_rx.is_some() {
            self.report_error("A library scan is already in progress.".to_owned());
            return;
        }
        if matches!(self.storage, StorageBackend::Canonical { .. }) {
            self.report_error(
                "Folder scanning is disabled for canonical libraries; use the Import Manager \
                 so every audio file is copied into the library."
                    .to_owned(),
            );
            return;
        }
        let scan_writes = match &self.storage {
            StorageBackend::Legacy { index_path } => index_path.is_some(),
            StorageBackend::Portable { .. } => self.portable_store.is_some(),
            StorageBackend::Canonical { .. } => false,
        };
        if scan_writes && !self.active_library_is_owned() {
            self.report_error(LIBRARY_NOT_OWNED.to_owned());
            return;
        }
        // A scan's completion always saves. If the existing legacy
        // index could not be read, running the scan would either make
        // no durable progress or tempt a later write to replace it;
        // refuse until the user explicitly preserves and replaces it.
        if self.legacy_index_write_protected {
            self.report_error(LEGACY_INDEX_WRITE_PROTECTED.to_owned());
            return;
        }
        // A recovered portable store is read-only, and a scan's
        // completion always saves — refuse up front rather than
        // scan successfully and then fail to persist.
        if self.portable_recovery.is_some() {
            self.report_error(PORTABLE_RECOVERY_READ_ONLY.to_owned());
            return;
        }
        // Validate before touching any state: a rejected scan must
        // leave the library, the session roots, and the saved index
        // exactly as they were. (The library layer would merely
        // itemize a bad root in its report; at this layer a mistyped
        // folder deserves a clear, immediate error. A root that
        // disappears *after* this check is still handled cleanly by
        // the scan itself, which itemizes it and continues.)
        let mut effective_roots = Vec::with_capacity(roots.len());
        for root in &roots {
            if let Err(message) = validate_scan_root(root) {
                self.report_error(message);
                return;
            }
            match self.portable_scan_root(root) {
                Ok(effective) => effective_roots.push(effective),
                Err(message) => {
                    self.report_error(message);
                    return;
                }
            }
        }
        let roots = effective_roots;
        let Some(library) = self.library.take() else {
            self.report_error("A library scan is already in progress.".to_owned());
            return;
        };
        for root in &roots {
            if !self.roots.contains(root) {
                self.roots.push(root.clone());
            }
        }
        self.report_status(format!(
            "Scanning {}…",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        // The backend — and, portably, the store itself — travels
        // with the work, so the worker can only save where this
        // controller was constructed to save.
        let persistence = match &self.storage {
            StorageBackend::Legacy { index_path } => match index_path {
                Some(path) => ScanPersistence::Legacy(path.clone()),
                None => ScanPersistence::None,
            },
            StorageBackend::Portable { .. } => match self.portable_store.take() {
                Some(store) => ScanPersistence::Portable(Box::new(store)),
                None => ScanPersistence::PortableUnavailable,
            },
            StorageBackend::Canonical { .. } => ScanPersistence::None,
        };
        let lease = self.library_lock.clone();
        let work = ScanWork {
            library,
            roots,
            persistence,
            lease,
        };
        let (work_tx, work_rx) = std::sync::mpsc::channel::<ScanWork>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<ScanOutcome>();
        // Spawn first and hand the library over only once the thread
        // exists, so a spawn failure cannot lose the collection.
        let spawned = std::thread::Builder::new()
            .name("flux-library-scan".to_owned())
            .spawn(move || {
                if let Ok(ScanWork {
                    mut library,
                    roots,
                    mut persistence,
                    lease,
                }) = work_rx.recv()
                {
                    // Bind (rather than `_`) so the lease lives until
                    // this closure returns, after the save attempt.
                    let _lease = lease;
                    let root_refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
                    let report = library.scan(&root_refs);
                    let (saved, sync_report) = match &mut persistence {
                        ScanPersistence::None => (None, None),
                        ScanPersistence::Legacy(path) => (Some(save_library(&library, path)), None),
                        ScanPersistence::Portable(store) => {
                            match persist_portable(&library, store) {
                                Ok(sync_report) => (Some(Ok(())), Some(sync_report)),
                                Err(message) => (Some(Err(message)), None),
                            }
                        }
                        ScanPersistence::PortableUnavailable => {
                            (Some(Err(PORTABLE_STORE_UNAVAILABLE.to_owned())), None)
                        }
                    };
                    let _ = done_tx.send(ScanOutcome {
                        library,
                        report,
                        persistence,
                        saved,
                        sync_report,
                    });
                }
            });
        match spawned {
            Ok(_handle) => match work_tx.send(work) {
                Ok(()) => {
                    self.scan_rx = Some(done_rx);
                }
                Err(err) => {
                    // The receiver is gone, so the scan will never run —
                    // but a failed `send` hands the payload back, so the
                    // library (which left `self` only for this handoff)
                    // is recovered whole instead of being discarded.
                    self.scan_start_failed(
                        err.0,
                        "The library scan could not be started.".to_owned(),
                    );
                }
            },
            Err(err) => {
                self.scan_start_failed(work, format!("Could not start the scan thread: {err}"));
            }
        }
    }

    /// Portable-mode scan-root gate: the folder must be the library
    /// root or inside it (canonically — a symlinked parent cannot
    /// smuggle an outside folder in, matching the scanner's
    /// never-follow-links stance), and the scan then runs under the
    /// canonical spelling so discovered paths share the loaded
    /// tracks' path identity. Legacy mode applies no extra rule.
    fn portable_scan_root(&self, root: &Path) -> Result<PathBuf, String> {
        let StorageBackend::Portable { root: library_root } = &self.storage else {
            return Ok(root.to_path_buf());
        };
        let canonical = std::fs::canonicalize(root)
            .map_err(|err| format!("Cannot scan \"{}\": {err}", root.display()))?;
        if canonical.starts_with(library_root) {
            Ok(canonical)
        } else {
            Err(format!(
                "Cannot scan \"{}\": it is outside this portable library's folder ({}). \
                 A portable library can only store files inside its own folder — \
                 scan that folder instead.",
                root.display(),
                library_root.display()
            ))
        }
    }

    /// Puts the library (and, portably, the store) back after a scan
    /// could not be started (the thread spawn or the work handoff
    /// failed). The collection is exactly as it was before the
    /// attempt and `scan_rx` stays `None`, so the controller is not
    /// left believing a scan is running; only the reported error is
    /// new.
    fn scan_start_failed(&mut self, work: ScanWork, message: String) {
        self.library = Some(work.library);
        if let ScanPersistence::Portable(store) = work.persistence {
            self.portable_store = Some(*store);
        }
        self.report_error(message);
    }

    fn finish_scan(&mut self, outcome: ScanOutcome) {
        let ScanOutcome {
            library,
            report: scan_report,
            persistence,
            saved,
            sync_report,
        } = outcome;
        self.library = Some(library);
        if let ScanPersistence::Portable(store) = persistence {
            self.portable_store = Some(*store);
        }

        // In portable mode the store is authoritative for effective
        // metadata after the scan's sync: it holds the newly read tag
        // snapshots *and* the user overrides, with the override
        // precedence applied by `StoredTrack::effective_metadata`.
        // Rebuilding here keeps overrides visible in the live session;
        // a legacy scan (or a portable scan whose store is unavailable)
        // uses the scanned library unchanged.
        let refresh_note = match self.rebuilt_from_store() {
            Some(report) => {
                let PortableRefreshReport {
                    library,
                    skipped_store_records,
                    dropped_memory_only,
                } = report;
                let note = refresh_skip_note(skipped_store_records, &dropped_memory_only);
                self.install_library_snapshot(library);
                note
            }
            None => {
                let library = self
                    .library
                    .take()
                    .expect("scan library was just installed");
                self.install_library_snapshot(library);
                None
            }
        };

        self.sync_default_queue();

        let mut message = format!(
            "Scan complete: {} added, {} updated, {} already indexed, {} skipped, {} failed. {} track(s) in the library.",
            scan_report.added(),
            scan_report.updated(),
            scan_report.deduplicated(),
            scan_report.skipped().len(),
            scan_report.failed().len(),
            self.tracks.len(),
        );
        if let Some(note) = refresh_note {
            message.push(' ');
            message.push_str(&note);
        }
        if let Some(sync_report) = &sync_report
            && !sync_report.skipped.is_empty()
        {
            message += &format!(
                " {} track(s) could not be stored in the portable library; they remain \
                 in memory for this session unless a path conflict is reported.",
                sync_report.skipped.len()
            );
        }
        self.report_status(message);
        if let Some(Err(message)) = saved {
            self.report_error(message);
        }
    }

    /// Recovers a library after the scan thread that owned it vanished
    /// without reporting back. The last saved state — legacy index or
    /// committed portable store — is the only faithful recovery
    /// source: rebuilding a `Library` from the display snapshot would
    /// renumber track IDs and break every queue reference. Returns
    /// whether a saved library was restored; otherwise installs an
    /// empty library, there being nothing the previous state could be
    /// recovered from. Uncommitted changes the lost thread had synced
    /// into the portable store are gone with it (exactly like an
    /// unsaved legacy scan); the committed generation is intact.
    fn recover_from_lost_scan(&mut self) -> bool {
        let restored = match &self.storage {
            StorageBackend::Legacy { index_path } => match index_path {
                Some(path) if path.exists() => Library::load_from_path(path).ok(),
                _ => None,
            },
            StorageBackend::Portable { root } => match PortableStore::open(root) {
                Ok(opened) => {
                    let (library, _skipped) = Library::from_portable_store(&opened.store);
                    self.portable_recovery = opened.recovery;
                    self.portable_store = Some(opened.store);
                    Some(library)
                }
                Err(_) => {
                    self.portable_store = None;
                    None
                }
            },
            StorageBackend::Canonical { root } => match CanonicalLibrary::open(root) {
                Ok(store) => {
                    let library = store.runtime_library();
                    self.canonical_store = Some(store);
                    Some(library)
                }
                Err(_) => {
                    self.canonical_store = None;
                    None
                }
            },
        };
        match restored {
            Some(library) => {
                self.tracks = snapshot(&library);
                self.library = Some(library);
                self.sync_default_queue();
                true
            }
            None => {
                self.library = Some(Library::new());
                self.tracks.clear();
                false
            }
        }
    }

    fn report_error(&mut self, message: String) {
        self.last_error = Some(message.clone());
        self.outbox.push(ControllerEvent::Error(message));
    }

    fn report_status(&mut self, message: String) {
        self.last_status = Some(message.clone());
        self.outbox.push(ControllerEvent::Status(message));
    }
}

fn snapshot(library: &Library) -> Vec<(TrackId, Track)> {
    library
        .iter()
        .map(|(id, track)| (id, track.clone()))
        .collect()
}

struct CopiedPath {
    path: PathBuf,
    was_copied: bool,
}

/// Copies one external source into the library root, never overwriting.
///
/// An existing file with identical bytes is reused (so importing the same
/// source repeatedly does not duplicate the catalogue); any other name
/// collision gets a numeric suffix. The source is only read. Keeping copies
/// at the root also keeps the portable folder-fallback metadata (`Artist/
/// Album/…`) from inventing an album named after an import staging folder.
fn copy_into_library(root: &Path, source: &Path) -> Result<CopiedPath, String> {
    let file_name = source
        .file_name()
        .ok_or_else(|| "the source has no file name".to_owned())?;
    let source_path = Path::new(file_name);
    let stem = source_path.file_stem().unwrap_or(file_name);
    let extension = source_path.extension();

    for attempt in 0..10_000u32 {
        let candidate_name = if attempt == 0 {
            file_name.to_os_string()
        } else {
            let mut name = OsString::from(stem);
            name.push(format!(" ({attempt})"));
            if let Some(extension) = extension {
                name.push(".");
                name.push(extension);
            }
            name
        };
        let candidate = root.join(candidate_name);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => {
                if files_equal(source, &candidate)? {
                    return Ok(CopiedPath {
                        path: candidate,
                        was_copied: false,
                    });
                }
                continue;
            }
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("could not inspect the copy destination: {err}")),
        }

        let mut input = std::fs::File::open(source)
            .map_err(|err| format!("could not read the source file: {err}"))?;
        let mut output = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("could not create the copy: {err}")),
        };
        if let Err(err) = std::io::copy(&mut input, &mut output).and_then(|_| output.sync_all()) {
            let _ = std::fs::remove_file(&candidate);
            return Err(format!("could not finish the copy: {err}"));
        }
        let canonical = std::fs::canonicalize(&candidate)
            .map_err(|err| format!("copied file cannot be resolved: {err}"))?;
        return Ok(CopiedPath {
            path: canonical,
            was_copied: true,
        });
    }
    Err("could not choose a collision-free copy name".to_owned())
}

fn files_equal(left: &Path, right: &Path) -> Result<bool, String> {
    let left_len = std::fs::metadata(left)
        .map_err(|err| format!("could not inspect {}: {err}", left.display()))?
        .len();
    let right_len = std::fs::metadata(right)
        .map_err(|err| format!("could not inspect {}: {err}", right.display()))?
        .len();
    if left_len != right_len {
        return Ok(false);
    }
    let mut left_file = std::fs::File::open(left)
        .map_err(|err| format!("could not read {}: {err}", left.display()))?;
    let mut right_file = std::fs::File::open(right)
        .map_err(|err| format!("could not read {}: {err}", right.display()))?;
    let mut left_buffer = [0u8; 8192];
    let mut right_buffer = [0u8; 8192];
    loop {
        let left_read = left_file
            .read(&mut left_buffer)
            .map_err(|err| format!("could not read {}: {err}", left.display()))?;
        let right_read = right_file
            .read(&mut right_buffer)
            .map_err(|err| format!("could not read {}: {err}", right.display()))?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn repeat_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    }
}

/// Checks that `root` can be scanned as a *folder*: it must exist and
/// be a directory. Checked with `symlink_metadata`, so a symbolic link
/// is reported as a link rather than silently followed — consistent
/// with the scanner's never-follow-links policy, which would skip a
/// link root anyway. The error is a complete user-facing message.
fn validate_scan_root(root: &Path) -> Result<(), String> {
    let shown = root.display();
    match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(meta) if meta.file_type().is_symlink() => Err(format!(
            "Cannot scan \"{shown}\": it is a symbolic link. Links are never \
             followed — scan the real folder instead."
        )),
        Ok(_) => Err(format!("Cannot scan \"{shown}\": it is not a folder.")),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "Cannot scan \"{shown}\": the folder does not exist."
        )),
        Err(err) => Err(format!("Cannot scan \"{shown}\": {err}")),
    }
}

/// Opens and validates a portable library candidate without
/// touching any controller: canonicalizes the root (so library
/// track paths and scan paths share one spelling — path identity
/// here is spelling-based by design), opens the store, and rebuilds
/// the in-memory library. Returns the canonical root, the opened
/// store, the library, and how many records this platform cannot
/// represent. Used identically by construction ([`Controller::new_portable`])
/// and by switching ([`Controller::switch_to_portable`]), so a
/// candidate is judged by exactly the same rules in both.
/// The queue file for a canonical legacy index: a sibling
/// `<index>.queue.json`, kept separate from the catalogue.
fn collect_audio_files(
    root: &Path,
    recursive: bool,
    files: &mut Vec<PathBuf>,
    issues: &mut Vec<ImportIssue>,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|err| format!("Cannot import {}: {err}", root.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "Cannot import {}: it is a symbolic link; links are never followed.",
            root.display()
        ));
    }
    if metadata.is_file() {
        if is_supported_audio_path(root) {
            files.push(root.to_path_buf());
        } else {
            issues.push(ImportIssue {
                path: root.to_path_buf(),
                reason: "unsupported or missing audio extension".to_owned(),
            });
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(format!(
            "Cannot import {}: it is not a file or folder.",
            root.display()
        ));
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|err| format!("Cannot list {}: {err}", dir.display()))?;
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    issues.push(ImportIssue {
                        path: dir.clone(),
                        reason: err.to_string(),
                    });
                    continue;
                }
            };
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(err) => {
                    issues.push(ImportIssue {
                        path,
                        reason: err.to_string(),
                    });
                    continue;
                }
            };
            if file_type.is_symlink() {
                issues.push(ImportIssue {
                    path,
                    reason: "symbolic links are not followed".to_owned(),
                });
            } else if file_type.is_dir() && recursive {
                stack.push(path);
            } else if file_type.is_file() {
                if is_supported_audio_path(&path) {
                    files.push(path);
                } else {
                    issues.push(ImportIssue {
                        path,
                        reason: "unsupported or missing audio extension".to_owned(),
                    });
                }
            }
        }
    }
    files.sort();
    Ok(())
}

/// Conservative filename/folder inference for quick imports. It never opens
/// the audio file or reads tags.
fn infer_canonical_import(path: &Path) -> Option<CanonicalImportedTrack> {
    let stem = path.file_stem()?.to_str()?.trim();
    if stem.is_empty() {
        return None;
    }
    let mut title = stem.to_owned();
    let mut track_number = None;
    let mut artist_name = None;
    // Numeric track prefix: "01 - Title", "01. Title", "01 Title".
    if let Some((prefix, rest)) = stem.split_once(['-', '.', ' '])
        && let Ok(number) = prefix.trim().parse::<u32>()
    {
        let rest = rest.trim().trim_start_matches(['-', '.', ' ']).trim();
        if !rest.is_empty() {
            track_number = Some(number);
            title = rest.to_owned();
        }
    }
    // "Artist - Title" remains after removing a numeric prefix.
    if let Some((left, right)) = title.split_once(" - ") {
        let left = left.trim();
        let right = right.trim();
        if !left.is_empty() && !right.is_empty() {
            artist_name = Some(left.to_owned());
            title = right.to_owned();
        }
    }
    let parent = path.parent();
    let album = parent
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
        .filter(|name| {
            !matches!(
                name.as_str(),
                "music" | "Music" | "audio" | "Audio" | "Downloads"
            )
        });
    if artist_name.is_none() {
        artist_name = parent
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(ToOwned::to_owned);
    }
    let artist_name = artist_name.unwrap_or_else(|| "Unknown Artist".to_owned());
    let album_title = album.unwrap_or_else(|| "Unknown Album".to_owned());
    Some(CanonicalImportedTrack {
        source: path.to_path_buf(),
        title,
        artist_id: None,
        artist_name,
        credited_artist_ids: Vec::new(),
        album_title: Some(album_title),
        release_type: ReleaseType::Album,
        track_number,
        disc_number: None,
        duration_ms: None,
    })
}

fn legacy_queue_path(canonical_index: &Path) -> PathBuf {
    let mut name = canonical_index.as_os_str().to_os_string();
    name.push(".queue.json");
    PathBuf::from(name)
}

/// The queue file for a portable library: inside a private subdirectory
/// of `.flux`, away from immutable segments and from the store's GC
/// patterns.
fn portable_queue_path(root: &Path) -> PathBuf {
    root.join(FLUX_DIR_NAME).join("queue").join("queue.json")
}

/// Queue persistence for a canonical library. This is application state, not
/// catalogue data, and never contains audio paths or metadata.
fn canonical_queue_path(root: &Path) -> PathBuf {
    root.join("queue.flux")
}

fn default_queue(library: &Library) -> Queue {
    let mut queue = Queue::new();
    for (id, _) in library.iter() {
        queue.push(id);
    }
    queue
}

/// Loads the queue for `library`, or derives the initial whole-library
/// queue when no queue file exists. Returns the queue, whether persistence
/// is write-protected, and a load error to report.
fn load_queue(
    store: Option<&QueueStore>,
    library: &Library,
) -> (Queue, bool, bool, Option<String>) {
    let Some(store) = store else {
        return (default_queue(library), true, false, None);
    };
    match store.load() {
        Ok(Some(loaded)) => (loaded.queue, loaded.auto_library, false, None),
        Ok(None) => (default_queue(library), true, false, None),
        Err(error) => (
            default_queue(library),
            true,
            true,
            Some(format!(
                "Could not load the saved playback queue ({}): {error}. \
                 The queue file is preserved; queue changes will not be saved \
                 during this session.",
                store.path().display()
            )),
        ),
    }
}

fn canonical_library_root(library_root: &Path) -> Result<PathBuf, CanonicalOpenError> {
    let invalid = |reason: String| CanonicalOpenError::RootInvalid {
        root: library_root.to_owned(),
        reason,
    };
    match std::fs::metadata(library_root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(invalid("it exists but is not a directory".to_owned())),
        Err(err) => {
            return Err(invalid(format!(
                "it does not exist or cannot be inspected: {err}"
            )));
        }
    }
    std::fs::canonicalize(library_root)
        .map_err(|err| invalid(format!("it cannot be canonicalized: {err}")))
}

fn canonical_portable_root(library_root: &Path) -> Result<PathBuf, PortableOpenError> {
    let root_invalid = |reason: String| PortableOpenError::RootInvalid {
        root: library_root.to_owned(),
        reason,
    };
    match std::fs::metadata(library_root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(root_invalid("it exists but is not a directory".to_owned())),
        Err(err) => {
            return Err(root_invalid(format!(
                "it does not exist or cannot be inspected: {err}"
            )));
        }
    }
    std::fs::canonicalize(library_root)
        .map_err(|err| root_invalid(format!("it cannot be canonicalized: {err}")))
}

/// Opens an already-canonical portable root under an exclusive lease.
/// The lease is returned to the caller so it can outlive construction
/// and be cloned into background workers.
fn open_portable_candidate_at(
    root: &Path,
) -> Result<(LibraryLock, OpenedStore, Library, usize), PortableOpenError> {
    let lock = LibraryLock::acquire_portable_root(root)?;
    let opened = PortableStore::open(root)?;
    let (library, skipped) = Library::from_portable_store(&opened.store);
    Ok((lock, opened, library, skipped))
}

fn open_portable_candidate(
    library_root: &Path,
) -> Result<(PathBuf, LibraryLock, OpenedStore, Library, usize), PortableOpenError> {
    let root = canonical_portable_root(library_root)?;
    let (lock, opened, library, skipped) = open_portable_candidate_at(&root)?;
    Ok((root, lock, opened, library, skipped))
}

/// The status text shown when some records of an opened portable
/// library cannot be represented on this platform.
fn skipped_unrepresentable_message(skipped: usize) -> String {
    format!(
        "{skipped} track(s) in this library cannot be shown on this platform \
         because their file paths are not representable here. They are \
         preserved unchanged in the library files."
    )
}

/// A single status sentence for the parts of a store refresh that could
/// not appear in the runtime library: unrepresentable store records and
/// memory-only tracks whose path a persistent record now owns.
fn refresh_skip_note(
    skipped_store_records: usize,
    dropped_memory_only: &[TrackId],
) -> Option<String> {
    let mut notes = Vec::new();
    if skipped_store_records > 0 {
        notes.push(skipped_unrepresentable_message(skipped_store_records));
    }
    if !dropped_memory_only.is_empty() {
        notes.push(format!(
            "{} in-memory track(s) could not be kept because the portable library \
             already owns their path; the persistent record won.",
            dropped_memory_only.len()
        ));
    }
    (!notes.is_empty()).then(|| notes.join(" "))
}

/// The portable save: sync the in-memory library into the store,
/// then commit through the store's own publication protocol. The
/// two failure stages get distinct messages; either way the synced
/// state stays pending inside the (controller-owned) store, so the
/// next save retries the same changes instead of losing them.
fn persist_portable(
    library: &Library,
    store: &mut PortableStore,
) -> Result<PortableSyncReport, String> {
    let report = sync_library_to_portable_store(library, store)
        .map_err(|err| format!("Could not save the portable library: {err}"))?;
    store.commit().map_err(|err| {
        format!(
            "Could not save the portable library while committing it: {err}. \
             The changes are kept in memory; saving again will retry."
        )
    })?;
    Ok(report)
}

/// A non-existing sibling path that preserves an unreadable legacy
/// index without overwriting an earlier preserved copy.
fn preserved_legacy_index_path(index_path: &Path) -> Result<PathBuf, String> {
    for attempt in 0..1_000u32 {
        let mut name = index_path.as_os_str().to_os_string();
        if attempt == 0 {
            name.push(".unreadable");
        } else {
            name.push(format!(".unreadable-{}", attempt + 1));
        }
        let candidate = PathBuf::from(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "Could not find an unused preservation name next to {}; the original file was left untouched.",
        index_path.display()
    ))
}

/// Whether two library locations name the same file/root. Used only to
/// make "open the already-active library" a friendly no-op instead of a
/// self-conflict with our own lock.
fn same_library_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return a == b;
    }
    match (std::path::absolute(a), std::path::absolute(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Creates parent directories, then writes the versioned index
/// (flushing the "parent must exist" precondition of
/// `Library::save_to_path`).
fn save_library(library: &Library, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    library
        .save_to_path(path)
        .map_err(|err| format!("Could not save the library index: {err}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use flux_audio::engine::{EngineError, EngineEvent, EngineHandle};
    use flux_audio::output::NullControl;
    use flux_core::{PlaybackState, Queue, QueueEntryId, RepeatMode, Track, TrackId};
    use flux_library::QueueStore;

    use super::{
        AudioStatus, Controller, ControllerEvent, CreateLibraryError, PortableOpenError,
        ScanOutcome, StorageMode, SwitchError, TrackMetadataEdit, create_canonical_library,
        create_portable_library, default_engine, execute_legacy_migration, is_track_playable,
        migrate_legacy_library, plan_legacy_migration,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-app-test-{}-{test}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Mono 44.1 kHz 16-bit WAV of constant DC — sample accounting is
    /// exact and amplitude reveals the applied gain.
    fn write_dc_wav(dir: &Path, name: &str, frames: usize, value: f32) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create wav");
        let sample = (value * 32_767.0) as i32;
        for _ in 0..frames {
            writer.write_sample(sample).expect("write sample");
        }
        writer.finalize().expect("finalize");
        path
    }

    /// Standard fixture set: two playable WAVs, one indexed-but-not-
    /// playable AIFF, one corrupt WAV, in `<dir>/music`.
    struct Fixtures {
        root: PathBuf,
        a: PathBuf,
        b: PathBuf,
    }

    fn fixtures(dir: &TempDir) -> Fixtures {
        let root = dir.path().join("music");
        fs::create_dir_all(&root).expect("music dir");
        let a = write_dc_wav(&root, "a.wav", 11_025, 0.4);
        let b = write_dc_wav(&root, "b.wav", 11_025, 0.3);
        fs::write(root.join("c.aif"), b"not really aiff").expect("aiff bytes");
        fs::write(root.join("d.wav"), b"not really wav").expect("corrupt wav");
        Fixtures { root, a, b }
    }

    fn boot(dir: &TempDir) -> (Controller, NullControl) {
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let index = dir.path().join("state").join("library-index.json");
        (
            Controller::new(engine, AudioStatus::Ready, Some(index))
                .expect("controller opens the test library"),
            control,
        )
    }

    fn legacy_queue_file(dir: &TempDir) -> PathBuf {
        dir.path()
            .join("state")
            .join("library-index.json.queue.json")
    }

    fn portable_queue_file(root: &Path) -> PathBuf {
        root.join(".flux").join("queue").join("queue.json")
    }

    fn queue_snapshot(
        controller: &Controller,
    ) -> (Vec<(QueueEntryId, TrackId)>, Option<QueueEntryId>) {
        (
            controller
                .queue_entries()
                .iter()
                .map(|entry| (entry.id(), entry.track()))
                .collect(),
            controller.current_queue_entry_id(),
        )
    }

    /// Convergence driver: drains the null device deterministically and
    /// polls until `done` observes the controller/events it wants.
    /// Every iteration makes real progress (512 device frames + scan/
    /// engine events), so the cap is a safety net, not a timing guess.
    fn drive(
        controller: &mut Controller,
        control: &NullControl,
        done: impl Fn(&Controller, &[ControllerEvent]) -> bool,
    ) -> Vec<ControllerEvent> {
        let mut seen = Vec::new();
        for _ in 0..4_000 {
            control.drain_frames(512);
            let events = controller.poll();
            let finished = done(controller, &events);
            seen.extend(events);
            if finished {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        seen
    }

    fn scanned(controller: &mut Controller, control: &NullControl, root: PathBuf) {
        controller.begin_scan(root);
        let events = drive(controller, control, |c, _| !c.is_scanning());
        assert!(
            events.contains(&ControllerEvent::LibraryChanged),
            "expected LibraryChanged, got {events:?}"
        );
    }

    fn id_of(controller: &Controller, path: &Path) -> TrackId {
        controller
            .tracks()
            .iter()
            .find(|(_, t)| t.source().path() == path)
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("track for {} indexed", path.display()))
    }

    /// A mono 16-bit tone WAV (440 Hz cosine): unlike the DC
    /// fixtures, any lost, duplicated, or reordered sample changes
    /// the content, so exact-sequence comparison can see it.
    fn write_tone_wav(dir: &Path, name: &str, frames: usize) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create wav");
        for n in 0..frames {
            let t = n as f64 / 44_100.0;
            let v = 0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).cos();
            writer
                .write_sample((v * 32_767.0) as i32)
                .expect("write sample");
        }
        writer.finalize().expect("finalize");
        path
    }

    /// The GUI's exact runtime shape — `play_track`, then a loop of
    /// "device drains, `controller.poll()`" (its 150 ms Tick), with
    /// a library scan completing mid-playback — must deliver the
    /// track sample-for-sample, bracketed only by silence. This is
    /// the end-to-end pin on the whole controller path the GUI
    /// drives: any load/stop reaching the engine mid-track would
    /// flush buffered audio and break the sequence, and any starved
    /// callback fill would insert zeros inside it — either failure
    /// (heard by users as cuts and jumps) shows up here first.
    #[test]
    fn gui_style_drive_loop_is_sample_exact_and_never_reloads() {
        let dir = TempDir::new("gui-loop");
        let root = dir.path().join("music");
        fs::create_dir_all(&root).expect("music dir");
        let tone_path = write_tone_wav(&root, "tone.wav", 44_100 * 2);
        // A second folder whose scan will complete during playback.
        let other = dir.path().join("other");
        fs::create_dir_all(&other).expect("other dir");
        write_dc_wav(&other, "extra.wav", 4_410, 0.2);

        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, root);
        let id = id_of(&controller, &tone_path);

        // Reference: direct decode of the same file. The null device
        // matches the file format (44.1 kHz mono), so engine
        // conversion is the identity and delivered samples must
        // equal decoded samples exactly.
        let reference = {
            let mut decoder = flux_audio::decode::open_decoder(&tone_path).expect("opens");
            let mut all = Vec::new();
            loop {
                let chunk = decoder.next_chunk(4096).expect("decodes");
                if chunk.is_empty() {
                    break;
                }
                all.extend(chunk);
            }
            all
        };
        assert_eq!(reference.len(), 44_100 * 2);

        control.clear_captured();
        controller.play_track(id);
        // Wait until audio is flowing, so what follows really is
        // mid-playback rather than startup.
        drive(&mut controller, &control, |_, _| {
            control.captured().iter().any(|s| *s != 0.0)
        });
        // A scan completing mid-playback must not disturb it.
        controller.begin_scan(other);
        // Position is derived from delivered frames; the identity
        // conversion emits exactly the input count, so the 2 s track
        // ends with position exactly 2 s and everything captured.
        drive(&mut controller, &control, |c, _| {
            !c.is_scanning() && c.position() >= Duration::from_secs(2)
        });
        // Collect a little post-track device time (must be silence).
        for _ in 0..4 {
            control.drain_frames(512);
            controller.poll();
        }

        let captured = control.captured();
        let start = captured
            .windows(64)
            .position(|w| w == &reference[..64])
            .expect("track head appears in the capture");
        assert!(
            captured[..start].iter().all(|s| *s == 0.0),
            "only silence may precede the track"
        );
        assert!(
            captured.len() >= start + reference.len(),
            "capture holds fewer samples than the decoded track"
        );
        assert_eq!(
            &captured[start..start + reference.len()],
            reference.as_slice(),
            "delivered samples diverge from direct decode"
        );
        assert!(
            captured[start + reference.len()..]
                .iter()
                .all(|s| *s == 0.0),
            "only silence may follow the track"
        );
        controller.shutdown();
    }

    #[test]
    fn scan_lists_tracks_and_persists_stable_ids() {
        let dir = TempDir::new("scan");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);

        assert!(controller.tracks().is_empty());
        scanned(&mut controller, &control, fx.root.clone());
        // All four files are indexed (even the corrupt one and the
        // MP3) — indexed does not mean playable.
        assert_eq!(
            controller.tracks().len(),
            4,
            "{:?}",
            controller.last_status()
        );
        assert_eq!(
            controller
                .tracks()
                .iter()
                .filter(|(_, t)| is_track_playable(t))
                .count(),
            3
        );
        assert!(
            controller
                .last_status()
                .is_some_and(|s| s.contains("Scan complete"))
        );

        // Re-scan deduplicates; IDs stay stable through a reload.
        let ids_before: Vec<TrackId> = controller.tracks().iter().map(|(id, _)| *id).collect();
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(controller.tracks().len(), 4);
        controller.shutdown();

        let (controller2, _control2) = boot(&dir);
        let ids_after: Vec<TrackId> = controller2.tracks().iter().map(|(id, _)| *id).collect();
        assert_eq!(ids_after, ids_before, "persisted IDs must be stable");
        assert_eq!(id_of(&controller2, &fx.a).get(), ids_before[0].get());
        controller2.shutdown();
    }

    #[test]
    fn nonexistent_scan_folder_is_rejected_and_preserves_state() {
        let dir = TempDir::new("scan-missing-root");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(controller.tracks().len(), 4);
        let index_path = dir.path().join("state").join("library-index.json");
        let index_before = fs::read(&index_path).expect("scan saved the index");
        let current = controller.current_id();
        assert!(current.is_some());

        let missing = dir.path().join("no-such-folder");
        controller.begin_scan(missing.clone());

        // Rejected synchronously: clear error, no scan started.
        assert!(!controller.is_scanning());
        let error = controller.last_error().expect("error reported").to_owned();
        assert!(error.contains("does not exist"), "{error}");
        assert!(error.contains(&missing.display().to_string()), "{error}");
        let events = controller.poll();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(_))),
            "{events:?}"
        );
        assert!(
            !events.contains(&ControllerEvent::LibraryChanged),
            "a rejected scan must not signal a library change: {events:?}"
        );

        // Nothing else moved: tracks, selection, playback state, index.
        assert_eq!(controller.tracks().len(), 4);
        assert_eq!(controller.current_id(), current);
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert_eq!(
            fs::read(&index_path).expect("index still readable"),
            index_before,
            "a rejected scan must not rewrite the index"
        );

        // The bad root was not remembered: refresh re-scans only the
        // good root and completes normally.
        controller.refresh();
        assert!(controller.is_scanning());
        drive(&mut controller, &control, |c, _| !c.is_scanning());
        assert_eq!(controller.tracks().len(), 4);
        controller.shutdown();
    }

    #[test]
    fn regular_file_as_scan_folder_is_rejected() {
        let dir = TempDir::new("scan-file-root");
        let fx = fixtures(&dir);
        let (mut controller, _control) = boot(&dir);

        controller.begin_scan(fx.a.clone());
        assert!(!controller.is_scanning());
        let error = controller.last_error().expect("error reported").to_owned();
        assert!(error.contains("is not a folder"), "{error}");
        assert!(controller.tracks().is_empty());
        // No scan ran, so no index was written...
        let index_path = dir.path().join("state").join("library-index.json");
        assert!(
            !index_path.exists(),
            "a rejected scan must not create an index"
        );
        // ...and the path was not remembered as a session root.
        controller.refresh();
        assert!(
            controller
                .last_status()
                .is_some_and(|s| s.contains("Nothing to refresh yet")),
            "{:?}",
            controller.last_status()
        );
        controller.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_scan_folder_is_rejected_without_following_it() {
        let dir = TempDir::new("scan-symlink-root");
        let fx = fixtures(&dir);
        let link = dir.path().join("music-link");
        std::os::unix::fs::symlink(&fx.root, &link).expect("create symlink");
        let (mut controller, _control) = boot(&dir);

        controller.begin_scan(link);
        assert!(!controller.is_scanning());
        let error = controller.last_error().expect("error reported").to_owned();
        assert!(error.contains("symbolic link"), "{error}");
        assert!(controller.tracks().is_empty());
        controller.shutdown();
    }

    #[test]
    fn play_pause_resume_and_stop_cycle() {
        let dir = TempDir::new("cycle");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);

        controller.play_track(a);
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.current_id(), Some(a));
        drive(&mut controller, &control, |c, _| {
            c.position() > Duration::from_millis(30)
        });
        let nonzero_before = control
            .captured()
            .iter()
            .filter(|s| s.abs() > 0.001)
            .count();
        assert!(nonzero_before > 0);

        controller.pause();
        assert_eq!(controller.player_state(), PlaybackState::Paused);
        // Pause reaches the engine asynchronously; converge on
        // *observed* silence (three silent drains in a row) rather
        // than guessing which control event marks its arrival.
        let mut silent_streak = 0;
        while silent_streak < 3 {
            control.clear_captured();
            control.drain_frames(512);
            controller.poll();
            if control.captured().iter().all(|s| s.abs() <= 0.001) {
                silent_streak += 1;
            } else {
                silent_streak = 0;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        control.clear_captured();
        for _ in 0..8 {
            control.drain_frames(512);
            controller.poll();
        }
        assert_eq!(
            control
                .captured()
                .iter()
                .filter(|s| s.abs() > 0.001)
                .count(),
            0,
            "paused means silence"
        );

        controller.toggle_play_pause();
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        // Drain to the end: A advances into B, B advances onto the MP3,
        // which errors and stops (covered in detail by its own test).
        drive(&mut controller, &control, |c, _| {
            c.player_state() == PlaybackState::Stopped
        });

        controller.play_track(a);
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.stop();
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        // Position resets on the first device fill after the engine
        // processes Stop — converge rather than assume it landed.
        drive(&mut controller, &control, |c, _| {
            c.position() == Duration::ZERO
        });
        assert_eq!(controller.position(), Duration::ZERO);
        controller.shutdown();
    }

    #[test]
    fn track_end_advances_through_queue_then_stops() {
        let dir = TempDir::new("advance");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let b = id_of(&controller, &fx.b);

        // From A, the queue's next playable-advance target is B
        // (library order: a, b, c.aif, d.wav follow scan order).
        controller.play_track(a);
        drive(&mut controller, &control, |c, _| {
            c.current_id() == Some(b) && c.player_state() == PlaybackState::Playing
        });
        assert_eq!(
            controller.current_id(),
            Some(b),
            "end of A must advance to B"
        );

        // B ends → next in queue is the AIFF → clear error, stopped,
        // consistent (no silent skip, no stuck Playing).
        let events = drive(&mut controller, &control, |c, ev| {
            c.player_state() == PlaybackState::Stopped
                && ev.iter().any(|e| matches!(e, ControllerEvent::Error(_)))
        });
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(m) if m.contains("cannot be played"))),
            "auto-advance onto an unplayable format must error clearly: {events:?}"
        );
        controller.shutdown();
    }

    #[test]
    fn compressed_formats_play_and_advance_through_the_controller() {
        // Real encoded fixtures from the flux-audio test suite
        // (provenance: crates/flux-audio/tests/fixtures/README.md).
        const OGG: &[u8] = include_bytes!("../../flux-audio/tests/fixtures/tone.ogg");
        const M4A: &[u8] = include_bytes!("../../flux-audio/tests/fixtures/tone.m4a");
        const MP3: &[u8] = include_bytes!("../../flux-audio/tests/fixtures/silence.mp3");
        let dir = TempDir::new("formats");
        let root = dir.path().join("music");
        fs::create_dir_all(&root).expect("music dir");
        fs::write(root.join("tone.ogg"), OGG).expect("ogg fixture");
        fs::write(root.join("tone.m4a"), M4A).expect("m4a fixture");
        fs::write(root.join("silence.mp3"), MP3).expect("mp3 fixture");
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, root.clone());

        // Play each compressed track in library order: every one must
        // reach Playing with a real stream info and produce no error,
        // then end and auto-advance (or stop after the last).
        let order: Vec<TrackId> = controller.tracks().iter().map(|(id, _)| *id).collect();
        assert_eq!(order.len(), 3, "all three fixtures indexed");
        for track in controller.tracks() {
            assert!(
                super::is_track_playable(&track.1),
                "{} playable",
                track.1.title()
            );
        }
        for (i, id) in order.iter().enumerate() {
            controller.play_track(*id);
            let events = drive(&mut controller, &control, |c, _| {
                c.current_id() == Some(*id)
                    && c.player_state() == PlaybackState::Playing
                    && c.stream_info().is_some()
            });
            assert!(
                events
                    .iter()
                    .all(|e| !matches!(e, ControllerEvent::Error(_))),
                "no playback errors expected: {events:?}"
            );
            // Let it run to completion.
            let is_last = i + 1 == order.len();
            let events = drive(&mut controller, &control, |c, _| {
                if is_last {
                    c.player_state() == PlaybackState::Stopped
                } else {
                    c.current_id() == Some(order[i + 1])
                }
            });
            assert!(
                events
                    .iter()
                    .all(|e| !matches!(e, ControllerEvent::Error(_))),
                "EOF advancement must not error: {events:?}"
            );
        }
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        controller.shutdown();
    }

    #[test]
    fn next_and_previous_move_selection_and_restart_playback() {
        let dir = TempDir::new("skip");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let b = id_of(&controller, &fx.b);

        controller.play_track(a);
        drive(&mut controller, &control, |c, _| {
            c.position() > Duration::from_millis(20)
        });
        controller.next();
        assert_eq!(controller.current_id(), Some(b));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.previous();
        assert_eq!(controller.current_id(), Some(a));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.shutdown();
    }

    #[test]
    fn volume_is_owned_by_the_controller_and_applied() {
        let dir = TempDir::new("volume");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);

        controller.set_volume(0.5);
        assert_eq!(controller.volume(), 0.5);
        controller.adjust_volume(0.1);
        assert!((controller.volume() - 0.6).abs() < 1e-6);
        controller.set_volume(2.5);
        assert_eq!(controller.volume(), 1.0, "clamped");
        controller.set_volume(0.5);

        controller.play_track(a);
        drive(&mut controller, &control, |c, _| {
            c.position() > Duration::from_millis(50)
        });
        let peak = control
            .captured()
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - 0.2).abs() < 0.02,
            "0.4 DC at gain 0.5 should peak ≈0.2, got {peak}"
        );
        controller.shutdown();
    }

    #[test]
    fn unsupported_format_is_a_clear_error_and_state_stays_consistent() {
        let dir = TempDir::new("unsupported");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let aif = id_of(&controller, &fx.root.join("c.aif"));

        controller.play_track(aif);
        let events = controller.poll();
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(m) if m.contains("cannot be played"))),
            "{events:?}"
        );
        let a = id_of(&controller, &fx.a);
        controller.play_track(a);
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.shutdown();
    }

    #[test]
    fn missing_and_corrupt_files_surface_errors_without_crashing() {
        let dir = TempDir::new("badfiles");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let corrupt = id_of(&controller, &fx.root.join("d.wav"));

        // Corrupt: opens as .wav, fails to decode → async engine error.
        controller.play_track(corrupt);
        let events = drive(&mut controller, &control, |_, ev| {
            ev.iter().any(|e| matches!(e, ControllerEvent::Error(_)))
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(m) if m.contains("Playback failed"))),
            "{events:?}"
        );
        assert_eq!(controller.player_state(), PlaybackState::Stopped);

        // Missing: file deleted after indexing (never pruned), play
        // reports the IO failure and stops cleanly.
        fs::remove_file(&fx.a).expect("remove file");
        controller.play_track(a);
        let events = drive(&mut controller, &control, |_, ev| {
            ev.iter().any(|e| matches!(e, ControllerEvent::Error(_)))
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(_)))
        );
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert!(
            controller.track(a).is_some(),
            "missing files are not pruned"
        );
        controller.shutdown();
    }

    #[test]
    fn stale_engine_errors_never_stop_current_playback() {
        let dir = TempDir::new("stale-errors");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let b = id_of(&controller, &fx.b);

        // Model a current Playing session on track B without starting
        // the real worker; event delivery is then fully controlled.
        controller.player.play_track(b).expect("select b");
        controller.engine_loaded = Some(b);
        controller.engine_session = Some(77);

        let mut events = Vec::new();
        controller.handle_engine_event(
            EngineEvent::Error {
                session: Some(76),
                id: Some(a),
                error: EngineError::Output("old track failed".to_owned()),
            },
            &mut events,
        );
        assert!(events.is_empty());
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.current_id(), Some(b));
        assert_eq!(controller.engine_session, Some(77));
        assert!(controller.last_error().is_none());

        // Earlier attempt for the *same* track: still stale.
        controller.handle_engine_event(
            EngineEvent::Error {
                session: Some(76),
                id: Some(b),
                error: EngineError::Output("earlier attempt failed".to_owned()),
            },
            &mut events,
        );
        assert!(events.is_empty());
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.current_id(), Some(b));
        assert_eq!(controller.engine_session, Some(77));
        assert!(controller.last_error().is_none());

        // A genuine current-session failure is reported and stopped.
        controller.handle_engine_event(
            EngineEvent::Error {
                session: Some(77),
                id: Some(b),
                error: EngineError::Output("current session failed".to_owned()),
            },
            &mut events,
        );
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert_eq!(controller.engine_loaded, None);
        assert_eq!(controller.engine_session, None);
        assert!(
            controller
                .last_error()
                .is_some_and(|message| message.contains("Playback failed")),
            "{:?}",
            controller.last_error()
        );
        assert!(events.contains(&ControllerEvent::PlaybackChanged));
        controller.shutdown();
    }

    #[test]
    fn global_audio_errors_stop_an_active_session() {
        let dir = TempDir::new("global-audio-error");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        controller.player.play_track(a).expect("select a");
        controller.engine_loaded = Some(a);
        controller.engine_session = Some(5);

        let mut events = Vec::new();
        controller.handle_engine_event(
            EngineEvent::Error {
                session: None,
                id: None,
                error: EngineError::Output("device disappeared".to_owned()),
            },
            &mut events,
        );
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert_eq!(controller.engine_session, None);
        assert!(
            controller
                .last_error()
                .is_some_and(|message| message.contains("device disappeared")),
            "{:?}",
            controller.last_error()
        );
        controller.shutdown();
    }

    #[test]
    fn stale_track_end_does_not_advance_but_current_end_does() {
        let dir = TempDir::new("stale-end");
        let root = two_track_dir(&dir, "music");
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, root);
        let ids = track_ids(&controller);
        let a = TrackId::new(ids[0]);
        let b = TrackId::new(ids[1]);
        controller.player.play_track(a).expect("select a");
        controller.engine_loaded = Some(a);
        controller.engine_session = Some(10);

        let mut events = Vec::new();
        controller.handle_engine_event(EngineEvent::TrackEnded { session: 9, id: a }, &mut events);
        assert_eq!(controller.current_id(), Some(a));
        assert_eq!(controller.player_state(), PlaybackState::Playing);

        controller.handle_engine_event(EngineEvent::TrackEnded { session: 10, id: a }, &mut events);
        assert_eq!(controller.current_id(), Some(b));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.shutdown();
    }

    #[test]
    fn failed_load_then_new_track_plays_without_a_delayed_error() {
        let dir = TempDir::new("failed-load-then-play");
        let root = dir.path().join("music");
        fs::create_dir_all(&root).expect("music dir");
        let bad = root.join("bad.wav");
        fs::write(&bad, b"not a real wav").expect("bad fixture");
        let good = write_dc_wav(&root, "good.wav", 441_000, 0.4);

        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, root);
        let bad_id = id_of(&controller, &bad);
        let good_id = id_of(&controller, &good);

        controller.play_track(bad_id);
        controller.play_track(good_id);
        let events = drive(&mut controller, &control, |controller, _| {
            controller.stream_info().is_some()
                && controller.player_state() == PlaybackState::Playing
                && controller.current_id() == Some(good_id)
        });

        assert_eq!(controller.current_id(), Some(good_id));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert!(
            controller.last_error().is_none(),
            "the old load failure must not surface: {:?}",
            controller.last_error()
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ControllerEvent::Error(_))),
            "stale/duplicate errors reached the UI: {events:?}"
        );
        controller.shutdown();
    }

    #[test]
    fn audio_status_distinguishes_build_and_device_failures() {
        assert!(AudioStatus::Ready.is_ready());
        assert!(AudioStatus::Ready.reason().is_none());

        let build = AudioStatus::BuildWithoutBackend;
        assert!(!build.is_ready());
        assert!(
            build
                .reason()
                .is_some_and(|reason| reason.contains("compiled without"))
        );

        let device = AudioStatus::DeviceUnavailable("no output device".to_owned());
        assert!(!device.is_ready());
        assert_eq!(device.reason(), Some("no output device"));
    }

    #[cfg(not(feature = "cpal"))]
    #[test]
    fn default_engine_does_not_claim_ready_without_a_backend() {
        let (engine, status) = default_engine();
        assert_eq!(status, AudioStatus::BuildWithoutBackend);
        assert!(!status.is_ready());
        engine.shutdown();
    }

    #[test]
    fn unavailable_audio_refuses_playback_but_still_browses() {
        let dir = TempDir::new("noaudio");
        let fx = fixtures(&dir);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let mut controller = Controller::new(
            engine,
            AudioStatus::DeviceUnavailable("no output device in this test".to_owned()),
            None,
        )
        .expect("session-only controller");
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(controller.tracks().len(), 4, "browsing works without audio");

        let a = id_of(&controller, &fx.a);
        controller.play_track(a);
        let events = controller.poll();
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(m) if m.contains("no output device")))
        );
        control.drain_frames(1024);
        assert_eq!(
            control
                .captured()
                .iter()
                .filter(|s| s.abs() > 0.001)
                .count(),
            0,
            "nothing may 'play' silently"
        );
        controller.shutdown();
    }

    #[test]
    fn repeat_one_replays_and_refresh_without_roots_is_a_status() {
        let dir = TempDir::new("repeat");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);

        controller.refresh();
        let events = controller.poll();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Status(_)))
        );
        assert!(controller.last_error().is_none());

        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        controller.set_repeat_mode(RepeatMode::One);
        controller.play_track(a);
        // Two full plays of A (11_025 frames each) must pass through.
        drive(&mut controller, &control, |_, _| {
            control
                .captured()
                .iter()
                .filter(|s| s.abs() > 0.001)
                .count()
                >= 22_050
        });
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.current_id(), Some(a));
        controller.shutdown();
    }

    #[test]
    fn shutdown_during_scan_is_clean() {
        let dir = TempDir::new("shutdown");
        let fx = fixtures(&dir);
        let (mut controller, _control) = boot(&dir);
        controller.begin_scan(fx.root.clone());
        controller.shutdown(); // joins the scan worker; must not hang
        let (controller2, _control2) = boot(&dir);
        assert_eq!(
            controller2.tracks().len(),
            4,
            "scan saved its index before shutdown"
        );
        controller2.shutdown();
    }

    /// S1 regression: a scan whose work handoff fails (the receiving
    /// end is gone) must hand the library back intact — never replace
    /// it with an empty one. Reproduces the exact production sequence
    /// deterministically: take the library, attempt the `send` against
    /// a dropped receiver, recover the payload from `SendError`, and
    /// run it through the same handler `begin_scan_roots` uses for both
    /// its spawn-failure and send-failure arms.
    #[test]
    fn failed_scan_handoff_restores_the_library() {
        let dir = TempDir::new("handoff_failure");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let before: Vec<_> = controller
            .tracks()
            .iter()
            .map(|(id, t)| (*id, t.source().path().to_owned()))
            .collect();
        assert_eq!(before.len(), 4);
        assert!(controller.last_error().is_none());

        let (work_tx, work_rx) = std::sync::mpsc::channel::<super::ScanWork>();
        drop(work_rx);
        let library = controller.library.take().expect("library present");
        let err = work_tx
            .send(super::ScanWork {
                library,
                roots: vec![fx.root.clone()],
                persistence: super::ScanPersistence::None,
                lease: None,
            })
            .expect_err("send to a dropped receiver must fail");
        controller.scan_start_failed(err.0, "The library scan could not be started.".to_owned());

        // The collection is exactly as before the attempt — same IDs,
        // same paths — the failure is reported as an error (not a false
        // success), and no scan is believed to be running.
        let after: Vec<_> = controller
            .tracks()
            .iter()
            .map(|(id, t)| (*id, t.source().path().to_owned()))
            .collect();
        assert_eq!(after, before);
        assert!(controller.library.is_some(), "library must be restored");
        assert!(!controller.is_scanning());
        assert_eq!(
            controller.last_error(),
            Some("The library scan could not be started.")
        );
        let events = controller.poll();
        assert!(
            events.contains(&ControllerEvent::Error(
                "The library scan could not be started.".to_owned()
            )),
            "the failure must reach the UI as an error event: {events:?}"
        );

        // The controller is undamaged: a real scan still works and
        // finds the same library.
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(controller.tracks().len(), 4);
        controller.shutdown();
    }

    /// S1 regression: if the scan thread vanishes while owning the
    /// library (its result channel disconnects), `poll` must restore
    /// the library from the saved index — with the same stable IDs —
    /// instead of installing an empty library.
    #[test]
    fn lost_scan_thread_recovers_library_from_saved_index() {
        let dir = TempDir::new("lost_scan_recovery");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let before: Vec<_> = controller
            .tracks()
            .iter()
            .map(|(id, t)| (*id, t.source().path().to_owned()))
            .collect();
        assert_eq!(before.len(), 4, "scan indexed and saved the fixtures");

        // Simulate the lost thread deterministically: the library is
        // gone with it (`None` here) and the result channel is
        // disconnected.
        let (done_tx, done_rx) = std::sync::mpsc::channel::<ScanOutcome>();
        drop(done_tx);
        controller.scan_rx = Some(done_rx);
        controller.library = None;
        let events = controller.poll();

        let after: Vec<_> = controller
            .tracks()
            .iter()
            .map(|(id, t)| (*id, t.source().path().to_owned()))
            .collect();
        assert_eq!(after, before, "IDs and paths restored from the saved index");
        assert!(controller.library.is_some());
        assert!(!controller.is_scanning());
        let error = controller.last_error().expect("failure reported");
        assert!(
            error.contains("restored from the saved index"),
            "got: {error}"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(_))),
            "got: {events:?}"
        );
        // Fully usable again — for example, it can save its index.
        controller.save_index().expect("restored library can save");
        controller.shutdown();
    }

    /// The honest floor of the same failure: with no saved index to
    /// restore from, a lost scan thread leaves an empty library — but
    /// the failure is reported as a failure, never as success.
    #[test]
    fn lost_scan_thread_without_saved_index_reports_and_starts_empty() {
        let dir = TempDir::new("lost_scan_no_index");
        let fx = fixtures(&dir);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let mut controller =
            Controller::new(engine, AudioStatus::Ready, None).expect("session-only controller");
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(controller.tracks().len(), 4);

        let (done_tx, done_rx) = std::sync::mpsc::channel::<ScanOutcome>();
        drop(done_tx);
        controller.scan_rx = Some(done_rx);
        controller.library = None;
        let events = controller.poll();

        assert!(controller.tracks().is_empty());
        assert!(controller.library.is_some(), "empty fallback installed");
        assert!(!controller.is_scanning());
        let error = controller.last_error().expect("failure reported");
        assert!(error.contains("no saved index"), "got: {error}");
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(_))),
            "got: {events:?}"
        );
        controller.shutdown();
    }

    // ------------------------------------------------- S4 storage ----

    fn imported_record(
        id: u64,
        path: &str,
        availability: flux_library::Availability,
        title: &str,
    ) -> flux_library::ImportedTrack {
        use flux_library::{MetadataOverrides, RelPath, TagSet};
        flux_library::ImportedTrack {
            id: TrackId::new(id),
            path: RelPath::from_utf8(path).expect("relative fixture path"),
            availability,
            file: None,
            tags: TagSet {
                title: Some(title.to_owned()),
                ..TagSet::default()
            },
            overrides: MetadataOverrides::new(),
            artist_id: None,
            added_unix_ms: 1,
            updated_unix_ms: 1,
        }
    }

    /// A portable store holding ID 2 (`a.wav`, present) and ID 5
    /// (`gone.wav`, missing), counter at 9.
    fn seed_portable_store(root: &Path) {
        use flux_library::{Availability, PortableStore};
        let mut store = PortableStore::create(root).expect("create portable store");
        store
            .import_tracks(
                vec![
                    imported_record(2, "a.wav", Availability::Present, "Aye"),
                    imported_record(5, "gone.wav", Availability::Missing, "Gone"),
                ],
                9,
            )
            .expect("seed import");
        store.commit().expect("seed commit");
    }

    fn boot_portable_at(root: &Path) -> Result<(Controller, NullControl), PortableOpenError> {
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        Controller::new_portable(engine, AudioStatus::Ready, root).map(|c| (c, control))
    }

    fn track_ids(controller: &Controller) -> Vec<u64> {
        controller.tracks().iter().map(|(id, _)| id.get()).collect()
    }

    #[test]
    fn legacy_mode_is_the_default_and_still_loads_and_saves() {
        let dir = TempDir::new("s4-legacy-mode");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        assert_eq!(controller.storage_mode(), StorageMode::Legacy);
        assert_eq!(
            controller.legacy_index_path(),
            Some(
                dir.path()
                    .join("state")
                    .join("library-index.json")
                    .as_path()
            )
        );
        assert_eq!(controller.portable_root(), None);
        assert!(controller.portable_recovery().is_none());
        let err = controller
            .acknowledge_portable_recovery()
            .expect_err("no portable recovery exists in legacy mode");
        assert!(err.contains("legacy"), "{err}");

        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(controller.tracks().len(), 4);
        assert!(
            !fx.root.join(".flux").exists(),
            "legacy mode never writes a portable store"
        );
        controller.save_index().expect("legacy save");
        controller.shutdown();

        let (controller2, _c2) = boot(&dir);
        assert_eq!(controller2.storage_mode(), StorageMode::Legacy);
        assert_eq!(controller2.tracks().len(), 4, "legacy index reloaded");
        controller2.shutdown();
    }

    #[test]
    fn unreadable_legacy_index_is_never_overwritten_by_save_or_scan() {
        let dir = TempDir::new("s7-legacy-protected");
        let index = dir.path().join("library-index.json");
        let original = b"this is not a valid Flux index";
        fs::write(&index, original).expect("corrupt index fixture");

        let (mut controller, _control) = boot_legacy_at(Some(index.clone()));
        assert!(
            controller.legacy_index_write_protected(),
            "an existing unreadable index must enter write protection"
        );

        let error = controller
            .save_index()
            .expect_err("explicit save must refuse");
        assert!(
            error.contains("will not overwrite"),
            "message should explain the protection: {error}"
        );
        assert_eq!(
            fs::read(&index).expect("index still present"),
            original,
            "an explicit save must not overwrite the unreadable index"
        );

        let root = two_track_dir(&dir, "music");
        controller.begin_scan(root);
        assert!(
            !controller.is_scanning(),
            "a scan must be refused before it can save over the index"
        );
        assert!(
            controller
                .last_error()
                .is_some_and(|message| message.contains("will not overwrite")),
            "the refusal must be visible"
        );
        assert_eq!(
            fs::read(&index).expect("index still present"),
            original,
            "a scan must not overwrite the unreadable index"
        );
        controller.shutdown();
    }

    #[test]
    fn replacing_unreadable_legacy_index_preserves_bytes_and_reenables_writes() {
        let dir = TempDir::new("s7-legacy-replace");
        let index = dir.path().join("library-index.json");
        let original = b"unreadable index payload";
        fs::write(&index, original).expect("corrupt index fixture");

        // An earlier preserved copy must never be overwritten.
        let mut earlier_name = index.as_os_str().to_os_string();
        earlier_name.push(".unreadable");
        let earlier = PathBuf::from(earlier_name);
        fs::write(&earlier, b"earlier preserved copy").expect("earlier backup");

        let (mut controller, _control) = boot_legacy_at(Some(index.clone()));
        assert!(controller.legacy_index_write_protected());

        let backup = controller
            .replace_unreadable_legacy_index()
            .expect("preserve and replace succeeds");
        assert_ne!(backup, index);
        assert_eq!(
            fs::read(&backup).expect("preserved copy"),
            original,
            "the unreadable bytes are preserved exactly"
        );
        assert_eq!(
            fs::read(&earlier).expect("earlier backup"),
            b"earlier preserved copy",
            "an earlier preserved copy is never overwritten"
        );
        assert!(
            !index.exists(),
            "the original path is freed for a new index"
        );
        assert!(!controller.legacy_index_write_protected());
        assert!(
            controller.last_error().is_none(),
            "the resolved protection error is cleared"
        );

        controller.save_index().expect("new empty index writes");
        let loaded =
            flux_library::Library::load_from_path(&index).expect("new index is valid and empty");
        assert!(loaded.is_empty());
        controller.shutdown();
    }

    #[test]
    fn portable_mode_opens_store_and_reconstructs_records() {
        let dir = TempDir::new("s4-portable-open");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 4_410, 0.4);
        seed_portable_store(&root);
        // A bystander legacy-index-shaped file: portable mode must
        // never read or write it.
        let legacy_bystander = dir.path().join("state").join("library-index.json");
        fs::create_dir_all(legacy_bystander.parent().expect("parent")).expect("state dir");
        fs::write(&legacy_bystander, b"legacy bystander bytes").expect("bystander");

        let (controller, _control) = boot_portable_at(&root).expect("portable open succeeds");
        assert_eq!(controller.storage_mode(), StorageMode::Portable);
        let canonical_root = fs::canonicalize(&root).expect("canonical root");
        assert_eq!(controller.portable_root(), Some(canonical_root.as_path()));
        assert_eq!(controller.legacy_index_path(), None);
        assert!(controller.portable_recovery().is_none());
        assert!(controller.last_error().is_none());

        assert_eq!(track_ids(&controller), vec![2, 5]);
        let aye = controller.track(TrackId::new(2)).expect("track 2");
        assert_eq!(aye.title(), "Aye");
        assert_eq!(aye.source().path(), canonical_root.join("a.wav").as_path());
        let gone = controller.track(TrackId::new(5)).expect("track 5");
        assert_eq!(gone.title(), "Gone", "missing records are represented");
        assert_eq!(
            fs::read(&legacy_bystander).expect("bystander"),
            b"legacy bystander bytes",
            "portable mode never writes the legacy index"
        );
        controller.shutdown();
    }

    #[test]
    fn two_controllers_cannot_own_the_same_legacy_index() {
        let dir = TempDir::new("s7-lock-legacy");
        let index = dir.path().join("library-index.json");
        let mut library = flux_library::Library::new();
        library.insert(Track::new(
            "One",
            flux_core::TrackSource::local("/music/one.flac"),
        ));
        library.save_to_path(&index).expect("seed index");
        let bytes = fs::read(&index).expect("index bytes");

        let (first, _control) = boot_legacy_at(Some(index.clone()));
        let (engine, _control2) = EngineHandle::spawn_null(44_100, 1);
        let error = Controller::new(engine, AudioStatus::Ready, Some(index.clone()))
            .err()
            .expect("second controller must be refused");
        assert!(
            matches!(
                error,
                super::ControllerOpenError::Lock(flux_library::LibraryLockError::Busy { .. })
            ),
            "got {error:?}"
        );
        assert!(
            error.to_string().contains("already open"),
            "message must be actionable: {error}"
        );
        assert_eq!(
            fs::read(&index).expect("index bytes"),
            bytes,
            "lock contention must not modify the catalogue"
        );

        // Re-opening the active library is a no-op, not a self-conflict.
        let mut first = first;
        first
            .switch_to_legacy_index(Some(index.clone()))
            .expect("same library is a no-op");

        first.shutdown();
        let (reopened, _control3) = boot_legacy_at(Some(index));
        reopened.shutdown();
    }

    #[test]
    fn two_controllers_cannot_own_the_same_portable_library() {
        let dir = TempDir::new("s7-lock-portable");
        let root = seeded_root(&dir, "portable");
        let catalog = catalog_path(&root);
        let bytes = fs::read(&catalog).expect("catalogue bytes");

        let (first, _control) = boot_portable_at(&root).expect("first controller");
        let (engine, _control2) = EngineHandle::spawn_null(44_100, 1);
        let error = Controller::new_portable(engine, AudioStatus::Ready, &root)
            .err()
            .expect("second controller must be refused");
        assert!(
            matches!(
                error,
                PortableOpenError::Lock(flux_library::LibraryLockError::Busy { .. })
            ),
            "got {error:?}"
        );
        assert_eq!(
            fs::read(&catalog).expect("catalogue bytes"),
            bytes,
            "lock contention must not touch the catalogue"
        );

        first.shutdown();
        let (reopened, _control3) = boot_portable_at(&root).expect("reopened after release");
        reopened.shutdown();
    }

    #[test]
    fn creating_a_library_respects_another_owners_lease() {
        let dir = TempDir::new("s7-lock-create");
        let root = dir.path().join("destination");
        fs::create_dir_all(&root).expect("root");
        let held =
            flux_library::LibraryLock::acquire_portable_root(&root).expect("hold destination");

        let error = create_portable_library(&root).expect_err("must refuse owned destination");
        assert!(
            matches!(
                error,
                CreateLibraryError::Lock(flux_library::LibraryLockError::Busy { .. })
            ),
            "got {error:?}"
        );
        assert!(
            !root.join(".flux").exists(),
            "creation must not run while another process owns the destination"
        );

        drop(held);
        create_portable_library(&root).expect("creation after release");
    }

    #[test]
    fn failed_portable_initialization_releases_its_lock() {
        let dir = TempDir::new("s7-lock-failed-init");
        let root = dir.path().join("empty-root");
        fs::create_dir_all(&root).expect("root");
        let (engine, _control) = EngineHandle::spawn_null(44_100, 1);
        let error = Controller::new_portable(engine, AudioStatus::Ready, &root)
            .err()
            .expect("no store exists");
        assert!(
            matches!(error, PortableOpenError::Store(_)),
            "got {error:?}"
        );

        // The failed attempt must not leave its lease behind.
        let lease = flux_library::LibraryLock::acquire_portable_root(&root)
            .expect("failed initialization released the lease");
        drop(lease);
    }

    #[test]
    fn switching_libraries_respects_other_owners_and_releases_the_old_lease() {
        let dir = TempDir::new("s7-lock-switch");
        let root_a = seeded_root(&dir, "a");
        let root_b = seeded_root(&dir, "b");
        let root_c = dir.path().join("c");
        fs::create_dir_all(&root_c).expect("root c");
        create_portable_library(&root_c).expect("create free destination");

        let (mut a, _control_a) = boot_portable_at(&root_a).expect("open a");
        let (b, _control_b) = boot_portable_at(&root_b).expect("open b");
        let canonical_a = fs::canonicalize(&root_a).expect("canonical a");

        // Same library: friendly no-op. Different owned library: refused
        // without changing the active backend.
        a.switch_to_portable(&root_a).expect("same library no-op");
        let error = a.switch_to_portable(&root_b).expect_err("b is owned");
        assert!(
            matches!(
                error,
                SwitchError::Portable(PortableOpenError::Lock(
                    flux_library::LibraryLockError::Busy { .. }
                ))
            ),
            "got {error:?}"
        );
        assert_eq!(a.portable_root(), Some(canonical_a.as_path()));

        // A free destination can be adopted; that releases root A.
        a.switch_to_portable(&root_c)
            .expect("adopt free destination");
        b.shutdown();
        a.shutdown();

        let (reopened_a, _control) = boot_portable_at(&root_a).expect("root A released");
        reopened_a.shutdown();
        let (reopened_c, _control) = boot_portable_at(&root_c).expect("root C released");
        reopened_c.shutdown();
    }

    #[test]
    fn background_scan_keeps_the_library_leased_until_completion() {
        let dir = TempDir::new("s7-lock-scan");
        let root = seeded_root(&dir, "portable");
        let (mut controller, control) = boot_portable_at(&root).expect("open");
        controller.begin_scan(root.clone());
        assert!(controller.is_scanning());

        let (engine, _control2) = EngineHandle::spawn_null(44_100, 1);
        let error = Controller::new_portable(engine, AudioStatus::Ready, &root)
            .err()
            .expect("scanning controller still owns the library");
        assert!(
            matches!(
                error,
                PortableOpenError::Lock(flux_library::LibraryLockError::Busy { .. })
            ),
            "got {error:?}"
        );

        let _ = drive(&mut controller, &control, |c, _| !c.is_scanning());
        controller.shutdown();
        let (reopened, _control3) = boot_portable_at(&root).expect("released after scan");
        reopened.shutdown();
    }

    #[test]
    fn portable_ids_gaps_and_counter_survive_scan_save_reopen() {
        use flux_library::{Availability, PortableStore};

        let dir = TempDir::new("s4-portable-ids");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 4_410, 0.4);
        seed_portable_store(&root);

        let (mut controller, control) = boot_portable_at(&root).expect("open");
        let b_file = write_dc_wav(&root, "b.wav", 4_410, 0.3);
        scanned(&mut controller, &control, root.clone());
        assert_eq!(track_ids(&controller), vec![2, 5, 9]);
        assert_eq!(
            id_of(&controller, &fs::canonicalize(&b_file).expect("canonical")),
            TrackId::new(9),
            "the preserved counter (9) issues the next ID"
        );

        // On disk: the new record persisted to the portable store,
        // the missing record is still Missing, the counter advanced.
        let store = PortableStore::open(&root).expect("direct open").store;
        assert_eq!(store.len(), 3);
        assert_eq!(store.next_track_id(), 10);
        assert_eq!(
            store
                .get(TrackId::new(5))
                .expect("missing kept")
                .availability(),
            Availability::Missing
        );
        assert_eq!(
            store.get(TrackId::new(9)).expect("new kept").availability(),
            Availability::Present
        );
        drop(store);
        controller.shutdown();

        // Reopen and scan a third file: allocation continues at 10.
        let (mut controller, control) = boot_portable_at(&root).expect("reopen");
        assert_eq!(track_ids(&controller), vec![2, 5, 9]);
        let c_file = write_dc_wav(&root, "c.wav", 4_410, 0.2);
        scanned(&mut controller, &control, root.clone());
        assert_eq!(
            id_of(&controller, &fs::canonicalize(&c_file).expect("canonical")),
            TrackId::new(10)
        );
        controller.shutdown();
        let store = PortableStore::open(&root).expect("direct open").store;
        assert_eq!(store.next_track_id(), 11);
        assert!(
            !dir.path().join("state").join("library-index.json").exists(),
            "no legacy index was ever written"
        );
    }

    #[test]
    fn portable_scan_keeps_override_effective_and_adds_new_tracks() {
        use flux_library::{Availability, PortableStore};

        let dir = TempDir::new("s6-scan-override");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 44_100, 0.4);

        let mut record = imported_record(1, "a.wav", Availability::Present, "Tag A");
        record.overrides.set_title("User Title".to_owned());
        let mut store = PortableStore::create(&root).expect("create");
        store.import_tracks(vec![record], 2).expect("seed");
        store.commit().expect("seed commit");
        drop(store);

        let (mut controller, control) = boot_portable_at(&root).expect("portable boot");
        let id = TrackId::new(1);
        assert_eq!(
            controller.track(id).expect("track").title(),
            "User Title",
            "the override is effective before the scan"
        );
        assert_eq!(controller.current_id(), Some(id));

        let new_file = write_dc_wav(&root, "b.wav", 4_410, 0.3);
        scanned(&mut controller, &control, root.clone());

        assert_eq!(
            controller.track(id).expect("track").title(),
            "User Title",
            "a scan must not replace the override with scanned metadata"
        );
        let new_id = id_of(
            &controller,
            &fs::canonicalize(&new_file).expect("canonical new file"),
        );
        assert_eq!(
            controller.track(new_id).expect("new track").title(),
            "b",
            "newly discovered tracks keep their scanned fallback metadata"
        );
        assert_eq!(track_ids(&controller), vec![1, 2]);
        assert_eq!(
            controller.current_id(),
            Some(id),
            "the refresh keeps the same library's selection"
        );
        controller.shutdown();

        // The store kept the override and captured the scan's tag
        // snapshot (a filename fallback here, since the WAV is untagged).
        let store = PortableStore::open(&root).expect("reopen store").store;
        let stored = store.get(id).expect("stored record");
        assert_eq!(
            stored.overrides().title(),
            Some(Some("User Title")),
            "the persistent override is untouched"
        );
        assert_eq!(
            stored.tags().title.as_deref(),
            Some("a"),
            "the scan refreshed the embedded-tag snapshot"
        );
        assert_eq!(stored.effective_metadata().title, "User Title");
        assert_eq!(
            store
                .get(new_id)
                .expect("new record")
                .tags()
                .title
                .as_deref(),
            Some("b")
        );
    }

    #[test]
    fn portable_scan_preserves_selection_and_paused_state() {
        use flux_library::{Availability, PortableStore};

        let dir = TempDir::new("s6-scan-selection");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 441_000, 0.4); // 10 seconds: no natural end

        let mut store = PortableStore::create(&root).expect("create");
        store
            .import_tracks(
                vec![imported_record(1, "a.wav", Availability::Present, "Aye")],
                2,
            )
            .expect("seed");
        store.commit().expect("seed commit");
        drop(store);

        let (mut controller, control) = boot_portable_at(&root).expect("portable boot");
        let id = TrackId::new(1);
        controller.play_track(id);
        controller.pause();
        assert_eq!(controller.player_state(), PlaybackState::Paused);

        write_dc_wav(&root, "b.wav", 4_410, 0.3);
        scanned(&mut controller, &control, root.clone());

        assert_eq!(controller.current_id(), Some(id));
        assert_eq!(
            controller.player_state(),
            PlaybackState::Paused,
            "a same-library refresh must not alter playback state"
        );
        assert_eq!(controller.player.queue().len(), 2);
        assert_eq!(controller.player.queue().current(), Some(id));
        controller.stop();
        controller.shutdown();
    }

    #[test]
    fn portable_scan_preserves_memory_only_tracks_without_path_duplicates() {
        use std::collections::HashSet;

        use flux_library::{Availability, PortableStore};

        let dir = TempDir::new("s6-scan-memory-only");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 44_100, 0.4);
        let mut store = PortableStore::create(&root).expect("create");
        store
            .import_tracks(
                vec![imported_record(1, "a.wav", Availability::Present, "Aye")],
                2,
            )
            .expect("seed");
        store.commit().expect("seed commit");
        drop(store);
        let outside = two_track_dir(&dir, "outside");

        let (mut controller, control) = boot_portable_at(&root).expect("portable boot");
        // White-box, like the existing scan-handoff tests: inject tracks
        // a portable sync cannot store, as a previous outside-root scan
        // would have left them.
        let mut library = controller.library.take().expect("library present");
        library.scan(&[outside.as_path()]);
        controller.tracks = super::snapshot(&library);
        controller.library = Some(library);
        let before = track_ids(&controller);
        assert_eq!(before.len(), 3);
        let memory_only: Vec<u64> = before.iter().copied().filter(|id| *id != 1).collect();

        write_dc_wav(&root, "b.wav", 4_410, 0.3);
        scanned(&mut controller, &control, root.clone());

        let after = track_ids(&controller);
        assert_eq!(after.len(), 4, "new track added, memory-only tracks kept");
        for id in &memory_only {
            assert!(
                after.contains(id),
                "memory-only track {id} was dropped by the refresh"
            );
        }
        let paths: HashSet<PathBuf> = controller
            .tracks()
            .iter()
            .map(|(_, track)| track.source().path().to_path_buf())
            .collect();
        assert_eq!(
            paths.len(),
            controller.tracks().len(),
            "every runtime track must own a distinct path"
        );
        controller.shutdown();
    }

    #[cfg(unix)]
    #[test]
    fn portable_scan_failed_commit_keeps_override_and_retry_persists() {
        use std::os::unix::fs::PermissionsExt;

        use flux_library::{Availability, PortableStore};

        let dir = TempDir::new("s6-scan-failed-commit");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 44_100, 0.4);

        let mut record = imported_record(1, "a.wav", Availability::Present, "Tag A");
        record.overrides.set_title("User Title".to_owned());
        let mut store = PortableStore::create(&root).expect("create");
        store.import_tracks(vec![record], 2).expect("seed");
        store.commit().expect("seed commit");
        drop(store);

        let (mut controller, control) = boot_portable_at(&root).expect("portable boot");
        let id = TrackId::new(1);
        let flux_dir = root.join(".flux");
        fs::set_permissions(&flux_dir, fs::Permissions::from_mode(0o555))
            .expect("make .flux read-only");
        scanned(&mut controller, &control, root.clone());
        fs::set_permissions(&flux_dir, fs::Permissions::from_mode(0o755))
            .expect("restore .flux permissions");

        assert_eq!(
            controller.track(id).expect("track").title(),
            "User Title",
            "a failed commit must not expose raw scanned metadata"
        );
        let error = controller.last_error().expect("commit failure is visible");
        assert!(
            error.contains("Could not save the portable library"),
            "{error}"
        );

        // The store holds the synced changes pending; the existing save
        // path retries the commit without a rescan.
        controller.save_index().expect("retry publishes");
        controller.shutdown();

        let store = PortableStore::open(&root).expect("reopen store").store;
        let stored = store.get(id).expect("stored record");
        assert_eq!(
            stored.overrides().title(),
            Some(Some("User Title")),
            "the retry preserves the override"
        );
        assert_eq!(
            stored.tags().title.as_deref(),
            Some("a"),
            "the retry publishes the tag snapshot captured before the failed commit"
        );
        assert_eq!(stored.effective_metadata().title, "User Title");
    }

    #[test]
    fn portable_open_failures_never_fall_back_or_create_anything() {
        let dir = TempDir::new("s4-portable-fail");
        let try_open = |root: &Path| {
            let (engine, _control) = EngineHandle::spawn_null(44_100, 1);
            Controller::new_portable(engine, AudioStatus::Ready, root)
                .map(|controller| controller.shutdown())
        };

        // Missing root.
        let err = try_open(&dir.path().join("nope")).expect_err("missing root must fail");
        assert!(
            matches!(err, PortableOpenError::RootInvalid { .. }),
            "{err:?}"
        );

        // Existing folder without a portable store: explicit failure,
        // and opening created nothing (no silent initialization).
        let empty_root = dir.path().join("empty-lib");
        fs::create_dir_all(&empty_root).expect("empty root");
        let err = try_open(&empty_root).expect_err("storeless root must fail");
        assert!(
            matches!(
                err,
                PortableOpenError::Store(flux_library::PortableStoreError::StoreNotFound { .. })
            ),
            "{err:?}"
        );
        assert!(
            !empty_root.join(".flux").exists(),
            "a failed open must not initialize a store"
        );

        // Corrupt store: typed failure, files untouched, no fallback.
        let bad_root = dir.path().join("bad-lib");
        let flux_dir = bad_root.join(".flux");
        fs::create_dir_all(&flux_dir).expect(".flux dir");
        fs::write(flux_dir.join("catalog.flux"), b"garbage{").expect("garbage catalog");
        let err = try_open(&bad_root).expect_err("corrupt store must fail");
        assert!(
            matches!(
                err,
                PortableOpenError::Store(
                    flux_library::PortableStoreError::BothCataloguesInvalid { .. }
                )
            ),
            "{err:?}"
        );
        assert_eq!(
            fs::read(flux_dir.join("catalog.flux")).expect("catalog"),
            b"garbage{",
            "corrupt storage is preserved, never repaired or replaced"
        );
    }

    #[test]
    fn portable_recovery_gates_writes_until_acknowledged() {
        use flux_library::{Availability, PortableStore, RelPath, TagSet};

        let dir = TempDir::new("s4-portable-recovery");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 4_410, 0.4);
        write_dc_wav(&root, "b.wav", 4_410, 0.3);
        let tags = |title: &str| TagSet {
            title: Some(title.to_owned()),
            ..TagSet::default()
        };
        let mut store = PortableStore::create(&root).expect("create");
        store
            .add_track(
                RelPath::from_utf8("a.wav").expect("rel"),
                Availability::Present,
                None,
                tags("Aye"),
            )
            .expect("add a");
        store.commit().expect("commit with a (becomes backup)");
        store
            .add_track(
                RelPath::from_utf8("b.wav").expect("rel"),
                Availability::Present,
                None,
                tags("Bee"),
            )
            .expect("add b");
        store.commit().expect("commit with b");
        drop(store);
        fs::write(root.join(".flux").join("catalog.flux"), b"junk").expect("damage primary");

        let (mut controller, control) =
            boot_portable_at(&root).expect("backup recovery still opens");
        let recovery = controller.portable_recovery().expect("recovery pending");
        assert_eq!(recovery.recovered_generation, 2);
        assert_eq!(
            controller.tracks().len(),
            1,
            "only the backup generation's track is visible"
        );
        assert!(
            controller
                .last_error()
                .is_some_and(|e| e.contains("read-only")),
            "recovery state is visible: {:?}",
            controller.last_error()
        );

        // Writes are gated: save refuses, scan refuses.
        let err = controller.save_index().expect_err("save must refuse");
        assert!(err.contains("read-only"), "{err}");
        controller.begin_scan(root.clone());
        assert!(
            !controller.is_scanning(),
            "scan must refuse while read-only"
        );

        // Explicit acknowledgement restores writability.
        controller
            .acknowledge_portable_recovery()
            .expect("acknowledge");
        assert!(controller.portable_recovery().is_none());
        controller
            .save_index()
            .expect("save works after acknowledgement");
        scanned(&mut controller, &control, root.clone());
        assert_eq!(controller.tracks().len(), 2, "b is scanned back in");
        controller.shutdown();

        let store = PortableStore::open(&root).expect("direct open").store;
        assert_eq!(store.len(), 2);
        assert!(!store.is_recovered_read_only());
    }

    #[cfg(unix)]
    #[test]
    fn portable_commit_failure_is_visible_and_the_retry_persists() {
        use std::os::unix::fs::PermissionsExt;

        use flux_library::PortableStore;

        let dir = TempDir::new("s4-portable-commit-fail");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 4_410, 0.4);
        PortableStore::create(&root).expect("create empty store");

        let (mut controller, control) = boot_portable_at(&root).expect("open");
        let flux_dir = root.join(".flux");
        fs::set_permissions(&flux_dir, fs::Permissions::from_mode(0o555))
            .expect("make .flux read-only");
        scanned(&mut controller, &control, root.clone());
        // Restore writability before any assertion can panic, so the
        // temp-dir cleanup works either way.
        fs::set_permissions(&flux_dir, fs::Permissions::from_mode(0o755))
            .expect("restore .flux permissions");

        assert_eq!(
            controller.tracks().len(),
            1,
            "the scan result is kept in memory"
        );
        let error = controller.last_error().expect("commit failure reported");
        assert!(
            error.contains("Could not save the portable library"),
            "{error}"
        );
        let store = PortableStore::open(&root).expect("direct open").store;
        assert_eq!(store.len(), 0, "the failed commit published nothing");
        drop(store);

        // The pending changes were retained: an explicit save retries
        // and now persists them without rescanning.
        controller.save_index().expect("retry save succeeds");
        let store = PortableStore::open(&root).expect("direct open").store;
        assert_eq!(store.len(), 1);
        assert_eq!(store.next_track_id(), 2);
        controller.shutdown();
    }

    #[test]
    fn portable_scan_outside_the_root_is_rejected_subfolders_allowed() {
        use flux_library::PortableStore;

        let dir = TempDir::new("s4-portable-scan-scope");
        let root = dir.path().join("library");
        fs::create_dir_all(root.join("sub")).expect("subfolder");
        write_dc_wav(&root, "a.wav", 4_410, 0.4);
        seed_portable_store(&root);
        let (mut controller, control) = boot_portable_at(&root).expect("open");
        let generation_before = PortableStore::open(&root)
            .expect("direct open")
            .store
            .generation();

        let outside = dir.path().join("elsewhere");
        fs::create_dir_all(&outside).expect("outside dir");
        write_dc_wav(&outside, "x.wav", 4_410, 0.5);
        controller.begin_scan(outside);
        assert!(!controller.is_scanning());
        let error = controller.last_error().expect("rejection reported");
        assert!(
            error.contains("outside this portable library's folder"),
            "{error}"
        );
        assert_eq!(controller.tracks().len(), 2, "library unchanged");
        let store = PortableStore::open(&root).expect("direct open").store;
        assert_eq!(store.generation(), generation_before, "store untouched");
        drop(store);

        // A subfolder of the root scans fine.
        let sub_file = write_dc_wav(&root.join("sub"), "s.wav", 4_410, 0.3);
        scanned(&mut controller, &control, root.join("sub"));
        assert_eq!(
            id_of(
                &controller,
                &fs::canonicalize(&sub_file).expect("canonical")
            ),
            TrackId::new(9)
        );
        controller.shutdown();
    }

    #[test]
    fn lost_scan_thread_recovers_the_portable_library() {
        let dir = TempDir::new("s4-portable-lost-scan");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        write_dc_wav(&root, "a.wav", 4_410, 0.4);
        seed_portable_store(&root);
        let (mut controller, _control) = boot_portable_at(&root).expect("open");
        let before: Vec<_> = controller
            .tracks()
            .iter()
            .map(|(id, t)| (*id, t.source().path().to_owned()))
            .collect();
        assert_eq!(before.len(), 2);

        let (done_tx, done_rx) = std::sync::mpsc::channel::<ScanOutcome>();
        drop(done_tx);
        controller.scan_rx = Some(done_rx);
        controller.library = None;
        controller.portable_store = None;
        controller.poll();

        let after: Vec<_> = controller
            .tracks()
            .iter()
            .map(|(id, t)| (*id, t.source().path().to_owned()))
            .collect();
        assert_eq!(after, before, "IDs and paths restored from the store");
        assert!(controller.portable_store.is_some(), "store reopened");
        let error = controller.last_error().expect("failure reported");
        assert!(
            error.contains("restored from the portable library files"),
            "{error}"
        );
        controller.save_index().expect("restored store can save");
        controller.shutdown();
    }

    #[test]
    fn explicit_migration_never_switches_the_active_backend() {
        let dir = TempDir::new("s4-migration");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let legacy_ids = track_ids(&controller);
        assert_eq!(legacy_ids.len(), 4);
        let index_path = dir.path().join("state").join("library-index.json");
        let index_before = fs::read(&index_path).expect("scan saved the index");

        // Plan is read-only; execution migrates into the music folder
        // itself (every legacy path is inside it).
        let plan = plan_legacy_migration(&index_path, &fx.root).expect("plan");
        assert_eq!(plan.migrated_count(), 4);
        assert!(!fx.root.join(".flux").exists(), "planning wrote nothing");
        let report = migrate_legacy_library(&index_path, &fx.root).expect("migration succeeds");
        assert_eq!(report.plan().present_count(), 4);
        assert!(!report.library_id().is_empty());

        // The running controller is untouched: same backend, same
        // tracks, no new error, and it still saves to the legacy
        // index — which is byte-for-byte what it was.
        assert_eq!(controller.storage_mode(), StorageMode::Legacy);
        assert_eq!(track_ids(&controller), legacy_ids);
        assert!(controller.last_error().is_none());
        assert_eq!(fs::read(&index_path).expect("index"), index_before);
        controller.save_index().expect("legacy save still works");

        // A second migration refuses (destination initialized) and
        // still changes nothing about the controller.
        let err =
            migrate_legacy_library(&index_path, &fx.root).expect_err("re-migration must refuse");
        assert!(
            matches!(
                err,
                flux_library::MigrationError::DestinationAlreadyInitialized { .. }
            ),
            "{err:?}"
        );
        assert_eq!(controller.storage_mode(), StorageMode::Legacy);
        assert_eq!(track_ids(&controller), legacy_ids);
        controller.shutdown();

        // Only an explicit new construction uses the migrated store —
        // and it sees the very same IDs.
        let (portable_controller, _c2) = boot_portable_at(&fx.root).expect("migrated store opens");
        assert_eq!(portable_controller.storage_mode(), StorageMode::Portable);
        assert_eq!(track_ids(&portable_controller), legacy_ids);
        portable_controller.shutdown();
    }

    // ---- S5: explicit library lifecycle (create / switch) -----------

    /// A legacy controller with a caller-chosen index path (or none).
    fn boot_legacy_at(index: Option<PathBuf>) -> (Controller, NullControl) {
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        (
            Controller::new(engine, AudioStatus::Ready, index)
                .expect("controller opens the test library"),
            control,
        )
    }

    /// A folder with exactly two scannable tracks (IDs 1 and 2 after
    /// a scan), unlike `fixtures` (four indexed files).
    fn two_track_dir(dir: &TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        fs::create_dir_all(&root).expect("music dir");
        write_dc_wav(&root, "a.wav", 11_025, 0.4);
        write_dc_wav(&root, "b.wav", 11_025, 0.3);
        root
    }

    /// A portable root seeded by `seed_portable_store` (ID 2 “Aye”,
    /// present as `a.wav`; ID 5 “Gone”, missing; counter 9).
    fn seeded_root(dir: &TempDir, name: &str) -> PathBuf {
        let root = dir.path().join(name);
        fs::create_dir_all(&root).expect("portable root");
        write_dc_wav(&root, "a.wav", 11_025, 0.4);
        seed_portable_store(&root);
        root
    }

    fn catalog_path(root: &Path) -> PathBuf {
        root.join(".flux")
            .join(flux_library::portable::CATALOG_FILE_NAME)
    }

    #[test]
    fn create_portable_library_publishes_empty_library_and_reopens() {
        let dir = TempDir::new("s5-create");
        let root = dir.path().join("new-library");
        fs::create_dir_all(&root).expect("root");

        let created = create_portable_library(&root).expect("creation");
        assert_eq!(created.root(), fs::canonicalize(&root).expect("canonical"));
        assert!(!created.library_id().is_empty());
        let catalog = catalog_path(&root);
        let catalog_before = fs::read(&catalog).expect("initial catalogue published");

        let (controller, _c) = boot_portable_at(&root).expect("created library opens");
        assert_eq!(controller.storage_mode(), StorageMode::Portable);
        assert!(controller.tracks().is_empty(), "a new library is empty");
        controller.shutdown();

        // Creating again refuses and changes nothing on disk.
        let err = create_portable_library(&root).expect_err("second creation must refuse");
        assert!(
            matches!(err, CreateLibraryError::AlreadyInitialized { .. }),
            "{err:?}"
        );
        assert_eq!(fs::read(&catalog).expect("catalog"), catalog_before);
    }

    #[test]
    fn create_refuses_partial_destination_and_preserves_bytes() {
        let dir = TempDir::new("s5-create-refusals");

        // Leftover segment, no catalogue.
        let partial = dir.path().join("partial");
        fs::create_dir_all(partial.join(".flux")).expect("flux dir");
        let orphan = partial.join(".flux").join("segment-leftover.json");
        fs::write(&orphan, b"leftover-bytes").expect("orphan");
        let err = create_portable_library(&partial).expect_err("partial store must refuse");
        assert!(
            matches!(err, CreateLibraryError::PartialOrInvalid { .. }),
            "{err:?}"
        );
        assert_eq!(fs::read(&orphan).expect("orphan"), b"leftover-bytes");
        assert!(!catalog_path(&partial).exists());

        // Corrupt catalogue: preserved, not repaired over.
        let corrupt = dir.path().join("corrupt");
        fs::create_dir_all(corrupt.join(".flux")).expect("flux dir");
        let catalog = catalog_path(&corrupt);
        fs::write(&catalog, b"{ not a catalogue").expect("catalog");
        let err = create_portable_library(&corrupt).expect_err("invalid store must refuse");
        assert!(
            matches!(err, CreateLibraryError::PartialOrInvalid { .. }),
            "{err:?}"
        );
        assert_eq!(fs::read(&catalog).expect("catalog"), b"{ not a catalogue");

        // A missing root is refused and never invented.
        let missing = dir.path().join("missing");
        let err = create_portable_library(&missing).expect_err("missing root must refuse");
        assert!(
            matches!(err, CreateLibraryError::RootInvalid { .. }),
            "{err:?}"
        );
        assert!(!missing.exists(), "creation must not invent directories");

        // An empty .flux directory holds no data and does not block.
        let empty_flux = dir.path().join("empty-flux");
        fs::create_dir_all(empty_flux.join(".flux")).expect("flux dir");
        create_portable_library(&empty_flux).expect("empty .flux is creatable");
    }

    #[test]
    fn switch_to_new_portable_library_creates_and_activates() {
        let dir = TempDir::new("s5-create-switch");
        let music = two_track_dir(&dir, "music");
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, music);
        assert_eq!(track_ids(&controller), vec![1, 2]);
        let index_path = dir.path().join("state").join("library-index.json");
        let index_before = fs::read(&index_path).expect("index written");
        let _ = controller.poll(); // drain scan events

        let new_root = dir.path().join("portable");
        fs::create_dir_all(&new_root).expect("new root");
        let created = controller
            .switch_to_new_portable_library(&new_root)
            .expect("create + switch");
        assert_eq!(controller.storage_mode(), StorageMode::Portable);
        assert_eq!(controller.portable_root(), Some(created.root()));
        assert!(
            controller.tracks().is_empty(),
            "the new library starts empty"
        );
        assert_eq!(controller.current_id(), None);
        assert!(
            controller.poll().contains(&ControllerEvent::LibraryChanged),
            "front-ends must learn the library changed"
        );
        // The outgoing legacy index was not written as a side effect.
        assert_eq!(fs::read(&index_path).expect("index"), index_before);
        // The active controller owns the new library, so a second
        // opener must wait until it closes.
        controller.shutdown();
        // And the new library is a real, reopenable store.
        let (again, _c2) = boot_portable_at(&new_root).expect("created library reopens");
        again.shutdown();
    }

    #[test]
    fn switch_replaces_library_and_never_carries_ids_across() {
        let dir = TempDir::new("s5-switch-ids");
        // Portable candidate with IDs 2 (“Aye”) and 5 (“Gone”).
        let portable_root = seeded_root(&dir, "portable");

        // Legacy controller with different tracks; ID 1 selected and
        // playing — an ID the candidate does not even have.
        let music = two_track_dir(&dir, "legacy-music");
        let (mut controller, control) = boot_legacy_at(None);
        scanned(&mut controller, &control, music);
        assert_eq!(track_ids(&controller), vec![1, 2]);
        controller.play_track(TrackId::new(1));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.current_id(), Some(TrackId::new(1)));

        controller
            .switch_to_portable(&portable_root)
            .expect("switch to existing library");
        assert_eq!(controller.storage_mode(), StorageMode::Portable);
        assert_eq!(track_ids(&controller), vec![2, 5]);
        assert_eq!(
            controller.player_state(),
            PlaybackState::Stopped,
            "playback must not continue into another library"
        );
        // The selection is the NEW library's first track, not the old
        // library's ID 1 reinterpreted (ID 1 does not exist here, and
        // ID 2 is a different track than the legacy ID 2 was).
        assert_eq!(controller.current_id(), Some(TrackId::new(2)));
        assert_eq!(
            controller.track(TrackId::new(2)).expect("track 2").title(),
            "Aye"
        );
        assert!(controller.track(TrackId::new(1)).is_none());
        controller.shutdown();
    }

    #[test]
    fn failed_switch_leaves_active_library_completely_unchanged() {
        let dir = TempDir::new("s5-failed-switch");
        let music = two_track_dir(&dir, "music");
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, music);
        let index_path = dir.path().join("state").join("library-index.json");
        controller.play_track(TrackId::new(2));
        assert_eq!(controller.player_state(), PlaybackState::Playing);

        let assert_unchanged = |c: &Controller| {
            assert_eq!(c.storage_mode(), StorageMode::Legacy);
            assert_eq!(c.legacy_index_path(), Some(index_path.as_path()));
            assert_eq!(track_ids(c), vec![1, 2]);
            assert_eq!(c.current_id(), Some(TrackId::new(2)));
            assert_eq!(c.player_state(), PlaybackState::Playing);
        };

        // Nonexistent candidate: refused, nothing created.
        let missing = dir.path().join("nowhere");
        let err = controller
            .switch_to_portable(&missing)
            .expect_err("missing candidate must fail");
        assert!(matches!(
            err,
            SwitchError::Portable(PortableOpenError::RootInvalid { .. })
        ));
        assert_unchanged(&controller);
        assert!(!missing.exists());

        // Empty folder: no store, and opening must not create one.
        let empty = dir.path().join("empty");
        fs::create_dir_all(&empty).expect("empty dir");
        assert!(controller.switch_to_portable(&empty).is_err());
        assert_unchanged(&controller);
        assert!(!empty.join(".flux").exists(), "opening never creates");

        // Corrupt store: refused, bytes preserved.
        let corrupt = dir.path().join("corrupt");
        fs::create_dir_all(corrupt.join(".flux")).expect("flux dir");
        let catalog = catalog_path(&corrupt);
        fs::write(&catalog, b"garbage").expect("catalog");
        assert!(controller.switch_to_portable(&corrupt).is_err());
        assert_unchanged(&controller);
        assert_eq!(fs::read(&catalog).expect("catalog"), b"garbage");

        // Create over an initialized library: refused.
        let initialized = seeded_root(&dir, "initialized");
        let err = controller
            .switch_to_new_portable_library(&initialized)
            .expect_err("initialized destination must refuse");
        assert!(matches!(err, CreateLibraryError::AlreadyInitialized { .. }));
        assert_unchanged(&controller);

        // Corrupt legacy index candidate: refused (no empty-library
        // substitution on switch, unlike legacy startup).
        let bad_index = dir.path().join("bad-index.json");
        fs::write(&bad_index, b"{ nope").expect("bad index");
        let err = controller
            .switch_to_legacy_index(Some(bad_index))
            .expect_err("corrupt legacy candidate must fail");
        assert!(matches!(err, SwitchError::Legacy(_)), "{err:?}");
        assert_unchanged(&controller);
        controller.shutdown();
    }

    #[test]
    fn switch_during_scan_is_refused() {
        let dir = TempDir::new("s5-switch-scanning");
        let portable_root = seeded_root(&dir, "portable");
        let (mut controller, _ctl) = boot_legacy_at(None);

        // Simulate the in-flight-scan state (library owned by the
        // worker) deterministically, as the S1 handoff tests do.
        let saved_library = controller.library.take();
        let (_tx, rx) = std::sync::mpsc::channel::<ScanOutcome>();
        controller.scan_rx = Some(rx);
        assert!(controller.is_scanning());

        let err = controller
            .switch_to_portable(&portable_root)
            .expect_err("switch during scan must refuse");
        assert!(matches!(err, SwitchError::ScanInProgress), "{err:?}");

        let new_root = dir.path().join("created-during-scan");
        fs::create_dir_all(&new_root).expect("root");
        let err = controller
            .switch_to_new_portable_library(&new_root)
            .expect_err("create+switch during scan must refuse");
        assert!(matches!(err, CreateLibraryError::ScanInProgress), "{err:?}");
        assert!(
            !new_root.join(".flux").exists(),
            "a refused create+switch must not create anything"
        );

        // Hand the library back for a clean shutdown.
        controller.scan_rx = None;
        controller.library = saved_library;
        controller.shutdown();
    }

    #[test]
    fn migration_cancel_writes_nothing_and_execute_never_switches() {
        let dir = TempDir::new("s5-migration-flow");
        let music = two_track_dir(&dir, "music");
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, music.clone());
        let index_path = dir.path().join("state").join("library-index.json");
        let index_before = fs::read(&index_path).expect("index");

        // Plan, then drop it (the UI cancel path): nothing written.
        let plan = plan_legacy_migration(&index_path, &music).expect("plan");
        assert_eq!(plan.migrated_count(), 2);
        drop(plan);
        assert!(!music.join(".flux").exists(), "cancel must write nothing");
        assert_eq!(controller.storage_mode(), StorageMode::Legacy);

        // Plan + execute: store exists, controller untouched.
        let plan = plan_legacy_migration(&index_path, &music).expect("plan again");
        execute_legacy_migration(&plan).expect("execute");
        assert!(catalog_path(&music).exists());
        assert_eq!(controller.storage_mode(), StorageMode::Legacy);
        assert_eq!(track_ids(&controller), vec![1, 2]);
        assert_eq!(fs::read(&index_path).expect("index"), index_before);

        // Only the explicit switch activates the migrated library.
        controller.switch_to_portable(&music).expect("activate");
        assert_eq!(controller.storage_mode(), StorageMode::Portable);
        assert_eq!(track_ids(&controller), vec![1, 2]);
        controller.shutdown();
    }

    #[test]
    fn switch_to_recovered_store_keeps_read_only_gate() {
        use flux_library::{Availability, PortableStore, RelPath, TagSet};

        let dir = TempDir::new("s5-switch-recovery");
        let portable_root = dir.path().join("portable");
        fs::create_dir_all(&portable_root).expect("portable root");
        write_dc_wav(&portable_root, "a.wav", 4_410, 0.4);
        write_dc_wav(&portable_root, "b.wav", 4_410, 0.3);
        let tags = |title: &str| TagSet {
            title: Some(title.to_owned()),
            ..TagSet::default()
        };
        let mut store = PortableStore::create(&portable_root).expect("create");
        store
            .add_track(
                RelPath::from_utf8("a.wav").expect("rel"),
                Availability::Present,
                None,
                tags("Aye"),
            )
            .expect("add a");
        store.commit().expect("commit with a (becomes backup)");
        store
            .add_track(
                RelPath::from_utf8("b.wav").expect("rel"),
                Availability::Present,
                None,
                tags("Bee"),
            )
            .expect("add b");
        store.commit().expect("commit with b");
        drop(store);
        // Damage the primary; the backup (with only “Aye”, ID 1)
        // must be what the switch sees.
        fs::write(catalog_path(&portable_root), b"garbage").expect("damage");

        let (mut controller, _ctl) = boot_legacy_at(None);
        controller
            .switch_to_portable(&portable_root)
            .expect("recovered store still switches in");
        assert_eq!(track_ids(&controller), vec![1]);
        assert!(controller.portable_recovery().is_some());
        assert!(
            controller.save_index().is_err(),
            "recovery gate must survive the switch"
        );
        controller
            .acknowledge_portable_recovery()
            .expect("acknowledge");
        controller.save_index().expect("save after acknowledge");
        controller.shutdown();
    }

    #[test]
    fn switch_back_to_legacy_loads_index_and_rebuilds_state() {
        let dir = TempDir::new("s5-switch-back");
        // A legacy index with two tracks (built by a real scan).
        let music = two_track_dir(&dir, "legacy-music");
        let (mut legacy, legacy_control) = boot(&dir);
        scanned(&mut legacy, &legacy_control, music);
        legacy.shutdown();
        let index_path = dir.path().join("state").join("library-index.json");

        // Start portable, then switch to that legacy index.
        let portable_root = seeded_root(&dir, "portable");
        let catalog_before = fs::read(catalog_path(&portable_root)).expect("catalog");
        let (mut controller, _c) = boot_portable_at(&portable_root).expect("portable boot");
        assert_eq!(track_ids(&controller), vec![2, 5]);
        controller
            .switch_to_legacy_index(Some(index_path.clone()))
            .expect("switch to legacy");
        assert_eq!(controller.storage_mode(), StorageMode::Legacy);
        assert_eq!(controller.portable_root(), None);
        assert_eq!(track_ids(&controller), vec![1, 2]);
        assert_eq!(controller.current_id(), Some(TrackId::new(1)));
        // The portable store was not written by the switch.
        assert_eq!(
            fs::read(catalog_path(&portable_root)).expect("catalog"),
            catalog_before
        );

        // A missing index file means an empty legacy library
        // (legacy startup semantics), still an explicit switch.
        controller
            .switch_to_legacy_index(Some(dir.path().join("absent.json")))
            .expect("missing index switches to empty legacy");
        assert!(controller.tracks().is_empty());
        assert_eq!(controller.current_id(), None);
        controller.shutdown();
    }

    // ---- S6: catalogue reconciliation -------------------------------

    #[test]
    fn controller_reconcile_marks_missing_and_publishes() {
        use flux_library::{Availability, PortableStore};

        let dir = TempDir::new("s6-reconcile");
        let root = seeded_root(&dir, "portable");
        let (mut controller, _c) = boot_portable_at(&root).expect("portable boot");
        assert_eq!(track_ids(&controller), vec![2, 5]);

        // ID 2's file disappears; ID 5 was already missing.
        fs::remove_file(root.join("a.wav")).expect("delete audio");
        let report = controller
            .reconcile_portable_library()
            .expect("reconcile succeeds");
        assert_eq!(
            report.outcome_for(TrackId::new(2)),
            Some(&flux_library::ReconcileOutcome::NewlyMissing)
        );
        assert_eq!(
            report.outcome_for(TrackId::new(5)),
            Some(&flux_library::ReconcileOutcome::StillMissing)
        );
        assert_eq!(report.missing_count(), 2);
        // Both records remain visible (missing is retained, never deleted).
        assert_eq!(track_ids(&controller), vec![2, 5]);
        assert!(controller.last_error().is_none());
        assert!(
            controller
                .last_status()
                .is_some_and(|status| status.starts_with("Reconciled 2 track(s)")),
            "{:?}",
            controller.last_status()
        );
        controller.shutdown();

        // Published: a fresh open sees the refreshed availability.
        let reopened = PortableStore::open(&root).expect("reopen").store;
        assert_eq!(
            reopened
                .get(TrackId::new(2))
                .expect("record 2")
                .availability(),
            Availability::Missing
        );
        assert_eq!(
            reopened
                .get(TrackId::new(2))
                .expect("record 2")
                .tags()
                .title
                .as_deref(),
            Some("Aye"),
            "metadata preserved"
        );
    }

    #[test]
    fn controller_reconcile_guards_legacy_recovery_and_scanning() {
        let dir = TempDir::new("s6-reconcile-guards");

        // Legacy mode: refused, visible, nothing changes.
        let (mut legacy, _l) = boot_legacy_at(None);
        assert!(legacy.reconcile_portable_library().is_err());
        assert!(legacy.last_error().is_some());
        assert_eq!(legacy.storage_mode(), StorageMode::Legacy);
        legacy.shutdown();

        // Recovered (read-only) portable store: refused until the
        // user acknowledges recovery — reconcile cannot bypass it.
        let root = seeded_root(&dir, "portable");
        let (mut warm, _w) = boot_portable_at(&root).expect("seeded store opens");
        warm.save_index().expect("advance generation");
        warm.shutdown();
        fs::write(catalog_path(&root), b"garbage").expect("damage primary");
        let (mut controller, _c) = boot_portable_at(&root).expect("recovered boot");
        assert!(controller.portable_recovery().is_some());
        assert!(controller.reconcile_portable_library().is_err());
        controller
            .acknowledge_portable_recovery()
            .expect("acknowledge");
        controller
            .reconcile_portable_library()
            .expect("reconcile after acknowledgement");

        // While a scan owns the store: refused.
        let saved_library = controller.library.take();
        let (_tx, rx) = std::sync::mpsc::channel::<ScanOutcome>();
        controller.scan_rx = Some(rx);
        assert!(controller.is_scanning());
        assert!(controller.reconcile_portable_library().is_err());
        controller.scan_rx = None;
        controller.library = saved_library;
        controller.shutdown();
    }

    #[test]
    fn controller_relink_flow_moves_runtime_path_and_preserves_selection() {
        use flux_library::PortableStore;

        let dir = TempDir::new("s6-relink-flow");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("root");
        write_dc_wav(&root, "a.wav", 11_025, 0.4);
        // An unrelated legacy index that must never be touched.
        let bystander = dir.path().join("legacy.json");
        fs::write(&bystander, b"bystander").expect("bystander");

        create_portable_library(&root).expect("create library");
        let (mut controller, control) = boot_portable_at(&root).expect("portable boot");
        scanned(&mut controller, &control, root.clone());
        assert_eq!(track_ids(&controller), vec![1]);
        let id = TrackId::new(1);
        assert_eq!(controller.current_id(), Some(id));

        // The user renames the file; reconciliation reports the lead.
        fs::rename(root.join("a.wav"), root.join("moved.wav")).expect("rename");
        let report = controller
            .reconcile_portable_library()
            .expect("reconcile succeeds");
        assert_eq!(report.unique_candidate_count(), 1);
        let candidate = report.relink_candidates[0].candidates[0].clone();
        // Nothing applied yet: the runtime still shows the old path.
        assert!(
            controller
                .track(id)
                .expect("track")
                .source()
                .path()
                .ends_with("a.wav")
        );

        controller
            .relink_portable_track(id, &candidate)
            .expect("validated relink applies");
        let canonical_root = controller.portable_root().expect("root").to_path_buf();
        assert_eq!(
            controller.track(id).expect("track").source().path(),
            canonical_root.join("moved.wav"),
            "runtime picks up the new path"
        );
        assert_eq!(
            controller.current_id(),
            Some(id),
            "selection preserved (same library)"
        );
        assert_eq!(fs::read(&bystander).expect("bystander"), b"bystander");
        controller.shutdown();

        let reopened = PortableStore::open(&root).expect("reopen").store;
        assert_eq!(
            reopened.get(id).expect("record").path(),
            &flux_library::RelPath::from_utf8("moved.wav").expect("rel")
        );
    }

    #[cfg(unix)]
    #[test]
    fn controller_reconcile_failed_commit_is_visible_and_retryable() {
        use std::os::unix::fs::PermissionsExt;

        use flux_library::{Availability, PortableStore};

        let dir = TempDir::new("s6-reconcile-failed-commit");
        let root = seeded_root(&dir, "portable");
        let (mut controller, _c) = boot_portable_at(&root).expect("portable boot");
        fs::remove_file(root.join("a.wav")).expect("delete audio");

        let flux_dir = root.join(".flux");
        fs::set_permissions(&flux_dir, fs::Permissions::from_mode(0o500)).expect("chmod ro");
        let result = controller.reconcile_portable_library();
        fs::set_permissions(&flux_dir, fs::Permissions::from_mode(0o700)).expect("chmod back");
        assert!(
            result.is_err(),
            "publication failure is returned, not hidden"
        );
        assert!(controller.last_error().is_some(), "and visible");
        // The controller stays on the same library throughout.
        assert_eq!(controller.storage_mode(), StorageMode::Portable);
        assert_eq!(track_ids(&controller), vec![2, 5]);

        // The pending reconciled state publishes on the next save.
        controller.save_index().expect("retry publishes");
        controller.shutdown();
        let reopened = PortableStore::open(&root).expect("reopen").store;
        assert_eq!(
            reopened
                .get(TrackId::new(2))
                .expect("record 2")
                .availability(),
            Availability::Missing
        );
    }

    #[test]
    fn controller_reconcile_preserves_memory_only_tracks() {
        use flux_library::PortableStore;

        let dir = TempDir::new("s6-reconcile-memory-only");
        let root = seeded_root(&dir, "portable");
        let outside = two_track_dir(&dir, "outside");
        let (mut controller, _c) = boot_portable_at(&root).expect("portable boot");
        assert_eq!(track_ids(&controller), vec![2, 5]);

        // White-box (like the scan-handoff tests): put a track into
        // the in-memory library that the store does not hold, as a
        // sync-skipped outside-root scan result would.
        let mut library = controller.library.take().expect("library present");
        library.scan(&[outside.as_path()]);
        controller.tracks = super::snapshot(&library);
        controller.library = Some(library);
        let with_memory_only = track_ids(&controller);
        assert_eq!(
            with_memory_only.len(),
            4,
            "two outside tracks joined in memory"
        );

        controller
            .reconcile_portable_library()
            .expect("reconcile succeeds");
        assert_eq!(
            track_ids(&controller),
            with_memory_only,
            "the refresh after reconcile must not drop in-memory-only tracks"
        );
        controller.shutdown();

        // The store still holds only its own two records.
        let reopened = PortableStore::open(&root).expect("reopen").store;
        assert_eq!(reopened.len(), 2);
    }

    #[test]
    fn queue_mutations_preserve_current_and_duplicate_identity() {
        let dir = TempDir::new("queue-mutations");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let b = id_of(&controller, &fx.b);
        let queue_file = legacy_queue_file(&dir);
        assert!(
            queue_file.exists(),
            "the auto-derived queue is persisted after a scan"
        );

        controller.play_track(a);
        let current = controller.current_queue_entry_id().expect("current");
        let current_index = controller.queue_entry_index(current).expect("index");
        let initial_len = controller.queue_len();

        let first_duplicate = controller.enqueue_track(a).expect("append duplicate");
        let second_duplicate = controller
            .enqueue_track(a)
            .expect("append second duplicate");
        assert_ne!(first_duplicate, current);
        assert_ne!(first_duplicate, second_duplicate);
        assert_eq!(controller.queue_len(), initial_len + 2);
        assert_eq!(controller.current_queue_entry_id(), Some(current));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(
            controller.queue_entries().last().map(|entry| entry.track()),
            Some(a)
        );

        let next_entry = controller.play_next_track(b).expect("play next");
        assert_eq!(
            controller.queue_entry_index(next_entry),
            Some(current_index + 1)
        );
        assert_eq!(controller.current_queue_entry_id(), Some(current));
        assert_eq!(controller.player_state(), PlaybackState::Playing);

        controller
            .move_queue_entry_by(current, 1)
            .expect("move current");
        assert_eq!(controller.current_queue_entry_id(), Some(current));
        assert_eq!(
            controller.queue_entry_index(current),
            Some(current_index + 1),
            "the current entry moves by identity, not by index"
        );
        assert_eq!(controller.player_state(), PlaybackState::Playing);

        // Removing a non-current duplicate changes neither selection nor
        // playback, and leaves the other duplicate distinct.
        controller
            .remove_queue_entry(first_duplicate)
            .expect("remove one duplicate");
        assert_eq!(controller.current_queue_entry_id(), Some(current));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert!(
            controller
                .queue_entries()
                .iter()
                .any(|entry| entry.id() == second_duplicate)
        );

        let before_remove: Vec<QueueEntryId> = controller
            .queue_entries()
            .iter()
            .map(|entry| entry.id())
            .collect();
        let removed_index = controller
            .queue_entry_index(current)
            .expect("current index");
        let replacement = if removed_index + 1 < before_remove.len() {
            Some(before_remove[removed_index + 1])
        } else if before_remove.len() > 1 {
            Some(before_remove[before_remove.len() - 2])
        } else {
            None
        };
        controller
            .remove_queue_entry(current)
            .expect("remove current");
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert_eq!(controller.current_queue_entry_id(), replacement);

        let loaded = QueueStore::new(&queue_file)
            .load()
            .expect("queue file loads")
            .expect("queue file exists");
        assert!(
            !loaded.auto_library,
            "a structural edit disables auto-follow"
        );
        assert_eq!(loaded.queue.current_entry_id(), replacement);
        assert_eq!(
            loaded
                .queue
                .entries()
                .iter()
                .map(|entry| (entry.id(), entry.track()))
                .collect::<Vec<_>>(),
            queue_snapshot(&controller).0
        );
        controller.shutdown();
    }

    #[test]
    fn play_track_appends_when_absent_and_prefers_the_first_duplicate() {
        let dir = TempDir::new("queue-play-track");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);

        controller.play_track(a);
        let first = controller.current_queue_entry_id().expect("first entry");
        let duplicate = controller.enqueue_track(a).expect("duplicate");
        assert_ne!(first, duplicate);
        controller.play_track(a);
        assert_eq!(
            controller.current_queue_entry_id(),
            Some(first),
            "library play selects the first matching entry"
        );

        let entries: Vec<QueueEntryId> = controller
            .queue_entries()
            .iter()
            .map(|entry| entry.id())
            .collect();
        for entry in entries {
            controller
                .remove_queue_entry(entry)
                .expect("empty the queue");
        }
        assert_eq!(controller.queue_len(), 0);

        controller
            .play_next_track(a)
            .expect("insert next into empty queue");
        assert_eq!(controller.queue_len(), 1);
        assert!(controller.current_queue_entry_id().is_some());
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        let empty_current = controller.current_queue_entry_id().expect("current");
        controller
            .remove_queue_entry(empty_current)
            .expect("empty again");

        controller.play_track(a);
        assert_eq!(controller.queue_len(), 1);
        assert_eq!(controller.current_id(), Some(a));
        assert_eq!(
            controller.player_state(),
            PlaybackState::Playing,
            "play-now adds an absent track and starts it"
        );
        controller.shutdown();
    }

    #[test]
    fn controller_seek_clamps_and_preserves_playback_and_queue_state() {
        let dir = TempDir::new("controller-seek");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let current = controller.current_queue_entry_id();
        controller.play_track(a);
        let total = controller.current_duration().expect("duration");

        let target = controller
            .seek(Duration::from_millis(100))
            .expect("middle seek");
        assert_eq!(target, Duration::from_millis(100));
        assert!((controller.position().as_millis() as i64 - 100).abs() <= 2);
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.current_queue_entry_id(), current);

        controller.pause();
        let target = controller
            .seek(Duration::from_secs(10))
            .expect("clamped seek");
        assert_eq!(target, total);
        assert_eq!(controller.player_state(), PlaybackState::Paused);
        assert!((controller.position().as_millis() as i64 - total.as_millis() as i64).abs() <= 2);
        assert_eq!(controller.current_queue_entry_id(), current);

        controller.stop();
        let target = controller
            .seek(Duration::from_millis(50))
            .expect("stopped seek");
        assert_eq!(target, Duration::from_millis(50));
        assert!((controller.position().as_millis() as i64 - 50).abs() <= 2);
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert_eq!(controller.current_queue_entry_id(), current);

        let queue = QueueStore::new(legacy_queue_file(&dir))
            .load()
            .expect("queue loads")
            .expect("queue exists");
        assert_eq!(queue.queue.current_entry_id(), current);
        controller.shutdown();
    }

    #[test]
    fn queue_gap_moves_handle_beginning_middle_and_end_without_touching_playback() {
        let dir = TempDir::new("queue-gap-moves");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let b = id_of(&controller, &fx.b);
        controller.play_track(a);
        controller.enqueue_track(a).expect("duplicate");
        controller.set_repeat_mode(RepeatMode::All);
        let current = controller.current_queue_entry_id().expect("current");
        let current_track = controller
            .current_track()
            .map(|track| track.title().to_owned());
        let len = controller.queue_len();
        let b_entry = controller
            .queue_entries()
            .iter()
            .find(|entry| entry.track() == b)
            .map(|entry| entry.id())
            .expect("b entry");

        assert!(
            controller
                .move_queue_entry_to_gap(b_entry, 0)
                .expect("beginning move")
        );
        assert_eq!(controller.queue_entries()[0].id(), b_entry);
        assert_eq!(controller.current_queue_entry_id(), Some(current));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.repeat_mode(), RepeatMode::All);

        assert!(
            controller
                .move_queue_entry_to_gap(b_entry, 2)
                .expect("middle move")
        );
        assert_eq!(controller.queue_entry_index(b_entry), Some(1));
        assert_eq!(controller.current_queue_entry_id(), Some(current));

        assert!(
            controller
                .move_queue_entry_to_gap(current, len)
                .expect("end move")
        );
        assert_eq!(
            controller.queue_entry_index(current),
            Some(controller.queue_len() - 1)
        );
        assert_eq!(controller.current_queue_entry_id(), Some(current));
        assert_eq!(
            controller
                .current_track()
                .map(|track| track.title().to_owned()),
            current_track
        );
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.repeat_mode(), RepeatMode::All);

        let before_invalid = queue_snapshot(&controller);
        assert!(
            !controller
                .move_queue_entry_to_gap(current, len + 1)
                .expect("invalid gap is a no-op")
        );
        assert!(
            !controller
                .move_queue_entry_to_gap(QueueEntryId::new(999), 0)
                .expect("absent entry is a no-op")
        );
        assert_eq!(queue_snapshot(&controller), before_invalid);

        let loaded = QueueStore::new(legacy_queue_file(&dir))
            .load()
            .expect("queue loads")
            .expect("queue exists");
        assert!(!loaded.auto_library);
        assert_eq!(loaded.queue.current_entry_id(), Some(current));
        assert_eq!(
            loaded
                .queue
                .entries()
                .iter()
                .map(|entry| entry.id())
                .collect::<Vec<_>>(),
            queue_snapshot(&controller)
                .0
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        );
        controller.shutdown();
    }

    #[test]
    fn queue_removal_switches_current_and_emptying_stops_cleanly() {
        let dir = TempDir::new("queue-empty");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        controller.play_track(a);
        let entries: Vec<QueueEntryId> = controller
            .queue_entries()
            .iter()
            .map(|entry| entry.id())
            .collect();
        assert!(entries.len() >= 2);

        for entry in entries {
            controller
                .remove_queue_entry(entry)
                .expect("queue entry removal is infallible once found");
        }
        assert_eq!(controller.queue_len(), 0);
        assert_eq!(controller.current_queue_entry_id(), None);
        assert_eq!(controller.player_state(), PlaybackState::Stopped);

        controller.play();
        assert!(
            controller
                .last_error()
                .is_some_and(|error| error.contains("Nothing to play"))
        );
        controller.next();
        controller.previous();
        assert_eq!(controller.current_queue_entry_id(), None);

        let loaded = QueueStore::new(legacy_queue_file(&dir))
            .load()
            .expect("empty queue loads")
            .expect("queue file exists");
        assert!(!loaded.auto_library);
        assert!(loaded.queue.is_empty());
        assert_eq!(loaded.queue.current_entry_id(), None);
        controller.shutdown();
    }

    #[test]
    fn queue_navigation_uses_entry_identity_and_persists_selection() {
        let dir = TempDir::new("queue-navigation");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        controller.play_track(a);
        let first = controller.current_queue_entry_id().expect("first");
        let duplicate = controller.enqueue_track(a).expect("duplicate at end");
        controller
            .play_queue_entry(duplicate)
            .expect("played duplicate entry");
        assert_eq!(controller.current_queue_entry_id(), Some(duplicate));

        let before_duplicate = controller.queue_entries()[controller
            .queue_entry_index(duplicate)
            .expect("duplicate index")
            - 1]
        .id();
        controller.previous();
        assert_eq!(
            controller.current_queue_entry_id(),
            Some(before_duplicate),
            "previous navigates by entry position, not by track ID"
        );

        controller.set_repeat_mode(RepeatMode::All);
        controller
            .play_queue_entry(duplicate)
            .expect("played duplicate entry");
        controller.next();
        assert_eq!(
            controller.current_queue_entry_id(),
            Some(first),
            "repeat-all wraps from the duplicate to the first entry"
        );

        controller.set_repeat_mode(RepeatMode::Off);
        controller
            .play_queue_entry(duplicate)
            .expect("played duplicate entry");
        controller.next();
        assert_eq!(
            controller.current_queue_entry_id(),
            Some(duplicate),
            "repeat-off keeps the selection at the last entry"
        );
        assert_eq!(controller.player_state(), PlaybackState::Stopped);

        let loaded = QueueStore::new(legacy_queue_file(&dir))
            .load()
            .expect("queue loads")
            .expect("queue exists");
        assert_eq!(loaded.queue.current_entry_id(), Some(duplicate));
        controller.shutdown();
    }

    #[test]
    fn queue_order_and_current_survive_restart() {
        let dir = TempDir::new("queue-restart");
        let fx = fixtures(&dir);
        let a;
        let b;
        let expected;
        {
            let (mut controller, control) = boot(&dir);
            scanned(&mut controller, &control, fx.root.clone());
            a = id_of(&controller, &fx.a);
            b = id_of(&controller, &fx.b);
            controller.play_track(a);
            controller.enqueue_track(a).expect("duplicate");
            controller.play_next_track(b).expect("next");
            controller
                .move_queue_entry_by(controller.current_queue_entry_id().expect("current"), -1)
                .expect("reorder");
            expected = queue_snapshot(&controller);
            controller.shutdown();
        }

        let (controller, _control) = boot(&dir);
        assert_eq!(queue_snapshot(&controller), expected);
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        let expected_track = expected.1.and_then(|entry| {
            controller
                .queue_entries()
                .iter()
                .find(|candidate| candidate.id() == entry)
                .map(|candidate| candidate.track())
        });
        assert_eq!(controller.current_id(), expected_track);
        let loaded = QueueStore::new(legacy_queue_file(&dir))
            .load()
            .expect("queue loads")
            .expect("queue exists");
        assert!(!loaded.auto_library);
        controller.shutdown();
    }

    #[test]
    fn malformed_queue_file_is_reported_preserved_and_never_overwritten() {
        let dir = TempDir::new("queue-malformed");
        let state = dir.path().join("state");
        fs::create_dir_all(&state).expect("state dir");
        let queue_file = legacy_queue_file(&dir);
        let original = b"{ this is not a queue";
        fs::write(&queue_file, original).expect("malformed queue fixture");

        let (mut controller, control) = boot(&dir);
        assert!(
            controller
                .last_error()
                .is_some_and(|error| error.contains("playback queue"))
        );
        let fx = fixtures(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        assert!(controller.queue_len() > 0, "library is still usable");
        let a = id_of(&controller, &fx.a);
        controller
            .enqueue_track(a)
            .expect("queue still mutable in memory");
        assert_eq!(
            fs::read(&queue_file).expect("original bytes remain"),
            original,
            "a malformed queue file is never replaced during the session"
        );
        assert!(QueueStore::new(&queue_file).load().is_err());
        controller.shutdown();
    }

    #[test]
    fn missing_and_unplayable_queue_entries_are_kept_without_corruption() {
        let dir = TempDir::new("queue-missing");
        let fx = fixtures(&dir);
        let a;
        let unplayable_entry;
        {
            let (mut controller, control) = boot(&dir);
            scanned(&mut controller, &control, fx.root.clone());
            a = id_of(&controller, &fx.a);
            let missing_path = fx.root.join("c.aif");
            unplayable_entry = controller
                .queue_entries()
                .iter()
                .find(|entry| {
                    controller
                        .track(entry.track())
                        .is_some_and(|track| track.source().path() == missing_path)
                })
                .map(|entry| entry.id())
                .expect("indexed non-playable track is in the default queue");
            let len = controller.queue_len();
            controller
                .play_queue_entry(unplayable_entry)
                .expect("selecting an indexed entry is not a queue error");
            assert_eq!(controller.player_state(), PlaybackState::Stopped);
            assert_eq!(controller.queue_len(), len);
            assert_eq!(controller.current_queue_entry_id(), Some(unplayable_entry));
            assert!(
                controller
                    .last_error()
                    .is_some_and(|error| error.contains("cannot be played"))
            );

            // Replace the persisted queue with one that also references a
            // track ID absent from this library. Reopening must keep it.
            let mut queue = Queue::new();
            queue.push(a);
            let missing = queue.push(TrackId::new(999));
            queue.set_current_entry(missing).expect("missing current");
            QueueStore::new(legacy_queue_file(&dir))
                .save(&queue, false)
                .expect("write missing-entry queue");
            controller.shutdown();
        }

        let (mut controller, _control) = boot(&dir);
        assert_eq!(controller.queue_len(), 2);
        assert_eq!(
            controller.current_queue_entry_id(),
            Some(QueueEntryId::new(2))
        );
        assert!(controller.current_queue_entry_id().is_some());
        assert_eq!(controller.current_track(), None);
        assert!(
            controller
                .queue_entries()
                .iter()
                .any(|entry| entry.track() == TrackId::new(999))
        );
        let a = id_of(&controller, &fx.a);
        controller
            .enqueue_track(a)
            .expect("existing track still addable");
        assert!(
            controller
                .queue_entries()
                .iter()
                .any(|entry| entry.track() == TrackId::new(999))
        );
        controller.shutdown();
    }

    #[test]
    fn queue_auto_follows_scans_until_the_user_edits_it() {
        let dir = TempDir::new("queue-auto");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let before = controller.queue_len();
        write_dc_wav(&fx.root, "e.wav", 4_410, 0.2);
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(
            controller.queue_len(),
            before + 1,
            "an untouched default queue follows new scan results"
        );

        let a = id_of(&controller, &fx.a);
        controller.enqueue_track(a).expect("structural edit");
        let after_edit = controller.queue_len();
        write_dc_wav(&fx.root, "f.wav", 4_410, 0.2);
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(
            controller.queue_len(),
            after_edit,
            "a user-managed queue is not silently extended by scanning"
        );
        controller.shutdown();
    }

    #[test]
    fn queue_is_isolated_between_legacy_and_portable_libraries() {
        let dir = TempDir::new("queue-isolation");
        let fx = fixtures(&dir);
        let index = dir.path().join("state").join("library-index.json");
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        controller.play_track(a);
        controller.enqueue_track(a).expect("legacy duplicate");
        let legacy_snapshot = queue_snapshot(&controller);

        let portable_root = dir.path().join("portable");
        fs::create_dir_all(&portable_root).expect("portable root");
        controller
            .switch_to_new_portable_library(&portable_root)
            .expect("portable created and activated");
        assert_eq!(controller.queue_len(), 0, "new library starts empty");
        let portable_music = portable_root.join("music");
        fs::create_dir_all(&portable_music).expect("portable music dir");
        write_dc_wav(&portable_music, "a.wav", 11_025, 0.4);
        write_dc_wav(&portable_music, "b.wav", 11_025, 0.3);
        scanned(&mut controller, &control, portable_music.clone());
        assert_eq!(controller.queue_len(), 2);
        let b = id_of(&controller, &portable_music.join("b.wav"));
        controller.play_track(b);
        controller.enqueue_track(b).expect("portable duplicate");
        let portable_snapshot = queue_snapshot(&controller);

        controller
            .switch_to_legacy_index(Some(index.clone()))
            .expect("legacy switch");
        assert_eq!(queue_snapshot(&controller), legacy_snapshot);
        let legacy_loaded = QueueStore::new(legacy_queue_file(&dir))
            .load()
            .expect("legacy queue loads")
            .expect("legacy queue exists");
        assert!(!legacy_loaded.auto_library);

        controller
            .switch_to_portable(&portable_root)
            .expect("portable switch");
        assert_eq!(queue_snapshot(&controller), portable_snapshot);
        let portable_loaded = QueueStore::new(portable_queue_file(&portable_root))
            .load()
            .expect("portable queue loads")
            .expect("portable queue exists");
        assert!(!portable_loaded.auto_library);
        controller.shutdown();
    }

    #[test]
    fn protected_index_blocks_queue_writes_until_explicit_recovery() {
        let dir = TempDir::new("queue-protected");
        let state = dir.path().join("state");
        fs::create_dir_all(&state).expect("state dir");
        let index = state.join("library-index.json");
        fs::write(&index, b"not a valid index").expect("unreadable index");
        let queue_file = legacy_queue_file(&dir);
        let mut queue = Queue::new();
        let entry = queue.push(TrackId::new(1));
        QueueStore::new(&queue_file)
            .save(&queue, false)
            .expect("queue fixture");

        let (mut controller, _control) = boot(&dir);
        assert!(controller.legacy_index_write_protected());
        assert_eq!(controller.queue_len(), 1);
        controller
            .remove_queue_entry(entry)
            .expect("queue is editable in memory");
        assert_eq!(controller.queue_len(), 0);

        let preserved = QueueStore::new(&queue_file)
            .load()
            .expect("queue loads")
            .expect("queue exists");
        assert_eq!(
            preserved.queue.len(),
            1,
            "protected state blocks queue writes"
        );

        controller
            .replace_unreadable_legacy_index()
            .expect("explicit recovery");
        assert!(!controller.legacy_index_write_protected());
        let flushed = QueueStore::new(&queue_file)
            .load()
            .expect("queue loads")
            .expect("queue exists");
        assert!(flushed.queue.is_empty());
        controller.shutdown();
    }

    #[test]
    fn portable_import_copies_multiple_files_and_dedupes_reimports() {
        let dir = TempDir::new("portable-import");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        create_portable_library(&root).expect("create portable library");
        let canonical_root = root.canonicalize().expect("canonical root");
        let (mut controller, _control) = boot_portable_at(&root).expect("portable boot");

        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        let song = write_dc_wav(&outside, "café.wav", 4_410, 0.2);
        let other = write_dc_wav(&outside, "other.wav", 4_410, 0.3);
        let original_bytes = fs::read(&song).expect("read source");

        let report = controller
            .import_files(&[song.clone(), other.clone()], true)
            .expect("copy imports");
        assert_eq!(report.added, 2);
        assert_eq!(report.updated, 0);
        assert_eq!(report.deduplicated, 0);
        assert_eq!(report.copied.len(), 2);
        assert!(report.issues.is_empty(), "{:?}", report.issues);
        assert_eq!(fs::read(&song).expect("source unchanged"), original_bytes);
        assert_eq!(
            fs::read(&other).expect("source unchanged"),
            fs::read(other).expect("other")
        );

        let copied = controller
            .tracks()
            .iter()
            .find(|(_, track)| track.title() == "café")
            .map(|(_, track)| track.source().path().to_path_buf())
            .expect("imported café track");
        assert!(copied.starts_with(&canonical_root));
        assert!(copied.starts_with(&canonical_root));
        assert_eq!(fs::read(&copied).expect("copied"), original_bytes);
        assert_eq!(controller.tracks().len(), 2);

        let ids_before: Vec<_> = controller.tracks().iter().map(|(id, _)| *id).collect();
        let again = controller
            .import_files(&[song], true)
            .expect("reimport same source");
        assert_eq!(again.added, 0);
        assert_eq!(again.deduplicated, 1, "{again:?}");
        assert_eq!(again.copied.len(), 0);
        assert!(again.issues.is_empty());
        assert_eq!(controller.tracks().len(), 2);
        assert_eq!(
            controller
                .tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            ids_before,
            "reimport must not mint a duplicate track ID"
        );

        // Without copy, an external file is refused with no write.
        let outside_again = write_dc_wav(&outside, "refused.wav", 4_410, 0.1);
        let refused = controller
            .import_files(&[outside_again], false)
            .expect("partial import report");
        assert_eq!(refused.added, 0);
        assert_eq!(refused.issues.len(), 1);
        assert!(refused.issues[0].reason().contains("outside"));
        assert_eq!(controller.tracks().len(), 2);

        // A file already inside the library root is referenced in place.
        let inside = write_dc_wav(&canonical_root, "inside.wav", 4_410, 0.4);
        let linked = controller
            .import_files(std::slice::from_ref(&inside), false)
            .expect("in-place import");
        assert_eq!(linked.added, 1);
        assert_eq!(linked.copied.len(), 0);
        assert_eq!(controller.tracks().len(), 3);
        assert!(
            controller
                .tracks()
                .iter()
                .any(|(_, track)| track.source().path() == inside)
        );
        controller.shutdown();
    }

    #[test]
    fn portable_track_edits_persist_without_touching_audio_or_track_ids() {
        let dir = TempDir::new("portable-edit");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        create_portable_library(&root).expect("create portable library");
        let canonical_root = root.canonicalize().expect("canonical");
        let first = write_dc_wav(&canonical_root, "first.wav", 4_410, 0.2);
        let second = write_dc_wav(&canonical_root, "second.wav", 4_410, 0.3);
        let (mut controller, _control) = boot_portable_at(&root).expect("portable boot");
        controller
            .import_files(&[first.clone(), second.clone()], false)
            .expect("import");
        let ids: Vec<_> = controller.tracks().iter().map(|(id, _)| *id).collect();
        let first_id = controller
            .tracks()
            .iter()
            .find(|(_, track)| track.source().path() == first)
            .map(|(id, _)| *id)
            .expect("first id");
        let second_id = controller
            .tracks()
            .iter()
            .find(|(_, track)| track.source().path() == second)
            .map(|(id, _)| *id)
            .expect("second id");
        let current = controller.current_queue_entry_id();
        let first_audio = fs::read(&first).expect("first audio");

        let artist = controller.create_artist("Old Name").expect("artist");
        controller
            .edit_track_metadata(
                first_id,
                TrackMetadataEdit {
                    title: Some("Edited Title".to_owned()),
                    artist_id: Some(artist),
                    album: Some("Edited Album".to_owned()),
                    track_number: Some(4),
                    disc_number: Some(2),
                },
            )
            .expect("edit first");
        controller
            .edit_track_metadata(
                second_id,
                TrackMetadataEdit {
                    title: Some("Second Title".to_owned()),
                    artist_id: Some(artist),
                    ..TrackMetadataEdit::default()
                },
            )
            .expect("edit second");

        assert_eq!(fs::read(&first).expect("audio unchanged"), first_audio);
        let edited = controller.track(first_id).expect("edited track");
        assert_eq!(edited.title(), "Edited Title");
        assert_eq!(edited.artist(), Some("Old Name"));
        assert_eq!(edited.album(), Some("Edited Album"));
        assert_eq!(edited.track_number(), Some(4));
        assert_eq!(edited.disc_number(), Some(2));
        assert_eq!(controller.track_artist_id(first_id), Some(artist));
        assert_eq!(controller.current_queue_entry_id(), current);
        assert_eq!(
            controller
                .tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            ids
        );

        controller
            .rename_artist(artist, "Renamed Artist")
            .expect("rename artist");
        assert_eq!(
            controller.track(first_id).and_then(Track::artist),
            Some("Renamed Artist")
        );
        assert_eq!(
            controller.track(second_id).and_then(Track::artist),
            Some("Renamed Artist")
        );
        assert_eq!(
            controller.track_artist_id(second_id),
            Some(artist),
            "renaming changes the canonical name, not the stable identity"
        );
        controller.shutdown();

        let (reopened, _control) = boot_portable_at(&root).expect("reopen");
        assert_eq!(
            reopened
                .tracks()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(
            reopened.track(first_id).map(Track::title),
            Some("Edited Title")
        );
        assert_eq!(
            reopened.track(first_id).and_then(Track::artist),
            Some("Renamed Artist")
        );
        assert_eq!(reopened.track_artist_id(first_id), Some(artist));
        assert_eq!(
            reopened.artist(artist).map(|a| a.name().to_owned()),
            Some("Renamed Artist".to_owned())
        );
        reopened.shutdown();
    }

    #[test]
    fn portable_same_named_artists_keep_distinct_tracks() {
        let dir = TempDir::new("portable-artist-duplicates");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        create_portable_library(&root).expect("create portable library");
        let canonical_root = root.canonicalize().expect("canonical");
        let first = write_dc_wav(&canonical_root, "one.wav", 4_410, 0.2);
        let second = write_dc_wav(&canonical_root, "two.wav", 4_410, 0.3);
        let (mut controller, _control) = boot_portable_at(&root).expect("portable boot");
        controller
            .import_files(&[first, second], false)
            .expect("import");
        let ids: Vec<_> = controller.tracks().iter().map(|(id, _)| *id).collect();
        let first_artist = controller.create_artist("Same Name").expect("first");
        let second_artist = controller.create_artist("Same Name").expect("second");
        assert_ne!(first_artist, second_artist);
        controller
            .edit_track_metadata(
                ids[0],
                TrackMetadataEdit {
                    title: Some("One".to_owned()),
                    artist_id: Some(first_artist),
                    ..TrackMetadataEdit::default()
                },
            )
            .expect("edit first");
        controller
            .edit_track_metadata(
                ids[1],
                TrackMetadataEdit {
                    title: Some("Two".to_owned()),
                    artist_id: Some(second_artist),
                    ..TrackMetadataEdit::default()
                },
            )
            .expect("edit second");
        controller
            .rename_artist(first_artist, "First Identity")
            .expect("rename first");
        assert_eq!(
            controller.track(ids[0]).and_then(Track::artist),
            Some("First Identity")
        );
        assert_eq!(
            controller.track(ids[1]).and_then(Track::artist),
            Some("Same Name"),
            "renaming one same-named artist must not touch the other"
        );
        assert_eq!(controller.track_artist_id(ids[1]), Some(second_artist));
        controller.shutdown();
    }

    #[test]
    fn portable_import_never_overwrites_a_name_collision() {
        let dir = TempDir::new("portable-import-collision");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        create_portable_library(&root).expect("create portable library");
        let canonical_root = root.canonicalize().expect("canonical");
        let first = dir.path().join("first-source");
        let second = dir.path().join("second-source");
        fs::create_dir_all(&first).expect("first source dir");
        fs::create_dir_all(&second).expect("second source dir");
        let first_audio = write_dc_wav(&first, "same.wav", 4_410, 0.2);
        let second_audio = write_dc_wav(&second, "same.wav", 4_410, -0.4);
        let first_bytes = fs::read(&first_audio).expect("first bytes");
        let second_bytes = fs::read(&second_audio).expect("second bytes");
        let (mut controller, _control) = boot_portable_at(&root).expect("portable boot");

        controller
            .import_files(std::slice::from_ref(&first_audio), true)
            .expect("first import");
        let second_report = controller
            .import_files(std::slice::from_ref(&second_audio), true)
            .expect("second import");
        assert_eq!(second_report.added, 1);
        assert_eq!(second_report.copied.len(), 1);
        let copied_second = &second_report.copied[0];
        assert_ne!(copied_second, &canonical_root.join("same.wav"));
        assert_eq!(
            fs::read(canonical_root.join("same.wav")).expect("first copy"),
            first_bytes,
            "the existing copy must not be overwritten"
        );
        assert_eq!(fs::read(copied_second).expect("second copy"), second_bytes);
        assert_eq!(controller.tracks().len(), 2);
        assert_eq!(
            controller
                .import_files(std::slice::from_ref(&second_audio), true)
                .expect("second reimport")
                .deduplicated,
            1
        );
        controller.shutdown();
    }

    #[test]
    fn portable_import_missing_file_is_retained_as_missing_and_edit_is_portable_only() {
        let dir = TempDir::new("portable-missing");
        let root = dir.path().join("library");
        fs::create_dir_all(&root).expect("library root");
        create_portable_library(&root).expect("create portable library");
        let canonical_root = root.canonicalize().expect("canonical");
        let file = write_dc_wav(&canonical_root, "gone.wav", 4_410, 0.2);
        let (mut controller, _control) = boot_portable_at(&root).expect("portable boot");
        controller
            .import_files(std::slice::from_ref(&file), false)
            .expect("import");
        let id = controller.tracks()[0].0;

        fs::remove_file(&file).expect("remove audio");
        let report = controller.reconcile_portable_library().expect("reconcile");
        assert_eq!(report.missing_count(), 1);
        assert_eq!(
            controller.track_availability(id),
            Some(flux_library::Availability::Missing)
        );
        assert!(
            controller.track(id).is_some(),
            "missing records are retained"
        );
        controller.shutdown();

        let (mut legacy, _control) = boot(&dir);
        assert!(legacy.create_artist("Nope").is_err());
        assert!(
            legacy
                .import_files(&[file], true)
                .expect_err("legacy import is refused")
                .contains("portable")
        );
        legacy.shutdown();
    }

    #[test]
    fn read_only_player_changes_do_not_write_the_queue() {
        let dir = TempDir::new("queue-write-only-changes");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let queue_file = legacy_queue_file(&dir);
        assert!(queue_file.exists());
        fs::remove_file(&queue_file).expect("remove queue for observation");

        controller.set_volume(0.42);
        controller.cycle_repeat_mode();
        controller.toggle_play_pause();
        controller.stop();
        controller.pause();
        assert!(
            !queue_file.exists(),
            "volume, repeat, pause, and stop are not queue writes"
        );

        controller.next();
        assert!(queue_file.exists(), "a selection change is persisted");
        let loaded = QueueStore::new(&queue_file)
            .load()
            .expect("queue loads")
            .expect("queue exists");
        assert_eq!(
            loaded.queue.current_entry_id(),
            controller.current_queue_entry_id()
        );
        controller.shutdown();
    }

    fn boot_canonical_at(root: &Path) -> (Controller, NullControl) {
        let (engine, control) = EngineHandle::spawn_null(48_000, 1);
        let controller = Controller::new_canonical(engine, AudioStatus::Ready, root)
            .expect("open canonical controller");
        (controller, control)
    }

    fn canonical_track_fixture(dir: &TempDir) -> (PathBuf, PathBuf) {
        let root = dir.path().join("FluxLibrary");
        create_canonical_library(&root).expect("create canonical library");
        let source = dir.path().join("Artist/Album/01 - Song.wav");
        fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        write_dc_wav(
            source.parent().expect("parent"),
            "01 - Song.wav",
            4_410,
            0.2,
        );
        (root, source)
    }

    #[test]
    fn canonical_controller_imports_copies_and_reopens_with_stable_ids() {
        let dir = TempDir::new("canonical-open");
        let (root, source) = canonical_track_fixture(&dir);
        let source_before = fs::read(&source).expect("source");
        let (mut controller, _control) = boot_canonical_at(&root);
        assert_eq!(controller.storage_mode(), StorageMode::Canonical);
        assert_eq!(controller.canonical_root(), Some(root.as_path()));
        assert!(controller.tracks().is_empty());

        let report = controller
            .import_files(std::slice::from_ref(&source), false)
            .expect("canonical import");
        assert_eq!(report.added, 1, "issues: {:?}", report.issues);
        assert_eq!(report.issues.len(), 0);
        assert_eq!(fs::read(&source).expect("source after"), source_before);
        let (id, track) = controller.tracks()[0].clone();
        assert_eq!(track.title(), "Song");
        assert_eq!(track.artist(), Some("Artist"));
        assert_eq!(track.album(), Some("Album"));
        assert!(track.source().path().starts_with(&root));
        assert_ne!(track.source().path(), source);
        assert!(fs::read(track.source().path()).is_ok());
        let artist_id = controller.track_artist_id(id).expect("artist");
        controller
            .rename_artist(artist_id, "Renamed Artist")
            .expect("rename");
        assert_eq!(
            controller.track(id).and_then(Track::artist),
            Some("Renamed Artist")
        );
        assert_eq!(controller.track_artist_id(id), Some(artist_id));

        let entry = controller.enqueue_track(id).expect("enqueue");
        let current = controller.current_queue_entry_id();
        assert!(current.is_some());
        assert!(controller.queue_entry_index(entry).is_some());
        controller.shutdown();

        let (controller, _control) = boot_canonical_at(&root);
        assert_eq!(controller.storage_mode(), StorageMode::Canonical);
        assert_eq!(controller.tracks().len(), 1);
        assert_eq!(controller.tracks()[0].0, id);
        assert_eq!(
            controller.track(id).and_then(Track::artist),
            Some("Renamed Artist")
        );
        assert_eq!(controller.track_artist_id(id), Some(artist_id));
        assert_eq!(controller.current_queue_entry_id(), current);
        assert!(root.join("queue.flux").is_file());
        assert!(
            !root.join(".flux").exists(),
            "no legacy portable backend is created"
        );
        controller.shutdown();
    }

    #[test]
    fn canonical_import_is_copy_only_and_never_reads_tags() {
        use crate::import::{
            ImportDecision, ImportProgress, ImportRequest, ImportStorage, prepare_import_files,
            prepare_import_plan,
        };
        use std::sync::atomic::AtomicBool;
        use std::sync::{Arc, Mutex};

        let dir = TempDir::new("canonical-import");
        let (root, source) = canonical_track_fixture(&dir);
        let (mut controller, _control) = boot_canonical_at(&root);
        let request = ImportRequest {
            sources: vec![source.clone()],
            recursive: false,
            storage: ImportStorage::Reference,
            library_root: root.clone(),
            existing_tracks: controller.tracks().to_vec(),
            existing_artists: Vec::new(),
        };
        let progress = Arc::new(Mutex::new(ImportProgress::default()));
        let cancel = Arc::new(AtomicBool::new(false));
        let mut plan = prepare_import_plan(request, Arc::clone(&progress), cancel);
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].title(), "Song");
        assert_eq!(plan.candidates[0].artist_name(), Some("Artist"));
        assert_eq!(plan.candidates[0].album(), Some("Album"));
        plan.set_storage(ImportStorage::Copy);
        plan.candidates[0].set_decision(ImportDecision::ImportNew);
        let prepared = prepare_import_files(
            &plan,
            Arc::clone(&progress),
            Arc::new(AtomicBool::new(false)),
        );
        assert!(
            prepared.created_files.is_empty(),
            "canonical engine owns staging"
        );
        let report = controller
            .apply_import_plan(&plan, &prepared)
            .expect("canonical apply");
        assert_eq!(report.imported, 1);
        assert_eq!(
            fs::read(&source).expect("source"),
            fs::read(&source).expect("source")
        );
        let track = controller.tracks()[0].1.clone();
        assert!(track.source().path().starts_with(&root));
        assert!(fs::read(track.source().path()).is_ok());
        controller.shutdown();
    }

    #[test]
    fn canonical_switching_resets_queue_and_preserves_each_library() {
        let dir = TempDir::new("canonical-switch");
        let (root_a, source) = canonical_track_fixture(&dir);
        let root_b = dir.path().join("OtherLibrary");
        create_canonical_library(&root_b).expect("second library");
        let (mut controller, _control) = boot_canonical_at(&root_a);
        controller.import_files(&[source], true).expect("import");
        let id = controller.tracks()[0].0;
        assert!(controller.queue_len() >= 1);
        controller.switch_to_canonical(&root_b).expect("switch b");
        assert!(controller.tracks().is_empty());
        assert_eq!(controller.queue_len(), 0);
        controller.switch_to_canonical(&root_a).expect("switch a");
        assert_eq!(controller.tracks()[0].0, id);
        assert!(controller.queue_len() >= 1);
        controller.shutdown();
    }

    #[test]
    fn canonical_missing_audio_is_reported_without_losing_identity() {
        let dir = TempDir::new("canonical-missing");
        let (root, source) = canonical_track_fixture(&dir);
        let (mut controller, _control) = boot_canonical_at(&root);
        controller.import_files(&[source], true).expect("import");
        let id = controller.tracks()[0].0;
        let audio = controller
            .track(id)
            .expect("track")
            .source()
            .path()
            .to_path_buf();
        controller.shutdown();
        fs::remove_file(audio).expect("remove managed audio");
        let (controller, _control) = boot_canonical_at(&root);
        assert!(controller.track(id).is_some());
        assert_eq!(
            controller.track_availability(id),
            Some(flux_library::Availability::Missing)
        );
        controller.shutdown();
    }

    #[test]
    fn canonical_playlists_ratings_and_lyrics_survive_restart() {
        let dir = TempDir::new("canonical-data");
        let (root, source) = canonical_track_fixture(&dir);
        let (mut controller, _control) = boot_canonical_at(&root);
        controller.import_files(&[source], true).expect("import");
        let id = controller.tracks()[0].0;
        controller.set_rating(id, Some(5), true).expect("rating");
        controller
            .set_lyrics(id, Some("[00:00.00]hello"))
            .expect("lyrics");
        let playlist = controller
            .set_playlist(None, "Favorites", vec![id])
            .expect("playlist");
        assert_eq!(
            controller.track_lyrics(id).as_deref(),
            Some("[00:00.00]hello")
        );
        controller.shutdown();

        let (controller, _control) = boot_canonical_at(&root);
        assert_eq!(
            controller.rating(id),
            Some(flux_library::canonical::TrackRating {
                rating: Some(5),
                favorite: true
            })
        );
        assert_eq!(
            controller
                .playlists()
                .into_iter()
                .find(|entry| entry.id == playlist)
                .map(|entry| entry.track_ids),
            Some(vec![id])
        );
        assert_eq!(
            controller.track_lyrics(id).as_deref(),
            Some("[00:00.00]hello")
        );
        controller.shutdown();
    }

    #[test]
    fn app_canonical_migration_workflow_preserves_source() {
        let dir = TempDir::new("canonical-migrate-app");
        let source = dir.path().join("old-portable");
        fs::create_dir_all(&source).expect("source dir");
        create_portable_library(&source).expect("portable source");
        let source_audio = source.join("Artist/Album/01 - Song.wav");
        fs::create_dir_all(source_audio.parent().expect("parent")).expect("dirs");
        write_dc_wav(
            source_audio.parent().expect("parent"),
            "01 - Song.wav",
            4_410,
            0.2,
        );
        let mut store = flux_library::PortableStore::open(&source)
            .expect("open portable")
            .store;
        let artist = store.create_artist("Artist").expect("artist");
        let mut record = imported_record(
            1,
            "Artist/Album/01 - Song.wav",
            flux_library::Availability::Present,
            "Song",
        );
        record.artist_id = Some(artist);
        store.import_tracks(vec![record], 2).expect("import");
        store.commit().expect("commit");
        let catalog_before = fs::read(source.join(".flux/catalog.flux")).expect("catalog");
        let destination = dir.path().join("Canonical");
        let plan = super::plan_canonical_migration(&source, &destination).expect("plan");
        assert!(plan.is_ready(), "issues: {:?}", plan.issues());
        let report = super::execute_canonical_migration(&plan).expect("execute");
        assert_eq!(report.tracks(), 1);
        assert_eq!(
            fs::read(source.join(".flux/catalog.flux")).expect("catalog"),
            catalog_before
        );
        assert!(source_audio.is_file());
        let (controller, _control) = boot_canonical_at(&destination);
        assert_eq!(controller.tracks().len(), 1);
        controller.shutdown();
    }

    #[test]
    fn controller_ownership_transfer_preserves_queue_playlist_rating_and_credits() {
        let dir = TempDir::new("canonical-transfer");
        let root = dir.path().join("FluxLibrary");
        create_canonical_library(&root).expect("create");
        let (mut controller, _control) = boot_canonical_at(&root);
        let a = controller.create_artist("A").expect("A");
        let b = controller.create_artist("B").expect("B");
        let source = dir.path().join("source/song.wav");
        fs::create_dir_all(source.parent().expect("parent")).expect("dirs");
        write_dc_wav(source.parent().expect("parent"), "song.wav", 4_410, 0.2);
        let id = {
            let store = controller.canonical_store.as_mut().expect("store");
            store
                .import_track(flux_library::canonical::CanonicalImportedTrack {
                    source,
                    title: "Shared".to_owned(),
                    artist_id: Some(a),
                    artist_name: "A".to_owned(),
                    credited_artist_ids: vec![a, b],
                    album_title: Some("Album".to_owned()),
                    release_type: flux_library::canonical::ReleaseType::Album,
                    track_number: Some(1),
                    disc_number: None,
                    duration_ms: None,
                })
                .expect("import")
        };
        controller.refresh_from_canonical();
        controller.set_rating(id, Some(3), true).expect("rating");
        let playlist = controller
            .set_playlist(None, "Keep", vec![id])
            .expect("playlist");
        controller.play_track(id);
        let entry = controller.current_queue_entry_id().expect("queue entry");
        let destination = {
            let store = controller.canonical_store.as_mut().expect("store");
            store
                .create_album(b, "Destination", flux_library::canonical::ReleaseType::Ep)
                .expect("album")
        };
        let plan = controller
            .plan_track_ownership_transfer(
                id,
                b,
                super::OwnershipDestination::Existing(destination),
            )
            .expect("plan");
        let report = controller
            .transfer_track_ownership(&plan)
            .expect("transfer");
        assert_eq!(report.track_id, id);
        assert_eq!(controller.track_artist_id(id), Some(b));
        assert_eq!(controller.track_credits(id), vec![a, b]);
        assert_eq!(controller.track_album_id(id), Some(destination));
        assert_eq!(controller.album_tracks(destination), vec![id]);
        assert_eq!(controller.current_queue_entry_id(), Some(entry));
        assert!(controller.queue_entry_index(entry).is_some());
        assert_eq!(
            controller.rating(id),
            Some(flux_library::canonical::TrackRating {
                rating: Some(3),
                favorite: true
            })
        );
        assert_eq!(
            controller
                .playlists()
                .into_iter()
                .find(|item| item.id == playlist)
                .map(|item| item.track_ids),
            Some(vec![id])
        );
        let audio = controller.track(id).expect("track").source().path();
        assert!(audio.starts_with(controller.canonical_root().expect("root")));
        assert!(audio.is_file());
        controller.shutdown();
    }

    #[test]
    fn controller_artist_deletion_keeps_queue_and_playlist_references_consistent() {
        let dir = TempDir::new("canonical-artist-delete");
        let root = dir.path().join("FluxLibrary");
        create_canonical_library(&root).expect("create");
        let (mut controller, _control) = boot_canonical_at(&root);
        let a = controller.create_artist("A").expect("A");
        let b = controller.create_artist("B").expect("B");
        let collab_source = dir.path().join("source/collab.wav");
        let solo_source = dir.path().join("source/solo.wav");
        let guest_source = dir.path().join("source/guest.wav");
        for (path, level) in [
            (&collab_source, 0.2),
            (&solo_source, 0.3),
            (&guest_source, 0.4),
        ] {
            fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            write_dc_wav(
                path.parent().expect("parent"),
                path.file_name().expect("name").to_str().expect("utf8"),
                4_410,
                level,
            );
        }
        let collab = controller
            .import_canonical_track(super::CanonicalImportedTrack {
                source: collab_source,
                title: "Collab".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("A Album".to_owned()),
                release_type: super::ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("collab");
        let solo = controller
            .import_canonical_track(super::CanonicalImportedTrack {
                source: solo_source,
                title: "Solo".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a],
                album_title: Some("A Album".to_owned()),
                release_type: super::ReleaseType::Album,
                track_number: Some(2),
                disc_number: None,
                duration_ms: None,
            })
            .expect("solo");
        let guest = controller
            .import_canonical_track(super::CanonicalImportedTrack {
                source: guest_source,
                title: "Guest".to_owned(),
                artist_id: Some(b),
                artist_name: "B".to_owned(),
                credited_artist_ids: vec![b, a],
                album_title: Some("B Album".to_owned()),
                release_type: super::ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("guest");
        let destination = controller.track_album_id(guest).expect("B album");
        let playlist = controller
            .set_playlist(None, "Keep", vec![collab, solo, guest])
            .expect("playlist");
        controller
            .set_rating(collab, Some(5), true)
            .expect("rating");
        controller.enqueue_track(collab).expect("enqueue collab");
        controller.play_track(solo);
        assert_eq!(controller.queue_len(), 2, "queue entries before deletion");
        assert_eq!(controller.current_id(), Some(solo));
        let mut plan = controller.plan_artist_deletion(a).expect("plan");
        plan.set_transfer(
            collab,
            b,
            super::ArtistDeletionDestination::Existing(destination),
        )
        .expect("transfer");
        plan.set_delete(solo, true).expect("delete");
        plan.set_remove_credit(guest, None).expect("credit");
        assert!(plan.is_ready(), "{:?}", plan.issues);
        let report = controller.execute_artist_deletion(&plan).expect("execute");
        assert_eq!(report.deleted_tracks, vec![solo]);
        assert!(controller.artist(a).is_none());
        assert_eq!(controller.track_artist_id(collab), Some(b));
        assert_eq!(controller.track_credits(collab), vec![b]);
        assert_eq!(controller.track_album_id(collab), Some(destination));
        assert_eq!(controller.track_credits(guest), vec![b]);
        assert!(controller.track(solo).is_none());
        assert_eq!(
            controller.current_id(),
            Some(collab),
            "queue advances to surviving entry"
        );
        assert_eq!(controller.player_state(), flux_core::PlaybackState::Stopped);
        assert!(
            controller
                .queue_entries()
                .iter()
                .all(|entry| entry.track() != solo)
        );
        assert_eq!(
            controller
                .playlists()
                .into_iter()
                .find(|item| item.id == playlist)
                .map(|item| item.track_ids),
            Some(vec![collab, guest])
        );
        assert_eq!(
            controller.rating(collab),
            Some(flux_library::canonical::TrackRating {
                rating: Some(5),
                favorite: true
            })
        );
        assert_eq!(controller.rating(solo), None);
        controller.shutdown();
    }
}
