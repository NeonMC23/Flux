//! Explicit migration from the legacy app-data index ([`crate::store`])
//! to a portable `.flux` library ([`crate::portable`]).
//!
//! Migration is **plan-first**: [`plan_migration`] reads and validates
//! the legacy index (losslessly — see
//! [`crate::store::load_legacy_index_data`]), classifies every record
//! against an explicitly supplied destination library root, and
//! returns a deterministic [`MigrationPlan`] without writing anything.
//! [`execute_migration`] then creates the portable store through the
//! storage layer's own APIs ([`PortableStore::create`],
//! [`PortableStore::import_tracks`], [`PortableStore::commit`]),
//! reopens it, and verifies the result against the plan before
//! reporting success. [`migrate_legacy_index`] chains the two.
//!
//! # Per-record policy
//!
//! The portable schema can only represent library-relative paths, so
//! every legacy record (whose path is a native, normally absolute,
//! path) is classified against the destination root:
//!
//! - **Inside the root, file present** — migrated with the same
//!   `TrackId` and metadata, availability [`Availability::Present`],
//!   and a size/mtime fingerprint taken from the same `stat` that
//!   established presence.
//! - **Inside the root, file absent** — retained with the same
//!   `TrackId` and metadata, availability [`Availability::Missing`],
//!   no fingerprint. "Inside" is decided by canonicalizing the
//!   record's parent directory when it exists (so a symlinked
//!   directory that points outside the root does *not* smuggle an
//!   external file in), or — when the parent is gone too — by lexical
//!   prefix against the canonical/absolute root. Lexical fallback
//!   never accepts `..` components.
//! - **Outside the root** ("external") — excluded and reported.
//!   Migration never copies, moves, or even `stat`s files outside the
//!   root.
//! - **Symlinked file** — a record whose inside-root path is itself a
//!   symlink is excluded and reported: managed content must be the
//!   real file under the root, so the final component is always
//!   checked with `symlink_metadata` and never followed.
//! - **Unrepresentable** — the path cannot be interpreted
//!   byte-exactly on this platform (Unix-byte paths off Unix) or
//!   cannot become a valid [`RelPath`]. The original representation is
//!   preserved verbatim in the plan/report; nothing is lossy-decoded.
//! - **Ambiguous** — migration refuses to guess: relative legacy
//!   paths (their base directory is unknowable), `..` with a missing
//!   parent, the root itself, entries that exist but are not regular
//!   files, records that map onto a library path another record
//!   already claimed (first in source order wins), and entries whose
//!   presence could not be verified (I/O error other than not-found).
//!
//! Source-wide problems (malformed JSON, unsupported version,
//! duplicate IDs or paths, impossible counters) fail planning before
//! any destination is created — migration never falls back to an
//! empty library and never skips malformed records silently.
//!
//! # Field mapping
//!
//! The legacy schema predates overrides, fingerprints, availability,
//! album-artist, and timestamps, so none of those can be *preserved*;
//! they are initialized instead: overrides empty, album artist
//! unknown, timestamps set to the migration time, availability and
//! fingerprint derived as above. Legacy title/artist/album/track
//! number/disc number/duration become the portable [`TagSet`] (the
//! legacy title is required text — originally a tag or scan filename
//! fallback that the legacy format does not distinguish — so it
//! becomes `TagSet::title`). The legacy next-ID counter is preserved
//! exactly, so IDs of excluded or previously removed tracks stay
//! retired in the new library; [`MigrationPlan::id_gaps`] lists them.
//! Portable records live in ascending `TrackId` order, so a legacy
//! index whose records are not stored in ID order (not produced by
//! the legacy writer, but accepted by its loader) changes order; the
//! plan records a warning in that case.
//!
//! # Destination safety
//!
//! The destination root must already exist as a directory. If it
//! already holds a valid portable library, migration refuses with
//! [`MigrationError::DestinationAlreadyInitialized`]; if `.flux/`
//! exists but is partial (segment/temp files without a catalogue) or
//! invalid (no catalogue validates), it refuses with
//! [`MigrationError::DestinationPartialOrInvalid`] and preserves every
//! file for inspection/recovery — an interrupted earlier migration is
//! therefore distinguishable from an empty destination, and retrying
//! can never duplicate tracks or overwrite data. (A pre-existing but
//! completely empty `.flux/` directory holds no data and does not
//! block migration.) Failures after creation leave the partial store
//! in place and say so via [`MigrationError::destination_created`];
//! the legacy index is only ever read, and audio files are never
//! moved, copied, deleted, or rewritten.
//!
//! # Limitations
//!
//! - One writer is assumed, as everywhere in the storage layer: plan
//!   checks (file presence, canonical parents) and execution can race
//!   concurrent filesystem changes. Migration is not a security
//!   sandbox; containment reflects the filesystem at plan time.
//! - Catalogue publication during the migration commit inherits the
//!   portable store's platform behavior: atomic rename on Unix;
//!   remove-then-rename (not atomic) on Windows, with the backup
//!   catalogue rotated first so a crash in that window recovers to
//!   the pre-commit state.
//! - Nothing here is wired into application startup: choosing when to
//!   migrate (and which root to offer) is library-lifecycle work in a
//!   later milestone. The legacy index remains the startup path.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};

use flux_core::TrackId;

use crate::portable::{
    Availability, CATALOG_BACKUP_FILE_NAME, CATALOG_FILE_NAME, FLUX_DIR_NAME, FileFingerprint,
    ImportedTrack, MetadataOverrides, PathCasePolicy, PathKey, PortableStore, PortableStoreError,
    RelPath, TagSet, now_unix_ms, path_key,
};
use crate::store::{LegacyPathData, StoreError, load_legacy_index_data};

// -------------------------------------------------------------- types ---

/// A legacy track path in its original stored representation, as
/// carried in plans and reports. Never lossy: non-UTF-8 Unix names
/// stay raw bytes on every platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyPath {
    /// UTF-8 path text, exactly as stored in the legacy index.
    Utf8(String),
    /// Raw Unix path bytes, exactly as stored in the legacy index.
    UnixBytes(Vec<u8>),
}

impl LegacyPath {
    fn from_data(data: LegacyPathData) -> Self {
        match data {
            LegacyPathData::Utf8(text) => Self::Utf8(text),
            LegacyPathData::UnixBytes(bytes) => Self::UnixBytes(bytes),
        }
    }

    /// A human-facing rendering for messages only. For raw-byte paths
    /// this is lossy by design; never use it as a filesystem path.
    #[must_use]
    pub fn display_lossy(&self) -> String {
        match self {
            Self::Utf8(text) => text.clone(),
            Self::UnixBytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        }
    }
}

/// Why a legacy record will not (or did not) enter the portable store.
/// Excluded records are never copied in, never deleted anywhere, and
/// always appear in the plan and report individually.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusionReason {
    /// The file lives outside the selected library root. Migration
    /// does not copy external files into the library.
    External,
    /// The inside-root path is a symbolic link. Managed content must
    /// be the real file under the root; links are never followed.
    SymlinkedFile,
    /// The path cannot be interpreted or stored byte-exactly here
    /// (for example a Unix-byte path on a non-Unix platform, or a name
    /// the portable relative-path form rejects). The record's
    /// original stored representation is preserved in the plan; the
    /// message says what is wrong.
    Unrepresentable(String),
    /// The mapping cannot be decided without guessing (relative
    /// legacy path, unresolved traversal, not a regular file, two
    /// records claiming one library path, unverifiable presence, …).
    /// The message says precisely what is ambiguous.
    Ambiguous(String),
}

impl fmt::Display for ExclusionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::External => write!(f, "file is outside the library root"),
            Self::SymlinkedFile => write!(
                f,
                "path inside the library root is a symbolic link, not the file itself"
            ),
            Self::Unrepresentable(detail) => write!(f, "unrepresentable path: {detail}"),
            Self::Ambiguous(detail) => write!(f, "ambiguous mapping: {detail}"),
        }
    }
}

/// What migration will do (plan) or did (report) with one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationDecision {
    /// The record enters the portable store with its original
    /// `TrackId` and metadata.
    Migrate {
        /// The validated library-relative path.
        relative_path: RelPath,
        /// [`Availability::Present`] when the file was verified inside
        /// the root at plan time, [`Availability::Missing`] when the
        /// mapped file (or its parent) is absent.
        availability: Availability,
        /// Size/mtime fingerprint for present files.
        fingerprint: Option<FileFingerprint>,
        /// The legacy metadata, mapped onto the portable tag set.
        tags: TagSet,
    },
    /// The record does not enter the portable store.
    Excluded(ExclusionReason),
}

/// One legacy record's place in a [`MigrationPlan`]/report: its
/// original identity and path, plus the decision made for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRecord {
    source_id: TrackId,
    original_path: LegacyPath,
    title: String,
    decision: MigrationDecision,
}

