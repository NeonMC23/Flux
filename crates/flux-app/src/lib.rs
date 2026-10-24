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
//! Persistence goes through one explicit backend at a time
//! ([`StorageMode`]). The backend is chosen at construction and can
//! afterwards be replaced only through the explicit switch methods,
//! which build and validate the candidate library first and swap
//! atomically — a failed switch changes nothing, and queue/selection
//! state never carries track IDs across libraries (playback stops
//! and the queue is rebuilt from the new library):
//!
//! - **Legacy** ([`Controller::new`] / [`Controller::open_default`],
//!   later [`Controller::switch_to_legacy_index`]): the version-1
//!   JSON index at the given path (today's default; its startup
//!   behavior is unchanged — a missing index starts empty, an
//!   unreadable one starts empty with a reported error).
//! - **Portable** ([`Controller::new_portable`], later
//!   [`Controller::switch_to_portable`] /
//!   [`Controller::switch_to_new_portable_library`]): the `.flux`
//!   store at a caller-supplied library root. Opening is fallible and
//!   strict: a missing or invalid store is a [`PortableOpenError`],
//!   never a silent fallback to an empty library or the legacy index,
//!   and nothing is created. The in-memory [`Library`] is
//!   reconstructed from the store (IDs, counter, and missing records
//!   included); saves sync it back and commit through the store's own
//!   protocol. A store recovered from its backup catalogue stays
//!   read-only — scans and saves refuse — until
//!   [`acknowledge_portable_recovery`](Controller::acknowledge_portable_recovery)
//!   is called explicitly. Because a portable library can only store
//!   paths inside its own root, portable-mode scans accept only
//!   folders within that root.
//!
//! Creating a brand-new portable library ([`create_portable_library`])
//! and migrating a legacy index ([`plan_legacy_migration`] →
//! [`execute_legacy_migration`], or the one-call
//! [`migrate_legacy_library`]) are likewise explicit, caller-requested
//! operations: never side effects of startup, scanning, or saving,
//! and neither switches a running controller by itself — activating
//! the result is the separate switch step above.
//!
//! # Threading
//!
//! All `Controller` methods are synchronous and, except where noted,
//! non-blocking — safe to call from a UI thread. Directory scans run on
//! a controller-owned worker thread: [`begin_scan`](Controller::begin_scan)
//! starts one and [`poll`](Controller::poll) reports completion. The
//! engine has its own threads (see `flux-audio`); its events are
//! drained in `poll` without waiting. The only blocking operations are
//! index load in [`Controller::new`] (call it before starting the UI),
//! [`save_index`](Controller::save_index), and
//! [`shutdown`](Controller::shutdown) — lifecycle points, not gestures.
//!
//! # Playable vs. indexed
//!
//! The library scanner indexes 16 file extensions, but the engine
//! decodes a subset of them (see `flux_audio::decode` for the exact
//! container/codec matrix: WAV, FLAC, MP3, AAC/ADTS, Ogg Vorbis,
//! Opus, and AAC in M4A/MP4). The controller checks
//! `flux_audio::decode::is_playable` before asking the engine to load
//! anything and surfaces a clear error for the rest; indexed tracks are
//! never presented as playable by implication (see
//! [`is_track_playable`], which front-ends use for display). Even for
//! a playable extension the engine still validates the contents, and
//! corrupt files fail with a typed error — never a silent skip.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use std::fmt;

use flux_audio::Player;
use flux_audio::decode::StreamInfo;
use flux_audio::engine::{EngineEvent, EngineHandle};
use flux_core::{PlaybackState, RepeatMode, Track, TrackId};
use flux_library::portable::{
    CATALOG_BACKUP_FILE_NAME, CATALOG_FILE_NAME, FLUX_DIR_NAME, OpenedStore, PortableStore,
    sync_library_to_portable_store,
};
use flux_library::{Library, PortableSyncReport, ScanReport};

pub use flux_library::migrate::{MigrationError, MigrationPlan, MigrationReport, MigrationStage};
pub use flux_library::{PortableStoreError, RecoveryInfo, StoreError};