impl PlannedRecord {
    /// The record's `TrackId` in the legacy index (and, when
    /// migrated, in the portable store — IDs are preserved).
    #[must_use]
    pub fn source_id(&self) -> TrackId {
        self.source_id
    }

    /// The path exactly as stored in the legacy index.
    #[must_use]
    pub fn original_path(&self) -> &LegacyPath {
        &self.original_path
    }

    /// The legacy title, for presenting the outcome to the user.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The decision made for this record.
    #[must_use]
    pub fn decision(&self) -> &MigrationDecision {
        &self.decision
    }
}

/// A maximal inclusive range of `TrackId` values below the preserved
/// next-ID counter that will have no live record in the portable
/// store: IDs the legacy library retired earlier, plus IDs of
/// records this migration excluded. They stay retired — the counter
/// is preserved exactly, so they are never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdGap {
    /// First retired ID in the range.
    pub first: u64,
    /// Last retired ID in the range (inclusive).
    pub last: u64,
}

/// A complete, deterministic migration plan: the output of
/// [`plan_migration`], the input of [`execute_migration`]. Building a
/// plan only reads (the legacy index, directory metadata, and `stat`s
/// inside the destination root); it writes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    source_index: PathBuf,
    destination_root: PathBuf,
    case_policy: PathCasePolicy,
    source_next_id: u64,
    records: Vec<PlannedRecord>,
    id_gaps: Vec<IdGap>,
    warnings: Vec<String>,
}

impl MigrationPlan {
    /// The legacy index this plan migrates (read-only input).
    #[must_use]
    pub fn source_index(&self) -> &Path {
        &self.source_index
    }

    /// The explicitly supplied destination library root.
    #[must_use]
    pub fn destination_root(&self) -> &Path {
        &self.destination_root
    }

    /// The path case policy the destination will be created/opened
    /// with, and under which destination duplicates were judged.
    #[must_use]
    pub fn case_policy(&self) -> PathCasePolicy {
        self.case_policy
    }

    /// The legacy next-ID counter, preserved exactly in the new
    /// store.
    #[must_use]
    pub fn source_next_id(&self) -> u64 {
        self.source_next_id
    }

    /// Every legacy record, in legacy source order, with its
    /// decision.
    #[must_use]
    pub fn records(&self) -> &[PlannedRecord] {
        &self.records
    }

    /// Retired-ID ranges below the preserved counter (see [`IdGap`]).
    #[must_use]
    pub fn id_gaps(&self) -> &[IdGap] {
        &self.id_gaps
    }

    /// Non-fatal observations (for example a legacy record order that
    /// is not `TrackId` order and therefore changes).
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Records that will enter the portable store with a verified
    /// file inside the root.
    #[must_use]
    pub fn present_count(&self) -> usize {
        self.count_decisions(|decision| {
            matches!(
                decision,
                MigrationDecision::Migrate {
                    availability: Availability::Present,
                    ..
                }
            )
        })
    }

    /// Records that will enter the portable store marked
    /// [`Availability::Missing`] (mapped under the root, file absent).
    #[must_use]
    pub fn missing_count(&self) -> usize {
        self.count_decisions(|decision| {
            matches!(
                decision,
                MigrationDecision::Migrate {
                    availability: Availability::Missing,
                    ..
                }
            )
        })
    }

    /// Records that will enter the portable store at all
    /// (present + missing).
    #[must_use]
    pub fn migrated_count(&self) -> usize {
        self.present_count() + self.missing_count()
    }

    /// Records excluded because their file is outside the root.
    #[must_use]
    pub fn external_count(&self) -> usize {
        self.count_excluded(|reason| matches!(reason, ExclusionReason::External))
    }

    /// Records excluded because the inside-root path is a symlink.
    #[must_use]
    pub fn symlinked_count(&self) -> usize {
        self.count_excluded(|reason| matches!(reason, ExclusionReason::SymlinkedFile))
    }

    /// Records excluded as unrepresentable on this platform/format.
    #[must_use]
    pub fn unrepresentable_count(&self) -> usize {
        self.count_excluded(|reason| matches!(reason, ExclusionReason::Unrepresentable(_)))
    }

    /// Records excluded because their mapping is ambiguous.
    #[must_use]
    pub fn ambiguous_count(&self) -> usize {
        self.count_excluded(|reason| matches!(reason, ExclusionReason::Ambiguous(_)))
    }

    /// All excluded records.
    #[must_use]
    pub fn excluded_count(&self) -> usize {
        self.records.len() - self.migrated_count()
    }

    fn count_decisions(&self, pred: impl Fn(&MigrationDecision) -> bool) -> usize {
        self.records
            .iter()
            .filter(|record| pred(&record.decision))
            .count()
    }

    fn count_excluded(&self, pred: impl Fn(&ExclusionReason) -> bool) -> usize {
        self.count_decisions(|decision| match decision {
            MigrationDecision::Excluded(reason) => pred(reason),
            MigrationDecision::Migrate { .. } => false,
        })
    }
}

/// The outcome of a successful migration: the executed plan (every
/// source record and its final decision) plus the identity of the
/// created library. Obtained only after the new store was committed,
/// reopened, and verified against the plan.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    plan: MigrationPlan,
    library_id: String,
    generation: u64,
}

impl MigrationReport {
    /// The plan that was executed, including per-record outcomes for
    /// presenting to the user (migrated, retained-missing, external,
    /// unrepresentable, ambiguous, …).
    #[must_use]
    pub fn plan(&self) -> &MigrationPlan {
        &self.plan
    }

    /// The new library's stable identity (as
    /// [`PortableStore::library_id`] reports it).
    #[must_use]
    pub fn library_id(&self) -> &str {
        &self.library_id
    }

    /// The catalogue generation after the migration commit, as
    /// verified by reopening the store.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

// ------------------------------------------------------------- errors ---

/// The stage of the migration sequence at which a failure occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationStage {
    /// Reading the legacy index file failed (I/O).
    ReadLegacyIndex,
    /// The legacy index parsed but failed validation (schema version,
    /// duplicate IDs/paths, impossible counter, malformed JSON).
    ValidateLegacyIndex,
    /// The destination root or any pre-existing `.flux/` store there
    /// made migration refuse before creating anything.
    CheckDestination,
    /// Creating the destination portable store failed.
    CreateDestination,
    /// Importing the planned records into the new store failed.
    ImportRecords,
    /// Committing the new store failed.
    CommitDestination,
    /// Reopening and verifying the committed store against the plan
    /// failed.
    VerifyDestination,
}

impl fmt::Display for MigrationStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::ReadLegacyIndex => "reading the legacy index",
            Self::ValidateLegacyIndex => "validating the legacy index",
            Self::CheckDestination => "checking the destination library root",
            Self::CreateDestination => "creating the destination portable store",
            Self::ImportRecords => "importing records into the destination store",
            Self::CommitDestination => "committing the destination store",
            Self::VerifyDestination => "verifying the destination store after commit",
        };
        write!(f, "{text}")
    }
}

/// Errors produced by planning or executing a migration. Every
/// failure identifies its [`MigrationStage`] and whether a
/// destination store had been created by the time it failed
/// ([`destination_created`](Self::destination_created)); partial
/// destination artifacts are always preserved, never cleaned up
/// automatically, and the legacy index is never modified.
#[derive(Debug)]
pub enum MigrationError {
    /// The legacy index could not be read or failed validation. The
    /// destination was not touched.
    Legacy(StoreError),
    /// The destination root does not exist, is not a directory, or
    /// cannot be canonicalized.
    DestinationRootInvalid {
        /// The destination root supplied by the caller.
        root: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The destination already contains a valid portable library.
    /// Migration never overwrites it.
    DestinationAlreadyInitialized {
        /// The destination root supplied by the caller.
        root: PathBuf,
    },
    /// The destination contains a partial or invalid portable store
    /// (for example leftovers of an interrupted migration, or `.flux`
    /// is not a directory). Every file is preserved for recovery;
    /// migration refuses automatic replacement.
    DestinationPartialOrInvalid {
        /// The destination root supplied by the caller.
        root: PathBuf,
        /// What was found (and why it does not validate).
        detail: String,
    },
    /// The portable storage layer failed during creation, import,
    /// commit, or verification-reopen.
    Portable {
        /// The stage that failed.
        stage: MigrationStage,
        /// Whether a destination catalogue exists as a result of this
        /// attempt (its artifacts are preserved on disk).
        destination_created: bool,
        /// The underlying storage error.
        error: PortableStoreError,
    },
    /// The committed destination store does not match the plan
    /// (records, IDs, counter, paths, metadata, availability). The
    /// destination store exists and is preserved; it must not be
    /// presented as a successful migration.
    Verification {
        /// What disagrees with the plan.
        detail: String,
    },
}

impl MigrationError {
    /// The stage at which migration failed.
    #[must_use]
    pub fn stage(&self) -> MigrationStage {
        match self {
            Self::Legacy(err) => match err {
                StoreError::Io(_) => MigrationStage::ReadLegacyIndex,
                StoreError::Malformed(_)
                | StoreError::UnsupportedVersion { .. }
                | StoreError::InvalidIndex(_) => MigrationStage::ValidateLegacyIndex,
            },
            Self::DestinationRootInvalid { .. }
            | Self::DestinationAlreadyInitialized { .. }
            | Self::DestinationPartialOrInvalid { .. } => MigrationStage::CheckDestination,
            Self::Portable { stage, .. } => *stage,
            Self::Verification { .. } => MigrationStage::VerifyDestination,
        }
    }

    /// Whether a destination portable store (catalogue) had been
    /// created when the failure occurred. When this is `true` the
    /// partial store remains on disk, untouched, for inspection.
    #[must_use]
    pub fn destination_created(&self) -> bool {
        match self {
            Self::Portable {
                destination_created,
                ..
            } => *destination_created,
            Self::Verification { .. } => true,
            Self::Legacy(_)
            | Self::DestinationRootInvalid { .. }
            | Self::DestinationAlreadyInitialized { .. }
            | Self::DestinationPartialOrInvalid { .. } => false,
        }
    }
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Legacy(err) => write!(f, "migration cannot use the legacy index: {err}"),
            Self::DestinationRootInvalid { root, reason } => write!(
                f,
                "destination library root {} is not usable: {reason}",
                root.display()
            ),
            Self::DestinationAlreadyInitialized { root } => write!(
                f,
                "{} already contains a Flux portable library; refusing to overwrite it",
                root.display()
            ),
            Self::DestinationPartialOrInvalid { root, detail } => write!(
                f,
                "{} contains a partial or invalid portable store ({}); \
                 refusing automatic replacement — its files were left untouched",
                root.display(),
                detail
            ),
            Self::Portable {
                stage,
                destination_created,
                error,
            } => write!(
                f,
                "migration failed while {stage}: {error} (destination store {})",
                if *destination_created {
                    "was created and its files were preserved"
                } else {
                    "was not created"
                }
            ),
            Self::Verification { detail } => write!(
                f,
                "the migrated library does not match the migration plan: {detail} \
                 (the destination store was preserved for inspection)"
            ),
        }
    }
}

impl std::error::Error for MigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Legacy(err) => Some(err),
            Self::Portable { error, .. } => Some(error),
            Self::DestinationRootInvalid { .. }
            | Self::DestinationAlreadyInitialized { .. }
            | Self::DestinationPartialOrInvalid { .. }
            | Self::Verification { .. } => None,
        }
    }
}

fn portable_failure(
    stage: MigrationStage,
    root: &Path,
    error: PortableStoreError,
) -> MigrationError {
    // For post-creation stages the catalogue certainly exists; a
    // creation failure may or may not have published one (for example
    // a failing directory sync after the rename) — report what is
    // actually on disk.
    let destination_created = match stage {
        MigrationStage::CreateDestination => {
            root.join(FLUX_DIR_NAME).join(CATALOG_FILE_NAME).exists()
        }
        _ => true,
    };
    MigrationError::Portable {
        stage,
        destination_created,
        error,
    }
}

// ------------------------------------------------------------- planning -

/// Builds a migration plan for `legacy_index` into `destination_root`
/// using the platform-default path case policy. Read-only.
///
/// # Errors
///
/// Any source-wide legacy problem and any destination state that
/// makes migration refuse (see [`MigrationError`]); nothing is
/// written either way.
pub fn plan_migration(
    legacy_index: &Path,
    destination_root: &Path,
) -> Result<MigrationPlan, MigrationError> {
    plan_migration_with_case_policy(
        legacy_index,
        destination_root,
        PathCasePolicy::platform_default(),
    )
}

/// [`plan_migration`] with an explicit path case policy (used by
/// tests and, later, by volume-probing lifecycle code).
pub fn plan_migration_with_case_policy(
    legacy_index: &Path,
    destination_root: &Path,
    case_policy: PathCasePolicy,
) -> Result<MigrationPlan, MigrationError> {
    // 1. Decode + validate the legacy index without ever converting
    //    paths lossily (see `load_legacy_index_data`).
    let data = load_legacy_index_data(legacy_index).map_err(MigrationError::Legacy)?;

    // 2. Duplicate legacy paths are a source-wide validation failure,
    //    exactly as in the legacy loader — but judged here on the raw
    //    stored representation (component-normalized, with the
    //    UTF-8/Unix-bytes twin spellings unified) so the check is
    //    lossless on every platform.
    let mut seen_paths: HashMap<LegacyPathKey, u64> = HashMap::with_capacity(data.tracks.len());
    for track in &data.tracks {
        let original = LegacyPath::from_data(track.path.clone());
        if let Some(previous) = seen_paths.insert(legacy_path_key(&original), track.id) {
            return Err(MigrationError::Legacy(StoreError::InvalidIndex(format!(
                "duplicate track path {} (track IDs {previous} and {})",
                original.display_lossy(),
                track.id
            ))));
        }
    }

    // 3. The destination must be usable and free of any existing or
    //    partial portable store (re-checked again at execution).
    let canonical_root = check_destination(destination_root, case_policy)?;
    let root = RootContext::new(destination_root, canonical_root);

    // 4. Classify every record in source order. The first record (in
    //    source order) to claim a destination library path keeps it;
    //    later collisions are reported as ambiguous rather than
    //    guessed at or silently deduplicated.
    let mut warnings: Vec<String> = Vec::new();
    if data.tracks.windows(2).any(|pair| pair[0].id > pair[1].id) {
        warnings.push(
            "legacy records are not stored in TrackId order; the portable store \
             keeps records in TrackId order, so track order will change"
                .to_owned(),
        );
    }
    let mut claimed: HashMap<PathKey, TrackId> = HashMap::new();
    let mut records: Vec<PlannedRecord> = Vec::with_capacity(data.tracks.len());
    let mut migrated_ids: Vec<u64> = Vec::new();
    for track in data.tracks {
        let source_id = TrackId::new(track.id);
        let original = LegacyPath::from_data(track.path);
        let tags = TagSet {
            title: Some(track.title.clone()),
            artist: track.artist,
            album: track.album,
            album_artist: None,
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
        };
        let decision = match classify(&original, &root) {
            Classified::Inside {
                relative_path,
                availability,
                fingerprint,
            } => {
                let key = path_key(&relative_path, case_policy);
                if let Some(existing) = claimed.get(&key) {
                    MigrationDecision::Excluded(ExclusionReason::Ambiguous(format!(
                        "maps to library path {} already claimed by track {existing}",
                        relative_path.display_lossy()
                    )))
                } else {
                    claimed.insert(key, source_id);
                    migrated_ids.push(track.id);
                    MigrationDecision::Migrate {
                        relative_path,
                        availability,
                        fingerprint,
                        tags,
                    }
                }
            }
            Classified::Excluded(reason) => MigrationDecision::Excluded(reason),
        };
        records.push(PlannedRecord {
            source_id,
            original_path: original,
            title: track.title,
            decision,
        });
    }

    migrated_ids.sort_unstable();
    let id_gaps = compute_id_gaps(&migrated_ids, data.next_id);

    Ok(MigrationPlan {
        source_index: legacy_index.to_owned(),
        destination_root: destination_root.to_owned(),
        case_policy,
        source_next_id: data.next_id,
        records,
        id_gaps,
        warnings,
    })
}

/// Retired-ID ranges below `next_id` given the sorted live IDs.
fn compute_id_gaps(sorted_live_ids: &[u64], next_id: u64) -> Vec<IdGap> {
    let mut gaps = Vec::new();
    let mut covered_through = 0u64;
    for &id in sorted_live_ids {
        if id > covered_through + 1 {
            gaps.push(IdGap {
                first: covered_through + 1,
                last: id - 1,
            });
        }
        covered_through = id;
    }
    // `next_id` was validated to exceed every source ID (and be at
    // least 1), so `covered_through + 1` cannot overflow here.
    if next_id > covered_through + 1 {
        gaps.push(IdGap {
            first: covered_through + 1,
            last: next_id - 1,
        });
    }
    gaps
}

// ----------------------------------------------------- destination gate -