/// Whether real device audio is available to this controller's engine.
///
/// A controller is fully usable for browsing/scanning in the
/// `Unavailable` state; only playback commands fail, with the reason
/// surfaced as the error message. This exists so a build without an
/// audio backend (or a machine without an output device) says so
/// plainly instead of pretending to play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStatus {
    /// The engine drives a working output (a real device, or a
    /// deliberately injected headless backend in tests).
    Ready,
    /// Playback is not possible; the string says why, in user terms.
    Unavailable(String),
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
            Self::Unavailable(reason) => Some(reason),
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
    /// The portable `.flux` store at an explicit library root.
    Portable,
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
        }
    }
}

impl std::error::Error for PortableOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::RootInvalid { .. } => None,
        }
    }
}

impl From<PortableStoreError> for PortableOpenError {
    fn from(err: PortableStoreError) -> Self {
        Self::Store(err)
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
            _ => None,
        }
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
        }
    }
}

impl std::error::Error for SwitchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ScanInProgress => None,
            Self::Portable(err) => Some(err),
            Self::Legacy(err) => Some(err),
        }
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

/// Notifications produced by [`Controller::poll`]. Front-ends treat
/// these as "re-read controller state now" signals; `Error`/`Status`
/// also carry the message (mirrored in
/// [`last_error`](Controller::last_error) /
/// [`last_status`](Controller::last_status)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerEvent {
    /// The track collection changed (scan finished, index loaded…).
    LibraryChanged,
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
/// With the `cpal` feature, this opens the default output device; on
/// any failure (or in a build without the feature) it returns a
/// headless null engine plus `Unavailable(reason)`, so the application
/// can still run and browse while telling the user exactly why
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
                    AudioStatus::Unavailable(format!("audio output unavailable: {err}")),
                );
            }
        }
    }
    #[allow(unreachable_code)]
    {
        let (engine, _control) = EngineHandle::spawn_null(48_000, 2);
        (
            engine,
            AudioStatus::Unavailable(
                "this build has no audio output backend (compiled without the `cpal` feature)"
                    .to_owned(),
            ),
        )
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
    /// Stream facts for `engine_loaded`, once the engine confirms it.
    stream_info: Option<StreamInfo>,
    volume: f32,
    /// The persistence backend, fixed at construction.
    storage: StorageBackend,
    /// The portable store in portable mode — `None` in legacy mode,
    /// while a scan thread owns it, or if it was lost with a scan
    /// thread and could not be reopened (saves then fail visibly).
    portable_store: Option<PortableStore>,
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

impl Controller {
    /// Creates a controller around an already-spawned engine.
    ///
    /// If `index_path` names an existing file, the library index is
    /// loaded from it (this reads and parses a file, so call this
    /// before starting the UI event loop). A missing file simply
    /// starts an empty library; an unreadable/corrupt one also starts
    /// empty but records a user-facing error in
    /// [`last_error`](Self::last_error). The play queue is built from
    /// the library in library order.
    #[must_use]
    pub fn new(engine: EngineHandle, audio: AudioStatus, index_path: Option<PathBuf>) -> Self {
        let (library, last_error) = match &index_path {
            Some(path) if path.exists() => match Library::load_from_path(path) {
                Ok(library) => (library, None),
                Err(err) => (
                    Library::new(),
                    Some(format!(
                        "Could not load the saved library index ({}): {err}. \
                         Starting with an empty library.",
                        path.display()
                    )),
                ),
            },
            _ => (Library::new(), None),
        };
        let mut controller = Self::assemble(
            engine,
            audio,
            library,
            StorageBackend::Legacy { index_path },
            None,
            None,
            Vec::new(),
        );
        if let Some(error) = last_error {
            controller.report_error(error);
        }
        controller
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
        let (root, opened, library, skipped) = open_portable_candidate(library_root)?;
        let recovery = opened.recovery;

        let mut controller = Self::assemble(
            engine,
            audio,
            library,
            StorageBackend::Portable { root: root.clone() },
            Some(opened.store),
            recovery,
            vec![root],
        );
        if skipped > 0 {
            controller.report_status(skipped_unrepresentable_message(skipped));
        }
        if controller.portable_recovery.is_some() {
            controller.report_error(PORTABLE_RECOVERY_READ_ONLY.to_owned());
        }
        Ok(controller)
    }

    /// Shared construction once the initial library and backend are
    /// known: snapshot, playback model, queue, and engine volume are
    /// set up identically for both backends.
    fn assemble(
        engine: EngineHandle,
        audio: AudioStatus,
        library: Library,
        storage: StorageBackend,
        portable_store: Option<PortableStore>,
        portable_recovery: Option<RecoveryInfo>,
        roots: Vec<PathBuf>,
    ) -> Self {
        let mut controller = Self {
            tracks: snapshot(&library),
            library: Some(library),
            player: Player::new(),
            engine,
            audio,
            engine_loaded: None,
            stream_info: None,
            volume: 1.0,
            storage,
            portable_store,
            portable_recovery,
            roots,
            scan_rx: None,
            outbox: Vec::new(),
            last_error: None,
            last_status: None,
        };
        controller.rebuild_queue();
        controller.engine.set_volume(controller.volume);
        controller
    }

    /// The standard application setup: [`default_engine`] +
    /// [`default_index_path`] (legacy backend, as before — portable
    /// mode is always an explicit choice via
    /// [`new_portable`](Self::new_portable)).
    #[must_use]
    pub fn open_default() -> Self {
        let (engine, audio) = default_engine();
        Self::new(engine, audio, default_index_path())
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

    /// The playback model's state.
    #[must_use]
    pub fn player_state(&self) -> PlaybackState {
        self.player.state()
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
        }
    }

    /// The legacy index path in legacy mode (`None` in portable mode,
    /// or when running without persistence).
    #[must_use]
    pub fn legacy_index_path(&self) -> Option<&Path> {
        match &self.storage {
            StorageBackend::Legacy { index_path } => index_path.as_deref(),
            StorageBackend::Portable { .. } => None,
        }
    }

    /// The library root in portable mode (canonical), `None` in
    /// legacy mode.
    #[must_use]
    pub fn portable_root(&self) -> Option<&Path> {
        match &self.storage {
            StorageBackend::Portable { root } => Some(root),
            StorageBackend::Legacy { .. } => None,
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
        self.report_status(
            "Portable library recovery acknowledged; scanning and saving are enabled.".to_owned(),
        );
        Ok(())
    }

    // --------------------------------------------------- switching ---

    /// Switches the active library to the existing portable library
    /// at `library_root`. The candidate is fully opened and
    /// validated first; only then is it installed — so on any
    /// [`SwitchError`] the current library, backend, queue, and
    /// playback state are completely unchanged, and the audio engine
    /// is never restarted (the same engine serves both libraries).
    ///
    /// On success playback stops and the queue is rebuilt from the
    /// new library: track IDs are unique only within one library, so
    /// the old selection is discarded rather than ever being
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
        let (root, opened, library, skipped) = match open_portable_candidate(library_root) {
            Ok(candidate) => candidate,
            Err(err) => {
                let err = SwitchError::from(err);
                self.report_error(err.to_string());
                return Err(err);
            }
        };
        let recovery = opened.recovery;
        self.install_library(
            library,
            StorageBackend::Portable { root: root.clone() },
            Some(opened.store),
            recovery,
        );
        self.roots = vec![root.clone()];
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
    /// Playback stops and the queue is rebuilt from the new library,
    /// as with every switch.
    pub fn switch_to_legacy_index(
        &mut self,
        index_path: Option<PathBuf>,
    ) -> Result<(), SwitchError> {
        if self.is_scanning() {
            self.report_error(SwitchError::ScanInProgress.to_string());
            return Err(SwitchError::ScanInProgress);
        }
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
        let shown = index_path
            .as_ref()
            .map(|path| format!(" at {}", path.display()))
            .unwrap_or_default();
        self.install_library(library, StorageBackend::Legacy { index_path }, None, None);
        self.roots = Vec::new();
        self.report_status(format!("Switched to the legacy library index{shown}."));
        Ok(())
    }

    /// The single commit point of a library switch: stops playback,
    /// forgets the engine's loaded track, discards the outgoing
    /// library/store, and installs the already-validated candidate.
    /// The queue is rebuilt from the new library with the *new*
    /// library's first track selected (or none) — the outgoing
    /// selection is never carried over, because an ID from one
    /// library must never be read as an ID from another. Volume and
    /// repeat mode are user preferences, not library state, and are
    /// kept.
    fn install_library(
        &mut self,
        library: Library,
        storage: StorageBackend,
        portable_store: Option<PortableStore>,
        portable_recovery: Option<RecoveryInfo>,
    ) {
        self.player.stop();
        self.engine.stop();
        self.engine_loaded = None;
        self.stream_info = None;

        self.tracks = snapshot(&library);
        self.library = Some(library);
        self.storage = storage;
        self.portable_store = portable_store;
        self.portable_recovery = portable_recovery;

        let queue = self.player.queue_mut();
        queue.clear();
        for (id, _) in &self.tracks {
            queue.push(*id);
        }
        self.outbox.push(ControllerEvent::LibraryChanged);
        self.outbox.push(ControllerEvent::PlaybackChanged);
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

    /// Starts playing the given track (which must be in the library).
    /// Restarting the already-playing track restarts it. Failures
    /// (unknown track, unsupported format, unavailable audio) set
    /// [`last_error`](Self::last_error) and leave playback stopped.
    pub fn play_track(&mut self, id: TrackId) {
        match self.player.play_track(id) {
            Ok(()) => self.sync_engine(true),
            Err(err) => self.report_error(format!("Cannot play that track: {err}")),
        }
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
        self.player.next_track();
        self.sync_engine(true);
    }

    /// Skips to the previous track; on the first track, restarts it.
    pub fn previous(&mut self) {
        self.player.previous_track();
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
        let Some(library) = self.library.as_ref() else {
            return Err(match self.storage {
                StorageBackend::Legacy { .. } => {
                    "A scan is in progress; the index will be saved when it finishes."
                }
                StorageBackend::Portable { .. } => {
                    "A scan is in progress; the portable library will be saved when it finishes."
                }
            }
            .into());
        };
        match &self.storage {
            StorageBackend::Legacy { index_path } => match index_path {
                Some(path) => save_library(library, path),
                None => Ok(()),
            },
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
        }
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
            match event {
                EngineEvent::Loaded { id, info } => {
                    if self.engine_loaded == Some(id) {
                        self.stream_info = Some(info);
                        events.push(ControllerEvent::PlaybackChanged);
                    }
                }
                EngineEvent::Started { .. } | EngineEvent::Paused { .. } | EngineEvent::Stopped => {
                    events.push(ControllerEvent::PlaybackChanged)
                }
                EngineEvent::TrackEnded { id } => {
                    // Accept only the end of the track we loaded and
                    // the model still considers current — stale events
                    // can never advance the wrong queue position (the
                    // engine's generations already prevent them; this
                    // is the controller-side belt to that suspenders).
                    if self.engine_loaded == Some(id) && self.player.current_track() == Some(id) {
                        if self.player.on_track_ended().is_some() {
                            self.sync_engine(true);
                        }
                        events.push(ControllerEvent::PlaybackChanged);
                    }
                }
                EngineEvent::Error { id, error } => {
                    let message = match id.and_then(|i| self.track(i)) {
                        Some(track) => {
                            format!("Playback failed for \"{}\": {error}", track.title())
                        }
                        None => format!("Audio error: {error}"),
                    };
                    self.player.stop();
                    self.engine_loaded = None;
                    self.stream_info = None;
                    self.report_error(message);
                    events.push(ControllerEvent::PlaybackChanged);
                }
            }
        }

        events.append(&mut self.outbox);
        events
    }

    // -------------------------------------------------------- internals ---

    /// Rebuilds the player's queue as "the whole library, in library
    /// order", preserving the current selection when it still exists.
    fn rebuild_queue(&mut self) {
        let previous = self.player.current_track();
        let ids: Vec<TrackId> = self.tracks.iter().map(|(id, _)| *id).collect();
        let queue = self.player.queue_mut();
        queue.clear();
        for id in ids {
            queue.push(id);
        }
        if let Some(id) = previous {
            let _ = queue.set_current_by_id(id);
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
                self.stream_info = None;
            }
        }
    }

    /// Resolves `id` to a file and asks the engine to load it.
    /// Synchronous failures (no audio, unknown track, unsupported
    /// format) are returned for the caller to surface; decode/IO
    /// failures arrive asynchronously as engine error events.
    fn load_into_engine(&mut self, id: TrackId, autoplay: bool) -> Result<(), String> {
        if let AudioStatus::Unavailable(reason) = &self.audio {
            return Err(reason.clone());
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
        self.engine.load(id, path);
        if autoplay {
            self.engine.play();
        }
        self.engine_loaded = Some(id);
        self.stream_info = None;
        Ok(())
    }

    /// Common failure path: stop the model and the engine, forget the
    /// load, and tell the user. State is consistent afterwards.
    fn fail_playback(&mut self, message: String) {
        self.player.stop();
        self.engine.stop();
        self.engine_loaded = None;
        self.stream_info = None;
        self.report_error(message);
    }

    fn begin_scan_roots(&mut self, roots: Vec<PathBuf>) {
        if self.scan_rx.is_some() {
            self.report_error("A library scan is already in progress.".to_owned());
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
        };
        let work = ScanWork {
            library,
            roots,
            persistence,
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
                }) = work_rx.recv()
                {
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
        self.tracks = snapshot(&outcome.library);
        self.library = Some(outcome.library);
        if let ScanPersistence::Portable(store) = outcome.persistence {
            self.portable_store = Some(*store);
        }
        self.rebuild_queue();
        let report = &outcome.report;
        let mut message = format!(
            "Scan complete: {} added, {} updated, {} already indexed, {} skipped, {} failed. {} track(s) in the library.",
            report.added(),
            report.updated(),
            report.deduplicated(),
            report.skipped().len(),
            report.failed().len(),
            self.tracks.len(),
        );
        if let Some(sync_report) = &outcome.sync_report
            && !sync_report.skipped.is_empty()
        {
            message += &format!(
                " {} track(s) could not be stored in the portable library and \
                 are kept for this session only.",
                sync_report.skipped.len()
            );
        }
        self.report_status(message);
        if let Some(Err(message)) = outcome.saved {
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
        };
        match restored {
            Some(library) => {
                self.tracks = snapshot(&library);
                self.library = Some(library);
                self.rebuild_queue();
                true
            }
            None => {
                self.library = Some(Library::new());
                self.tracks.clear();
                self.rebuild_queue();
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
fn open_portable_candidate(
    library_root: &Path,
) -> Result<(PathBuf, OpenedStore, Library, usize), PortableOpenError> {
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
    let root = std::fs::canonicalize(library_root)
        .map_err(|err| root_invalid(format!("it cannot be canonicalized: {err}")))?;

    let opened = PortableStore::open(&root)?;
    let (library, skipped) = Library::from_portable_store(&opened.store);
    Ok((root, opened, library, skipped))
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

    use flux_audio::engine::EngineHandle;
    use flux_audio::output::NullControl;
    use flux_core::{PlaybackState, RepeatMode, TrackId};

    use super::{
        AudioStatus, Controller, ControllerEvent, CreateLibraryError, PortableOpenError,
        ScanOutcome, StorageMode, SwitchError, create_portable_library, execute_legacy_migration,
        is_track_playable, migrate_legacy_library, plan_legacy_migration,
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
            Controller::new(engine, AudioStatus::Ready, Some(index)),
            control,
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
    fn unavailable_audio_refuses_playback_but_still_browses() {
        let dir = TempDir::new("noaudio");
        let fx = fixtures(&dir);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let mut controller = Controller::new(
            engine,
            AudioStatus::Unavailable("no output device in this test".to_owned()),
            None,
        );
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
        let mut controller = Controller::new(engine, AudioStatus::Ready, None);
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
        (Controller::new(engine, AudioStatus::Ready, index), control)
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
        // And the new library is a real, reopenable store.
        let (again, _c2) = boot_portable_at(&new_root).expect("created library reopens");
        again.shutdown();
        controller.shutdown();
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
}