/// Confirms the destination root is an existing directory and does
/// not already contain a valid, partial, or invalid portable store.
/// Returns the canonicalized root for containment checks. Used by
/// both planning and execution (the state could change in between;
/// [`PortableStore::create`] additionally refuses on its own).
fn check_destination(root: &Path, case_policy: PathCasePolicy) -> Result<PathBuf, MigrationError> {
    let root_invalid = |reason: String| MigrationError::DestinationRootInvalid {
        root: root.to_owned(),
        reason,
    };
    let partial = |detail: String| MigrationError::DestinationPartialOrInvalid {
        root: root.to_owned(),
        detail,
    };

    match fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(root_invalid("it exists but is not a directory".to_owned())),
        Err(err) => {
            return Err(root_invalid(format!(
                "it does not exist or cannot be inspected: {err}"
            )));
        }
    }
    let canonical = fs::canonicalize(root)
        .map_err(|err| root_invalid(format!("it cannot be canonicalized: {err}")))?;

    let flux_dir = root.join(FLUX_DIR_NAME);
    match fs::symlink_metadata(&flux_dir) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(canonical),
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
                // A catalogue of some kind is present: valid means an
                // initialized library (never overwrite); anything
                // else is a partial/invalid store to preserve.
                match PortableStore::open_with_case_policy(root, case_policy) {
                    Ok(_) => Err(MigrationError::DestinationAlreadyInitialized {
                        root: root.to_owned(),
                    }),
                    Err(err) => Err(partial(format!("the existing store does not open: {err}"))),
                }
            } else {
                match fs::read_dir(&flux_dir) {
                    Ok(mut entries) => {
                        if entries.next().is_some() {
                            Err(partial(
                                ".flux contains files but no catalogue — the remains of \
                                 an interrupted store creation or a damaged store"
                                    .to_owned(),
                            ))
                        } else {
                            // An empty .flux directory holds no data
                            // to preserve; creating into it overwrites
                            // nothing.
                            Ok(canonical)
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

// -------------------------------------------------------- classification -

/// Canonical + lexical views of the destination root used for
/// containment decisions.
struct RootContext {
    canonical: PathBuf,
    lexical_prefixes: Vec<PathBuf>,
}

impl RootContext {
    fn new(given: &Path, canonical: PathBuf) -> Self {
        let mut prefixes = vec![canonical.clone()];
        if let Ok(absolute) = std::path::absolute(given)
            && !prefixes.contains(&absolute)
        {
            prefixes.push(absolute);
        }
        if given.is_absolute() {
            let given = given.to_path_buf();
            if !prefixes.contains(&given) {
                prefixes.push(given);
            }
        }
        Self {
            canonical,
            lexical_prefixes: prefixes,
        }
    }
}

enum Classified {
    Inside {
        relative_path: RelPath,
        availability: Availability,
        fingerprint: Option<FileFingerprint>,
    },
    Excluded(ExclusionReason),
}

/// The native path a legacy record refers to, without any lossy
/// conversion — `None` when the stored representation cannot be
/// interpreted on this platform (Unix bytes off Unix).
fn native_path(original: &LegacyPath) -> Option<PathBuf> {
    match original {
        LegacyPath::Utf8(text) => Some(PathBuf::from(text)),
        LegacyPath::UnixBytes(bytes) => {
            #[cfg(unix)]
            {
                use std::ffi::OsString;
                use std::os::unix::ffi::OsStringExt;
                Some(PathBuf::from(OsString::from_vec(bytes.clone())))
            }
            #[cfg(not(unix))]
            {
                let _ = bytes;
                None
            }
        }
    }
}

/// Decides where one legacy record belongs relative to the root.
/// Only ever `stat`s inside the root; see the module docs for the
/// full policy and its symlink rules.
fn classify(original: &LegacyPath, root: &RootContext) -> Classified {
    let Some(native) = native_path(original) else {
        return Classified::Excluded(ExclusionReason::Unrepresentable(
            "Unix-byte path cannot be interpreted as a filesystem path on this platform".to_owned(),
        ));
    };
    if !native.is_absolute() {
        return Classified::Excluded(ExclusionReason::Ambiguous(
            "legacy path is not absolute, so the directory it was relative to is unknown"
                .to_owned(),
        ));
    }
    let (Some(file_name), Some(parent)) = (native.file_name(), native.parent()) else {
        return Classified::Excluded(ExclusionReason::Ambiguous(
            "legacy path does not name a file inside a directory".to_owned(),
        ));
    };

    match fs::canonicalize(parent) {
        Ok(canonical_parent) => {
            // The parent exists: its canonical form decides
            // containment authoritatively. A symlinked directory
            // pointing outside the root canonicalizes outside it and
            // the record is external — the link is never used to
            // claim outside content as managed.
            let candidate = canonical_parent.join(file_name);
            let Ok(relative) = candidate.strip_prefix(&root.canonical) else {
                return Classified::Excluded(ExclusionReason::External);
            };
            if relative.as_os_str().is_empty() {
                return Classified::Excluded(ExclusionReason::Ambiguous(
                    "legacy path is the library root itself".to_owned(),
                ));
            }
            let relative_path = match RelPath::from_os_path(relative) {
                Ok(path) => path,
                Err(err) => {
                    return Classified::Excluded(ExclusionReason::Unrepresentable(format!(
                        "path inside the library root cannot be stored portably: {err}"
                    )));
                }
            };
            // The final component is inspected without following it:
            // a symlinked track file is never managed content.
            match fs::symlink_metadata(&candidate) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    Classified::Excluded(ExclusionReason::SymlinkedFile)
                }
                Ok(metadata) if metadata.is_file() => Classified::Inside {
                    relative_path,
                    availability: Availability::Present,
                    fingerprint: Some(fingerprint_of(&metadata)),
                },
                Ok(_) => Classified::Excluded(ExclusionReason::Ambiguous(
                    "path exists inside the library root but is not a regular file".to_owned(),
                )),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Classified::Inside {
                    relative_path,
                    availability: Availability::Missing,
                    fingerprint: None,
                },
                Err(err) => Classified::Excluded(ExclusionReason::Ambiguous(format!(
                    "the file inside the library root could not be verified: {err}"
                ))),
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // The parent directory is gone, so the file is certainly
            // absent; containment can only be lexical. `..` would make
            // even that a guess once the directories that could
            // resolve it no longer exist.
            if native
                .components()
                .any(|component| component == Component::ParentDir)
            {
                return Classified::Excluded(ExclusionReason::Ambiguous(
                    "legacy path contains `..` and its parent directory no longer exists, \
                     so it cannot be mapped safely"
                        .to_owned(),
                ));
            }
            for prefix in &root.lexical_prefixes {
                if let Some(relative) = lexical_relative(&native, prefix) {
                    return match RelPath::from_os_path(&relative) {
                        Ok(path) => Classified::Inside {
                            relative_path: path,
                            availability: Availability::Missing,
                            fingerprint: None,
                        },
                        Err(err) => {
                            Classified::Excluded(ExclusionReason::Unrepresentable(format!(
                                "path inside the library root cannot be stored portably: {err}"
                            )))
                        }
                    };
                }
            }
            Classified::Excluded(ExclusionReason::External)
        }
        Err(err) => Classified::Excluded(ExclusionReason::Ambiguous(format!(
            "the parent directory could not be verified: {err}"
        ))),
    }
}

/// The suffix of `path` under `base`, compared component-wise and
/// lexically (`components()` already normalizes `.` and repeated
/// separators; `..` anywhere defeats the comparison).
fn lexical_relative(path: &Path, base: &Path) -> Option<PathBuf> {
    let base_parts: Vec<Component<'_>> = base.components().collect();
    let path_parts: Vec<Component<'_>> = path.components().collect();
    if path_parts.len() <= base_parts.len() {
        return None;
    }
    for (base_component, path_component) in base_parts.iter().zip(path_parts.iter()) {
        if std::mem::discriminant(base_component) != std::mem::discriminant(path_component)
            || base_component.as_os_str() != path_component.as_os_str()
        {
            return None;
        }
    }
    let mut suffix = PathBuf::new();
    for component in &path_parts[base_parts.len()..] {
        match component {
            Component::Normal(name) => suffix.push(name),
            _ => return None,
        }
    }
    if suffix.as_os_str().is_empty() {
        None
    } else {
        Some(suffix)
    }
}

fn fingerprint_of(metadata: &fs::Metadata) -> FileFingerprint {
    FileFingerprint {
        size_bytes: metadata.len(),
        modified_unix_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0),
    }
}

// -------------------------------------------- legacy path identity keys -

/// Lossless duplicate-detection key for a legacy path: its components
/// in raw bytes. On Unix, `Utf8` and `UnixBytes` spellings of the
/// same path key identically (both decompose into the same OS byte
/// components), matching the legacy loader's `PathBuf` equality; off
/// Unix, stored Unix bytes are decomposed by the Unix rules they were
/// written under. Anything that cannot be decomposed falls back to
/// comparing the stored bytes verbatim.
#[derive(Debug, PartialEq, Eq, Hash)]
enum LegacyPathKey {
    Components {
        prefix: Vec<u8>,
        rooted: bool,
        parts: Vec<LegacyPathPart>,
    },
    Opaque(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum LegacyPathPart {
    Parent,
    Normal(Vec<u8>),
}

fn legacy_path_key(original: &LegacyPath) -> LegacyPathKey {
    match original {
        LegacyPath::Utf8(text) => components_key(Path::new(text))
            .unwrap_or_else(|| LegacyPathKey::Opaque(text.as_bytes().to_vec())),
        LegacyPath::UnixBytes(bytes) => {
            #[cfg(unix)]
            {
                match native_path(original).and_then(|path| components_key(&path)) {
                    Some(key) => key,
                    None => LegacyPathKey::Opaque(bytes.clone()),
                }
            }
            #[cfg(not(unix))]
            {
                unix_bytes_key(bytes)
            }
        }
    }
}

fn components_key(path: &Path) -> Option<LegacyPathKey> {
    let mut prefix = Vec::new();
    let mut rooted = false;
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(value) => prefix = component_bytes(value.as_os_str())?,
            Component::RootDir => rooted = true,
            Component::CurDir => {}
            Component::ParentDir => parts.push(LegacyPathPart::Parent),
            Component::Normal(name) => {
                parts.push(LegacyPathPart::Normal(component_bytes(name)?));
            }
        }
    }
    Some(LegacyPathKey::Components {
        prefix,
        rooted,
        parts,
    })
}

/// The raw bytes of one OS path component, losslessly: OS bytes on
/// Unix; the UTF-8 bytes elsewhere (stored text paths are UTF-8 by
/// construction, so their components are too).
fn component_bytes(name: &std::ffi::OsStr) -> Option<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(name.as_bytes().to_vec())
    }
    #[cfg(not(unix))]
    {
        name.to_str().map(|text| text.as_bytes().to_vec())
    }
}

/// Decomposes stored Unix path bytes under Unix rules on platforms
/// where they cannot become an OS path (duplicate detection only —
/// such records are always reported unrepresentable and are never
/// mapped or touched on the filesystem).
#[cfg(not(unix))]
fn unix_bytes_key(bytes: &[u8]) -> LegacyPathKey {
    let mut parts = Vec::new();
    for component in bytes.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => parts.push(LegacyPathPart::Parent),
            name => parts.push(LegacyPathPart::Normal(name.to_vec())),
        }
    }
    LegacyPathKey::Components {
        prefix: Vec::new(),
        rooted: bytes.first() == Some(&b'/'),
        parts,
    }
}

// ------------------------------------------------------------ execution -

/// Plans and executes a migration in one call (platform-default case
/// policy). Equivalent to [`plan_migration`] followed by
/// [`execute_migration`].
pub fn migrate_legacy_index(
    legacy_index: &Path,
    destination_root: &Path,
) -> Result<MigrationReport, MigrationError> {
    let plan = plan_migration(legacy_index, destination_root)?;
    execute_migration(&plan)
}

/// Executes a previously built [`MigrationPlan`]: re-checks the
/// destination, creates the portable store, imports the planned
/// records with their original IDs, sets the preserved counter,
/// commits through the storage layer's normal protocol, then reopens
/// the store and verifies it against the plan. A success report is
/// returned only after that verification passes.
///
/// The legacy index is not read again here and never written; audio
/// files are not touched. Any failure preserves whatever destination
/// artifacts exist and reports the stage plus
/// [`MigrationError::destination_created`].
pub fn execute_migration(plan: &MigrationPlan) -> Result<MigrationReport, MigrationError> {
    let root = plan.destination_root();

    // Re-check: the destination may have changed since planning, and
    // `create` alone does not distinguish "valid library" from
    // "partial leftovers" for the caller.
    let _canonical_root = check_destination(root, plan.case_policy)?;

    let mut store = PortableStore::create_with_case_policy(root, plan.case_policy)
        .map_err(|err| portable_failure(MigrationStage::CreateDestination, root, err))?;

    // Portable segment order is ascending TrackId; the import API
    // enforces it. Migration time is the honest "added" timestamp
    // for records whose legacy format carries none.
    let now = now_unix_ms();
    let mut imports: Vec<ImportedTrack> = plan
        .records
        .iter()
        .filter_map(|record| match &record.decision {
            MigrationDecision::Migrate {
                relative_path,
                availability,
                fingerprint,
                tags,
            } => Some(ImportedTrack {
                id: record.source_id,
                path: relative_path.clone(),
                availability: *availability,
                file: *fingerprint,
                tags: tags.clone(),
                overrides: MetadataOverrides::new(),
                added_unix_ms: now,
                updated_unix_ms: now,
            }),
            MigrationDecision::Excluded(_) => None,
        })
        .collect();
    imports.sort_by_key(|record| record.id.get());

    store
        .import_tracks(imports, plan.source_next_id)
        .map_err(|err| portable_failure(MigrationStage::ImportRecords, root, err))?;
    store
        .commit()
        .map_err(|err| portable_failure(MigrationStage::CommitDestination, root, err))?;
    drop(store);

    // Reopen and verify before calling this a migration.
    let opened = PortableStore::open_with_case_policy(root, plan.case_policy)
        .map_err(|err| portable_failure(MigrationStage::VerifyDestination, root, err))?;
    if let Some(recovery) = &opened.recovery {
        return Err(MigrationError::Verification {
            detail: format!(
                "the new store opens only through backup recovery \
                 (generation {}, primary error: {})",
                recovery.recovered_generation, recovery.primary_error
            ),
        });
    }
    verify_migrated_store(&opened.store, plan)?;

    Ok(MigrationReport {
        plan: plan.clone(),
        library_id: opened.store.library_id().to_owned(),
        generation: opened.store.generation(),
    })
}

/// Checks every planned migration record, the preserved counter, and
/// the absence of unplanned records in the reopened store.
fn verify_migrated_store(
    store: &PortableStore,
    plan: &MigrationPlan,
) -> Result<(), MigrationError> {
    let verification = |detail: String| MigrationError::Verification { detail };

    if store.next_track_id() != plan.source_next_id {
        return Err(verification(format!(
            "next-ID counter is {} but the plan preserves {}",
            store.next_track_id(),
            plan.source_next_id
        )));
    }
    let planned_count = plan.migrated_count();
    if store.len() != planned_count {
        return Err(verification(format!(
            "store holds {} records but the plan migrates {planned_count}",
            store.len()
        )));
    }
    for record in &plan.records {
        let MigrationDecision::Migrate {
            relative_path,
            availability,
            fingerprint,
            tags,
        } = &record.decision
        else {
            continue;
        };
        let id = record.source_id;
        let Some(stored) = store.get(id) else {
            return Err(verification(format!(
                "migrated track {id} is missing from the new store"
            )));
        };
        if stored.path() != relative_path {
            return Err(verification(format!(
                "track {id} has path {} but the plan maps {}",
                stored.path().display_lossy(),
                relative_path.display_lossy()
            )));
        }
        if stored.availability() != *availability {
            return Err(verification(format!(
                "track {id} has availability {:?} but the plan records {availability:?}",
                stored.availability()
            )));
        }
        if stored.file() != *fingerprint {
            return Err(verification(format!(
                "track {id} has fingerprint {:?} but the plan records {fingerprint:?}",
                stored.file()
            )));
        }
        if stored.tags() != tags {
            return Err(verification(format!(
                "track {id} metadata {:?} does not match the planned legacy metadata {tags:?}",
                stored.tags()
            )));
        }
        if !stored.overrides().is_empty() {
            return Err(verification(format!(
                "track {id} unexpectedly carries metadata overrides"
            )));
        }
        if store.id_for_path(relative_path) != Some(id) {
            return Err(verification(format!(
                "path {} does not resolve back to track {id}",
                relative_path.display_lossy()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use flux_core::TrackId;

    use super::*;
    use crate::store::INDEX_VERSION;
    use crate::test_support::TempDir;

    fn rec_json(id: u64, title: &str, path: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "id": id, "title": title, "artist": null, "album": null,
            "track_number": null, "disc_number": null, "duration_ms": null,
            "path": path
        })
    }

    fn utf8_rec(id: u64, title: &str, path: &Path) -> serde_json::Value {
        rec_json(
            id,
            title,
            serde_json::json!({"utf8": path.to_str().expect("UTF-8 fixture path")}),
        )
    }

    fn index_content(next_id: u64, tracks: Vec<serde_json::Value>) -> String {
        serde_json::json!({"version": INDEX_VERSION, "next_id": next_id, "tracks": tracks})
            .to_string()
    }

    fn write_index(dir: &TempDir, next_id: u64, tracks: Vec<serde_json::Value>) -> PathBuf {
        dir.write(
            "library-index.json",
            index_content(next_id, tracks).as_bytes(),
        )
    }

    fn decision_of(plan: &MigrationPlan, id: u64) -> &MigrationDecision {
        plan.records()
            .iter()
            .find(|record| record.source_id() == TrackId::new(id))
            .expect("record in plan")
            .decision()
    }

    fn snapshot_tree(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).expect("read dir") {
                let entry = entry.expect("dir entry");
                let path = entry.path();
                let file_type = entry.file_type().expect("file type");
                if file_type.is_dir() {
                    walk(&path, base, out);
                } else if file_type.is_file() {
                    out.insert(
                        path.strip_prefix(base).expect("under base").to_path_buf(),
                        fs::read(&path).expect("read file"),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        if root.exists() {
            walk(root, root, &mut out);
        }
        out
    }

    #[test]
    fn successful_migration_preserves_everything_representable() {
        let data = TempDir::new("mig-ok-data");
        let root = TempDir::new("mig-ok-root");
        let song = root.write("Artist/Album/song.flac", b"aaaa");
        let other = root.write("Artist/other.flac", b"bb");
        let dotted_file = root.write("dotted.flac", b"c");
        let dotted = root.path().join("./dotted.flac");
        assert_eq!(dotted_file, root.path().join("dotted.flac"));

        let mut full = utf8_rec(2, "Song", &song);
        full["artist"] = serde_json::json!("The Artist");
        full["album"] = serde_json::json!("The Album");
        full["track_number"] = serde_json::json!(3);
        full["disc_number"] = serde_json::json!(1);
        full["duration_ms"] = serde_json::json!(201_000);
        let index = write_index(
            &data,
            9,
            vec![
                full,
                utf8_rec(5, "Other", &other),
                utf8_rec(8, "Dotted", &dotted),
            ],
        );
        let legacy_before = fs::read(&index).expect("read legacy index");

        let report = migrate_legacy_index(&index, root.path()).expect("migration succeeds");

        // The legacy index and the audio files are byte-for-byte intact.
        assert_eq!(fs::read(&index).expect("reread legacy"), legacy_before);
        assert_eq!(fs::read(&song).expect("audio"), b"aaaa");
        assert_eq!(fs::read(&other).expect("audio"), b"bb");

        let plan = report.plan();
        assert_eq!(plan.source_next_id(), 9);
        assert_eq!(plan.present_count(), 3);
        assert_eq!(plan.missing_count(), 0);
        assert_eq!(plan.excluded_count(), 0);
        assert_eq!(
            plan.id_gaps(),
            [
                IdGap { first: 1, last: 1 },
                IdGap { first: 3, last: 4 },
                IdGap { first: 6, last: 7 },
            ]
        );

        let mut store = PortableStore::open(root.path())
            .expect("open migrated")
            .store;
        assert_eq!(store.library_id(), report.library_id());
        assert_eq!(store.generation(), report.generation());
        assert_eq!(store.len(), 3);
        assert_eq!(store.next_track_id(), 9, "counter preserved");
        let ids: Vec<u64> = store.iter().map(|record| record.id().get()).collect();
        assert_eq!(ids, vec![2, 5, 8], "IDs and gaps preserved, in ID order");

        let song_record = store.get(TrackId::new(2)).expect("track 2");
        assert_eq!(
            song_record.path(),
            &RelPath::from_utf8("Artist/Album/song.flac").expect("rel"),
            "inside-root path mapped to a clean relative path"
        );
        assert_eq!(song_record.availability(), Availability::Present);
        assert_eq!(
            song_record.file().map(|f| f.size_bytes),
            Some(4),
            "fingerprint derived from the verified file"
        );
        assert_eq!(song_record.tags().title.as_deref(), Some("Song"));
        assert_eq!(song_record.tags().artist.as_deref(), Some("The Artist"));
        assert_eq!(song_record.tags().album.as_deref(), Some("The Album"));
        assert_eq!(song_record.tags().track_number, Some(3));
        assert_eq!(song_record.tags().disc_number, Some(1));
        assert_eq!(song_record.tags().duration_ms, Some(201_000));
        assert!(
            song_record.overrides().is_empty(),
            "legacy has no overrides; none are invented"
        );
        assert!(song_record.added_unix_ms() > 0);

        let dotted_record = store.get(TrackId::new(8)).expect("track 8");
        assert_eq!(
            dotted_record.path(),
            &RelPath::from_utf8("dotted.flac").expect("rel"),
            "`.` spelling variants map to the clean relative path"
        );

        // The retired IDs stay retired: allocation continues at 9.
        let fresh = store
            .add_track(
                RelPath::from_utf8("new.flac").expect("rel"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add after migration");
        assert_eq!(fresh, TrackId::new(9));
    }

    #[test]
    fn missing_files_are_retained_as_unavailable() {
        let data = TempDir::new("mig-missing-data");
        let root = TempDir::new("mig-missing-root");
        root.write("sub/keep.txt", b"keep");
        // Parent exists, file absent (canonical-parent tier).
        let missing_shallow = root.path().join("sub/missing.flac");
        // Parent absent too (lexical tier).
        let missing_deep = root.path().join("no-parent/deep/missing.flac");
        let index = write_index(
            &data,
            3,
            vec![
                utf8_rec(1, "Shallow", &missing_shallow),
                utf8_rec(2, "Deep", &missing_deep),
            ],
        );

        let report = migrate_legacy_index(&index, root.path()).expect("migration succeeds");
        assert_eq!(report.plan().present_count(), 0);
        assert_eq!(report.plan().missing_count(), 2);

        let store = PortableStore::open(root.path()).expect("open").store;
        for (id, relative) in [(1, "sub/missing.flac"), (2, "no-parent/deep/missing.flac")] {
            let record = store.get(TrackId::new(id)).expect("retained record");
            assert_eq!(record.availability(), Availability::Missing);
            assert_eq!(record.file(), None);
            assert_eq!(record.path(), &RelPath::from_utf8(relative).expect("rel"));
        }
    }

    #[test]
    fn external_records_are_excluded_without_touching_anything() {
        let data = TempDir::new("mig-ext-data");
        let root = TempDir::new("mig-ext-root");
        let ext = TempDir::new("mig-ext-outside");
        let inside = root.write("inside.flac", b"in");
        let outside = ext.write("outside.flac", b"outside-bytes");
        let index = write_index(
            &data,
            3,
            vec![utf8_rec(1, "In", &inside), utf8_rec(2, "Out", &outside)],
        );
        let legacy_before = fs::read(&index).expect("legacy");

        let plan = plan_migration(&index, root.path()).expect("plan");
        assert_eq!(plan.external_count(), 1);
        assert!(matches!(
            decision_of(&plan, 2),
            MigrationDecision::Excluded(ExclusionReason::External)
        ));
        assert_eq!(
            plan.id_gaps(),
            [IdGap { first: 2, last: 2 }],
            "the excluded ID stays retired via the preserved counter"
        );

        let report = execute_migration(&plan).expect("execute");
        assert_eq!(report.plan().migrated_count(), 1);
        let store = PortableStore::open(root.path()).expect("open").store;
        assert_eq!(store.len(), 1);
        assert!(store.get(TrackId::new(2)).is_none());
        assert_eq!(store.next_track_id(), 3);

        // Nothing copied in, nothing outside modified, source intact.
        assert!(!root.path().join("outside.flac").exists());
        assert_eq!(fs::read(&outside).expect("outside file"), b"outside-bytes");
        assert_eq!(fs::read(&index).expect("legacy"), legacy_before);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_files_are_not_accepted_as_managed_content() {
        let data = TempDir::new("mig-sym-data");
        let root = TempDir::new("mig-sym-root");
        let ext = TempDir::new("mig-sym-outside");
        let real_inside = root.write("real.flac", b"real");
        let real_outside = ext.write("target.flac", b"target");
        let link_out = root.path().join("link-out.flac");
        let link_in = root.path().join("link-in.flac");
        std::os::unix::fs::symlink(&real_outside, &link_out).expect("symlink out");
        std::os::unix::fs::symlink(&real_inside, &link_in).expect("symlink in");

        let index = write_index(
            &data,
            4,
            vec![
                utf8_rec(1, "Real", &real_inside),
                utf8_rec(2, "LinkOut", &link_out),
                utf8_rec(3, "LinkIn", &link_in),
            ],
        );
        let plan = plan_migration(&index, root.path()).expect("plan");
        assert_eq!(plan.symlinked_count(), 2);
        assert_eq!(plan.migrated_count(), 1);

        execute_migration(&plan).expect("execute");
        let store = PortableStore::open(root.path()).expect("open").store;
        assert_eq!(store.len(), 1);
        assert!(store.get(TrackId::new(1)).is_some());
        // The links themselves are left exactly as they were.
        assert!(
            fs::symlink_metadata(&link_out)
                .expect("link")
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&real_outside).expect("target"), b"target");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_unix_bytes_are_preserved_and_migrated_on_unix() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let data = TempDir::new("mig-bytes-data");
        let root = TempDir::new("mig-bytes-root");
        let name: Vec<u8> = b"bad-\xff-name.flac".to_vec();
        let file = root.path().join(OsString::from_vec(name.clone()));
        fs::write(&file, b"weird").expect("write non-UTF-8 file");
        let absolute_bytes = file.as_os_str().as_bytes().to_vec();

        let index = write_index(
            &data,
            4,
            vec![rec_json(
                3,
                "Weird",
                serde_json::json!({"unix_bytes": absolute_bytes.clone()}),
            )],
        );
        let plan = plan_migration(&index, root.path()).expect("plan");
        let planned = &plan.records()[0];
        assert_eq!(
            planned.original_path(),
            &LegacyPath::UnixBytes(absolute_bytes),
            "original bytes preserved exactly in the plan"
        );

        execute_migration(&plan).expect("execute");
        let store = PortableStore::open(root.path()).expect("open").store;
        let record = store.get(TrackId::new(3)).expect("migrated");
        assert_eq!(
            record.path(),
            &RelPath::UnixBytes(name),
            "relative non-UTF-8 bytes preserved byte-for-byte, never lossy-decoded"
        );
        assert_eq!(record.availability(), Availability::Present);
    }

    #[test]
    fn unmappable_paths_are_reported_not_rewritten() {
        let data = TempDir::new("mig-unrep-data");
        let root = TempDir::new("mig-unrep-root");
        let inside = root.write("ok.flac", b"ok");
        // A NUL byte can never name a file; on Unix this reaches the
        // RelPath validation, off Unix the bytes are unrepresentable
        // outright. Either way: reported with bytes intact.
        let mut nul_bytes = root.path().to_str().expect("utf8 root").as_bytes().to_vec();
        nul_bytes.extend_from_slice(b"/bad\0name.flac");

        let index = write_index(
            &data,
            4,
            vec![
                rec_json(
                    1,
                    "Nul",
                    serde_json::json!({"unix_bytes": nul_bytes.clone()}),
                ),
                rec_json(2, "Rel", serde_json::json!({"utf8": "Music/song.flac"})),
                utf8_rec(3, "Ok", &inside),
            ],
        );
        let plan = plan_migration(&index, root.path()).expect("plan");
        assert!(matches!(
            decision_of(&plan, 1),
            MigrationDecision::Excluded(ExclusionReason::Unrepresentable(_))
        ));
        assert_eq!(
            plan.records()[0].original_path(),
            &LegacyPath::UnixBytes(nul_bytes),
            "unrepresentable bytes reported verbatim"
        );
        assert!(
            matches!(
                decision_of(&plan, 2),
                MigrationDecision::Excluded(ExclusionReason::Ambiguous(msg))
                    if msg.contains("not absolute")
            ),
            "relative legacy paths are ambiguous, got {:?}",
            decision_of(&plan, 2)
        );

        execute_migration(&plan).expect("execute");
        let store = PortableStore::open(root.path()).expect("open").store;
        assert_eq!(store.len(), 1);
        assert!(store.get(TrackId::new(3)).is_some());
        assert_eq!(store.next_track_id(), 4);
    }

    #[test]
    fn invalid_sources_fail_before_any_destination_is_created() {
        fn is_malformed(err: &MigrationError) -> bool {
            matches!(err, MigrationError::Legacy(StoreError::Malformed(_)))
        }
        fn is_version(err: &MigrationError) -> bool {
            matches!(
                err,
                MigrationError::Legacy(StoreError::UnsupportedVersion { found: 999, .. })
            )
        }
        fn is_invalid(err: &MigrationError) -> bool {
            matches!(err, MigrationError::Legacy(StoreError::InvalidIndex(_)))
        }

        let rec = |id: u64, path: &str| rec_json(id, "T", serde_json::json!({"utf8": path}));
        type ErrorCheck = fn(&MigrationError) -> bool;
        let cases: Vec<(String, ErrorCheck)> = vec![
            ("{ not json".to_owned(), is_malformed),
            (
                r#"{"version":999,"next_id":1,"tracks":[]}"#.to_owned(),
                is_version,
            ),
            (
                index_content(3, vec![rec(1, "/a.flac"), rec(1, "/b.flac")]),
                is_invalid,
            ),
            (
                index_content(3, vec![rec(1, "/a.flac"), rec(2, "/a.flac")]),
                is_invalid,
            ),
            (
                // A `.` spelling variant is the same legacy path.
                index_content(3, vec![rec(1, "/x/a.flac"), rec(2, "/x/./a.flac")]),
                is_invalid,
            ),
            (index_content(0, vec![]), is_invalid),
            (index_content(2, vec![rec(2, "/a.flac")]), is_invalid),
            (index_content(2, vec![rec(0, "/a.flac")]), is_invalid),
        ];

        for (content, check) in cases {
            let data = TempDir::new("mig-invalid-data");
            let root = TempDir::new("mig-invalid-root");
            let index = data.write("library-index.json", content.as_bytes());
            let before = fs::read(&index).expect("legacy");

            let err = plan_migration(&index, root.path()).expect_err("source must be rejected");
            assert!(check(&err), "unexpected error: {err:?}");
            assert!(!err.destination_created());
            assert!(
                !root.path().join(FLUX_DIR_NAME).exists(),
                "failure before creation leaves no destination store"
            );
            assert_eq!(fs::read(&index).expect("legacy"), before);
        }

        // Missing index file: read-stage I/O failure, still nothing created.
        let data = TempDir::new("mig-invalid-data2");
        let root = TempDir::new("mig-invalid-root2");
        let err = plan_migration(&data.path().join("nope.json"), root.path())
            .expect_err("missing index must fail");
        assert!(matches!(err, MigrationError::Legacy(StoreError::Io(_))));
        assert_eq!(err.stage(), MigrationStage::ReadLegacyIndex);
        assert!(!root.path().join(FLUX_DIR_NAME).exists());
    }

    #[cfg(unix)]
    #[test]
    fn duplicate_utf8_and_unix_bytes_twin_paths_are_invalid() {
        let data = TempDir::new("mig-twin-data");
        let root = TempDir::new("mig-twin-root");
        // The same path stored once as text and once as raw bytes is
        // one file claimed twice (the legacy loader rejects this too).
        let index = write_index(
            &data,
            3,
            vec![
                rec_json(1, "Text", serde_json::json!({"utf8": "/music/twin.flac"})),
                rec_json(
                    2,
                    "Bytes",
                    serde_json::json!({"unix_bytes": b"/music/twin.flac".to_vec()}),
                ),
            ],
        );
        let err = plan_migration(&index, root.path()).expect_err("twin duplicates must fail");
        assert!(matches!(
            err,
            MigrationError::Legacy(StoreError::InvalidIndex(_))
        ));
        assert!(!root.path().join(FLUX_DIR_NAME).exists());
    }

    #[test]
    fn existing_valid_destination_is_refused_untouched() {
        let data = TempDir::new("mig-exists-data");
        let root = TempDir::new("mig-exists-root");
        let inside = root.write("inside.flac", b"in");
        let index = write_index(&data, 2, vec![utf8_rec(1, "In", &inside)]);
        let legacy_before = fs::read(&index).expect("legacy");

        let mut store = PortableStore::create(root.path()).expect("create destination");
        store
            .add_track(
                RelPath::from_utf8("inside.flac").expect("rel"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("seed track");
        store.commit().expect("seed commit");
        drop(store);

        let flux_before = snapshot_tree(&root.path().join(FLUX_DIR_NAME));
        let err = migrate_legacy_index(&index, root.path()).expect_err("must refuse");
        assert!(
            matches!(err, MigrationError::DestinationAlreadyInitialized { .. }),
            "got {err:?}"
        );
        assert_eq!(err.stage(), MigrationStage::CheckDestination);
        assert!(!err.destination_created());
        assert_eq!(
            snapshot_tree(&root.path().join(FLUX_DIR_NAME)),
            flux_before,
            "existing library files byte-for-byte untouched"
        );
        assert_eq!(fs::read(&index).expect("legacy"), legacy_before);

        // Retrying after a *successful* migration refuses the same way.
        let root2 = TempDir::new("mig-exists-root2");
        let inside2 = root2.write("inside.flac", b"in");
        let index2 = write_index(&data, 2, vec![utf8_rec(1, "In", &inside2)]);
        migrate_legacy_index(&index2, root2.path()).expect("first migration");
        let err = migrate_legacy_index(&index2, root2.path()).expect_err("retry must refuse");
        assert!(matches!(
            err,
            MigrationError::DestinationAlreadyInitialized { .. }
        ));
        let store = PortableStore::open(root2.path()).expect("open").store;
        assert_eq!(store.len(), 1, "retry duplicated nothing");
    }

    #[test]
    fn partial_or_invalid_destination_is_refused_untouched() {
        type PartialSetup<'a> = (&'a str, Vec<(&'a str, &'a [u8])>);
        let setups: Vec<PartialSetup<'_>> = vec![
            ("garbage", vec![(CATALOG_FILE_NAME, b"garbage{")]),
            ("segment-only", vec![("tracks-0001@1.flux", b"{}")]),
            ("backup-only", vec![(CATALOG_BACKUP_FILE_NAME, b"{}")]),
        ];
        for (name, files) in setups {
            let data = TempDir::new("mig-partial-data");
            let root = TempDir::new("mig-partial-root");
            let flux_dir = root.path().join(FLUX_DIR_NAME);
            fs::create_dir_all(&flux_dir).expect("make .flux");
            for (file, bytes) in files {
                fs::write(flux_dir.join(file), bytes).expect("plant partial file");
            }
            let index = write_index(&data, 1, vec![]);
            let before = snapshot_tree(&flux_dir);

            let err = plan_migration(&index, root.path())
                .expect_err("partial destination must be refused");
            assert!(
                matches!(err, MigrationError::DestinationPartialOrInvalid { .. }),
                "{name}: got {err:?}"
            );
            assert_eq!(snapshot_tree(&flux_dir), before, "{name}: files preserved");
        }

        // `.flux` as a regular file is preserved and refused too.
        let data = TempDir::new("mig-partial-data2");
        let root = TempDir::new("mig-partial-root2");
        let flux_path = root.path().join(FLUX_DIR_NAME);
        fs::write(&flux_path, b"not a directory").expect("plant file");
        let index = write_index(&data, 1, vec![]);
        let err = plan_migration(&index, root.path()).expect_err("must refuse");
        assert!(matches!(
            err,
            MigrationError::DestinationPartialOrInvalid { .. }
        ));
        assert_eq!(fs::read(&flux_path).expect("file"), b"not a directory");
    }

    #[test]
    fn failure_after_creation_preserves_artifacts_and_retry_is_refused() {
        let data = TempDir::new("mig-fail-data");
        let root = TempDir::new("mig-fail-root");
        let a = root.write("a.flac", b"a");
        let b = root.write("b.flac", b"b");
        let index = write_index(&data, 3, vec![utf8_rec(1, "A", &a), utf8_rec(2, "B", &b)]);
        let legacy_before = fs::read(&index).expect("legacy");

        // Deterministic post-creation failure: corrupt the plan in
        // memory so the import collides (planning files untouched on
        // disk). Execution then has a created-but-empty store.
        let mut plan = plan_migration(&index, root.path()).expect("plan");
        let first_relative = match &plan.records[0].decision {
            MigrationDecision::Migrate { relative_path, .. } => relative_path.clone(),
            other => panic!("expected migrate decision, got {other:?}"),
        };
        match &mut plan.records[1].decision {
            MigrationDecision::Migrate { relative_path, .. } => {
                *relative_path = first_relative;
            }
            other => panic!("expected migrate decision, got {other:?}"),
        }

        let err = execute_migration(&plan).expect_err("tampered plan must fail at import");
        assert_eq!(err.stage(), MigrationStage::ImportRecords);
        assert!(
            err.destination_created(),
            "the created store is reported and preserved"
        );
        assert!(matches!(
            err,
            MigrationError::Portable {
                error: PortableStoreError::DuplicatePath { .. },
                ..
            }
        ));
        assert_eq!(fs::read(&index).expect("legacy"), legacy_before);

        // The partial artifacts remain: a valid, empty store from the
        // interrupted migration — distinguishable from an empty
        // destination, so a retry refuses instead of overwriting it.
        let store = PortableStore::open(root.path())
            .expect("partial store opens")
            .store;
        assert_eq!(store.len(), 0);
        let err = plan_migration(&index, root.path()).expect_err("retry must refuse");
        assert!(matches!(
            err,
            MigrationError::DestinationAlreadyInitialized { .. }
        ));

        // The audio files were never touched.
        assert_eq!(fs::read(&a).expect("a"), b"a");
        assert_eq!(fs::read(&b).expect("b"), b"b");
    }

    #[test]
    fn empty_legacy_index_migrates_to_empty_store_with_preserved_counter() {
        let data = TempDir::new("mig-empty-data");
        let root = TempDir::new("mig-empty-root");
        let index = write_index(&data, 7, vec![]);

        let report = migrate_legacy_index(&index, root.path()).expect("migrate empty");
        assert_eq!(report.plan().migrated_count(), 0);
        assert_eq!(
            report.plan().id_gaps(),
            [IdGap { first: 1, last: 6 }],
            "every previously issued ID stays retired"
        );

        let mut store = PortableStore::open(root.path()).expect("open").store;
        assert_eq!(store.len(), 0);
        assert_eq!(store.next_track_id(), 7);
        let fresh = store
            .add_track(
                RelPath::from_utf8("seven.flac").expect("rel"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add");
        assert_eq!(fresh, TrackId::new(7));
    }

    #[test]
    fn case_insensitive_destination_conflict_is_reported_ambiguous() {
        let data = TempDir::new("mig-case-data");
        let root = TempDir::new("mig-case-root");
        let upper = root.write("A.flac", b"upper");
        let lower = root.write("a.flac", b"lower");
        let index = write_index(
            &data,
            3,
            vec![utf8_rec(1, "Upper", &upper), utf8_rec(2, "Lower", &lower)],
        );

        // Distinct files under the case-sensitive policy…
        let sensitive =
            plan_migration_with_case_policy(&index, root.path(), PathCasePolicy::Sensitive)
                .expect("sensitive plan");
        assert_eq!(sensitive.migrated_count(), 2);

        // …one claimed library path under the case-insensitive one.
        let insensitive =
            plan_migration_with_case_policy(&index, root.path(), PathCasePolicy::Insensitive)
                .expect("insensitive plan");
        assert_eq!(insensitive.migrated_count(), 1);
        assert!(
            matches!(
                decision_of(&insensitive, 2),
                MigrationDecision::Excluded(ExclusionReason::Ambiguous(msg))
                    if msg.contains("already claimed")
            ),
            "got {:?}",
            decision_of(&insensitive, 2)
        );

        execute_migration(&insensitive).expect("execute insensitive plan");
        let opened = PortableStore::open_with_case_policy(root.path(), PathCasePolicy::Insensitive)
            .expect("open");
        assert_eq!(opened.store.len(), 1);
        assert!(opened.store.get(TrackId::new(1)).is_some());
    }

    #[test]
    fn out_of_order_legacy_records_migrate_in_id_order_with_warning() {
        let data = TempDir::new("mig-order-data");
        let root = TempDir::new("mig-order-root");
        let five = root.write("five.flac", b"5");
        let two = root.write("two.flac", b"2");
        // Stored order 5, 2: accepted by the legacy loader, not
        // representable in portable segment order.
        let index = write_index(
            &data,
            6,
            vec![utf8_rec(5, "Five", &five), utf8_rec(2, "Two", &two)],
        );

        let plan = plan_migration(&index, root.path()).expect("plan");
        assert_eq!(plan.warnings().len(), 1);
        assert!(plan.warnings()[0].contains("TrackId order"));

        execute_migration(&plan).expect("execute");
        let store = PortableStore::open(root.path()).expect("open").store;
        let ids: Vec<u64> = store.iter().map(|record| record.id().get()).collect();
        assert_eq!(ids, vec![2, 5]);
    }

    #[test]
    fn planning_is_deterministic_and_writes_nothing() {
        let data = TempDir::new("mig-det-data");
        let root = TempDir::new("mig-det-root");
        let inside = root.write("in/song.flac", b"song");
        let index = write_index(&data, 2, vec![utf8_rec(1, "Song", &inside)]);
        let root_before = snapshot_tree(root.path());

        let first = plan_migration(&index, root.path()).expect("plan");
        let second = plan_migration(&index, root.path()).expect("plan again");
        assert_eq!(first, second, "identical inputs plan identically");
        assert!(!root.path().join(FLUX_DIR_NAME).exists());
        assert_eq!(snapshot_tree(root.path()), root_before);
    }

    #[test]
    fn destination_root_must_be_an_existing_directory() {
        let data = TempDir::new("mig-root-data");
        let index = write_index(&data, 1, vec![]);

        let missing = data.path().join("does-not-exist");
        let err = plan_migration(&index, &missing).expect_err("missing root must fail");
        assert!(matches!(err, MigrationError::DestinationRootInvalid { .. }));
        assert_eq!(err.stage(), MigrationStage::CheckDestination);

        let file_root = data.write("file-root", b"x");
        let err = plan_migration(&index, &file_root).expect_err("file root must fail");
        assert!(matches!(err, MigrationError::DestinationRootInvalid { .. }));
        assert_eq!(fs::read(&file_root).expect("file root"), b"x");
    }
}
