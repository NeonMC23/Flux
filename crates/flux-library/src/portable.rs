//! Portable, segmented `.flux` storage for one Flux music library.
//!
//! A portable library root holds the audio files plus a `.flux/`
//! directory containing ordinary, readable JSON files (with a `.flux`
//! extension):
//!
//! ```text
//! <library-root>/.flux/
//!   catalog.flux                 the commit record (small manifest)
//!   catalog.flux.bak             the previous published catalogue
//!   tracks-<segment_id>@<generation>.flux   immutable track segments
//! ```
//!
//! The catalogue holds the schema version, the library identity, the
//! ID/segment counters, and a descriptor (logical segment ID, file
//! name, generation, record count, ID bounds) for every segment of the
//! committed generation — never the track records themselves. Segment
//! files hold the actual records: stable [`TrackId`], library-relative
//! path ([`RelPath`], byte-exact), last-known availability, a file
//! size/mtime change-detection fingerprint, a snapshot of the embedded
//! tags, and sparse user metadata [`MetadataOverrides`] (Flux never
//! writes audio tags back). Physical placement is *not* stable: records
//! may move between segments when repacked, and rewritten segments get
//! new generation-qualified file names. `TrackId`s never change and are
//! never reused.
//!
//! # Publication protocol
//!
//! [`PortableStore::commit`] publishes in a fixed order: write each new
//! segment generation to a same-directory temp file, `sync_all` it,
//! finalize it by rename **without overwriting** any existing file,
//! sync the directory (where supported), write + sync the new
//! catalogue to a temp file, rotate the currently published catalogue
//! to `catalog.flux.bak` (temp + sync + rename), then atomically rename
//! the new catalogue into place and sync the directory again. The
//! catalogue is the commit record: it only ever references segment
//! files that were fully written and finalized before it. Segment
//! files left behind by an interrupted publication are unreferenced
//! orphans — ignored on load and garbage-collected after a later
//! successful commit (files referenced by either the current or the
//! backup catalogue are always kept). Finalizing a segment whose exact
//! bytes are already in place (a retried commit after a crash) succeeds
//! idempotently; a same-named file with *different* bytes — typically
//! an orphan from a catalogue generation that was lost and recovered
//! from the backup — is never overwritten either: generation
//! allocation steps forward to the next free number, so recovery can
//! never deadlock later commits against their own history.
//!
//! # Recovery ladder
//!
//! [`PortableStore::open`] loads and fully validates the primary
//! catalogue and every segment it references (schema version, file
//! kind, library identity, counters, segment descriptors vs. contents,
//! global track-ID uniqueness and ordering, duplicate path keys). If
//! the primary is missing or invalid, the backup catalogue is tried the
//! same way; a valid backup yields a store in an explicitly reported
//! read-only recovered state ([`OpenedStore::recovery`]) until
//! [`PortableStore::acknowledge_recovery`] restores the primary from
//! it. Records from different catalogue generations are never combined.
//! If neither catalogue validates, opening fails with a typed error —
//! this module never fabricates an empty library over corrupt data, and
//! stores are only ever created through the explicit
//! [`PortableStore::create`].
//!
//! # Paths
//!
//! Stored paths are library-relative [`RelPath`] values: constrained
//! UTF-8 with `/` separators, or raw Unix bytes for names that are not
//! (or cannot be) represented that way. Path values never transit
//! through `PathBuf`/`String` inside the storage layer, so non-UTF-8
//! Unix paths round-trip byte-for-byte through load, unrelated edits,
//! and save — including on platforms where they cannot be resolved to
//! filesystem paths (there they are preserved opaquely, marked
//! unrepresentable, and their path field is frozen). Duplicate
//! detection uses a derived runtime [`PathKey`] under a
//! [`PathCasePolicy`]; the serialized spelling is never altered.
//!
//! # Guarantees and limitations
//!
//! - Atomic *visibility* of segment files and of the catalogue comes
//!   from rename; crash *durability* additionally relies on file and
//!   directory `sync_all`. Directory synchronization is performed on
//!   Unix; on Windows `std` offers no directory sync, so durability
//!   there is best-effort plus the backup ladder (renaming *over* the
//!   catalogue on Windows falls back to remove-then-rename, as in the
//!   legacy store — the backup written just before covers that window).
//! - Validation detects torn/truncated files, stale-generation
//!   references, count/order/uniqueness violations, and counter
//!   regressions. It does not detect in-place bit-rot that keeps the
//!   JSON valid and counts consistent; content checksums are reserved
//!   for a later schema revision. Segment integrity says nothing about
//!   audio-file integrity (size/mtime fingerprints detect change, not
//!   correctness).
//! - This module is the storage layer only: one writer is assumed (no
//!   inter-process locking yet), no filesystem monitoring, import, or
//!   migration logic lives here, and the application's current startup
//!   path continues to use the legacy index in [`crate::store`].

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use flux_core::TrackId;
use serde::{Deserialize, Deserializer, Serialize};

/// The `.flux` schema version this build reads and writes. (The legacy
/// single-file index in [`crate::store`] is an independent format with
/// its own version 1; the portable format starts at 2 so the two are
/// never confused.)
pub const PORTABLE_SCHEMA_VERSION: u32 = 2;

/// Name of the storage directory inside a library root.
pub const FLUX_DIR_NAME: &str = ".flux";

/// File name of the catalogue (the commit record) inside `.flux/`.
pub const CATALOG_FILE_NAME: &str = "catalog.flux";

/// File name of the previous published catalogue inside `.flux/`.
pub const CATALOG_BACKUP_FILE_NAME: &str = "catalog.flux.bak";

/// Provisional segment cap: at most this many records per segment.
pub const MAX_SEGMENT_RECORDS: usize = 1_000;

/// Provisional segment cap: at most ~256 KiB of serialized content per
/// segment (estimated; a single record larger than this still gets its
/// own segment).
pub const MAX_SEGMENT_SERIALIZED_BYTES: usize = 256 * 1024;

const CATALOG_KIND: &str = "flux-catalog";
const SEGMENT_KIND: &str = "flux-tracks";
/// Fixed per-segment allowance folded into serialized-size estimates
/// (file header, brackets, separators).
const SEGMENT_OVERHEAD_BYTES: usize = 256;

// --------------------------------------------------------------- IDs ---

/// Logical identifier of a track segment within one portable library.
///
/// Segment IDs are allocated sequentially and never reused within a
/// library's history — including across repacks, which mint fresh
/// logical segments and retire the old ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SegmentId(u64);

impl SegmentId {
    /// The raw underlying value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SegmentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// -------------------------------------------------------------- paths ---

/// Why a candidate library-relative path was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathValidationError {
    /// The path is empty.
    Empty,
    /// The path is absolute (or has a drive/UNC prefix).
    Absolute,
    /// The path contains a `..` component.
    ParentTraversal,
    /// The path contains an empty component (`//` or a trailing `/`).
    EmptyComponent,
    /// The path contains a `.` component.
    CurrentDirComponent,
    /// A component contains a character forbidden in the portable
    /// UTF-8 form (`\`, `:`, or a control character).
    ForbiddenCharacter(char),
    /// A raw-byte path contains a NUL byte (invalid in Unix file names).
    NulByte,
    /// An OS path component is not valid UTF-8 on a platform where
    /// raw-byte paths are unavailable.
    NonUtf8Component,
}

impl fmt::Display for PathValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "path is empty"),
            Self::Absolute => write!(f, "path is absolute or has a drive/UNC prefix"),
            Self::ParentTraversal => write!(f, "path contains a `..` component"),
            Self::EmptyComponent => write!(f, "path contains an empty component"),
            Self::CurrentDirComponent => write!(f, "path contains a `.` component"),
            Self::ForbiddenCharacter(c) => {
                write!(f, "path component contains forbidden character {c:?}")
            }
            Self::NulByte => write!(f, "path contains a NUL byte"),
            Self::NonUtf8Component => {
                write!(f, "path component is not valid UTF-8 on this platform")
            }
        }
    }
}

impl std::error::Error for PathValidationError {}

/// A library-relative path, stored byte-exactly.
///
/// `Utf8` holds `/`-separated components free of `\`, `:` and control
/// characters (so it means the same thing on every platform);
/// `UnixBytes` holds the raw relative-path bytes with `b'/'`
/// separators for names that cannot use that form. Both forms reject
/// absolute paths, `..`, `.` and empty components.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RelPath {
    /// `/`-separated UTF-8 relative path.
    Utf8(String),
    /// Raw Unix relative-path bytes (`b'/'` separators).
    UnixBytes(Vec<u8>),
}

impl RelPath {
    /// Validates and stores a `/`-separated UTF-8 relative path.
    pub fn from_utf8(path: &str) -> Result<Self, PathValidationError> {
        if path.is_empty() {
            return Err(PathValidationError::Empty);
        }
        if path.starts_with('/') {
            return Err(PathValidationError::Absolute);
        }
        for component in path.split('/') {
            match component {
                "" => return Err(PathValidationError::EmptyComponent),
                "." => return Err(PathValidationError::CurrentDirComponent),
                ".." => return Err(PathValidationError::ParentTraversal),
                _ => {}
            }
            for c in component.chars() {
                if c == '\\' || c == ':' || c.is_control() {
                    return Err(PathValidationError::ForbiddenCharacter(c));
                }
            }
        }
        Ok(Self::Utf8(path.to_owned()))
    }

    /// Validates and stores raw Unix relative-path bytes.
    pub fn from_unix_bytes(bytes: Vec<u8>) -> Result<Self, PathValidationError> {
        if bytes.is_empty() {
            return Err(PathValidationError::Empty);
        }
        if bytes.first() == Some(&b'/') {
            return Err(PathValidationError::Absolute);
        }
        if bytes.contains(&0) {
            return Err(PathValidationError::NulByte);
        }
        for component in bytes.split(|b| *b == b'/') {
            match component {
                b"" => return Err(PathValidationError::EmptyComponent),
                b"." => return Err(PathValidationError::CurrentDirComponent),
                b".." => return Err(PathValidationError::ParentTraversal),
                _ => {}
            }
        }
        Ok(Self::UnixBytes(bytes))
    }

    /// Converts an OS relative path (as a filesystem walk would yield,
    /// already made relative to the library root) into a [`RelPath`].
    /// Purely lexical: no filesystem access, no canonicalization, no
    /// symlink resolution.
    pub fn from_os_path(path: &Path) -> Result<Self, PathValidationError> {
        let mut components: Vec<&std::ffi::OsStr> = Vec::new();
        for component in path.components() {
            match component {
                Component::Normal(os) => components.push(os),
                Component::CurDir => {}
                Component::ParentDir => return Err(PathValidationError::ParentTraversal),
                Component::RootDir | Component::Prefix(_) => {
                    return Err(PathValidationError::Absolute);
                }
            }
        }
        if components.is_empty() {
            return Err(PathValidationError::Empty);
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let text: Option<Vec<&str>> = components.iter().map(|os| os.to_str()).collect();
            if let Some(parts) = text
                && let Ok(rel) = Self::from_utf8(&parts.join("/"))
            {
                return Ok(rel);
            }
            let mut bytes = Vec::new();
            for (i, os) in components.iter().enumerate() {
                if i > 0 {
                    bytes.push(b'/');
                }
                bytes.extend_from_slice(os.as_bytes());
            }
            Self::from_unix_bytes(bytes)
        }
        #[cfg(not(unix))]
        {
            let parts: Option<Vec<&str>> = components.iter().map(|os| os.to_str()).collect();
            match parts {
                Some(parts) => Self::from_utf8(&parts.join("/")),
                None => Err(PathValidationError::NonUtf8Component),
            }
        }
    }

    /// The OS path for this value, or `None` when it cannot be
    /// represented on the current platform (raw Unix bytes on a
    /// non-Unix platform). Never lossy: an unrepresentable path stays
    /// opaque rather than becoming a wrong path.
    #[must_use]
    pub fn to_os_path(&self) -> Option<PathBuf> {
        match self {
            Self::Utf8(text) => {
                let mut out = PathBuf::new();
                for component in text.split('/') {
                    out.push(component);
                }
                Some(out)
            }
            Self::UnixBytes(bytes) => {
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

    /// Whether this path can be resolved to a filesystem path on the
    /// current platform (see [`to_os_path`](Self::to_os_path)).
    #[must_use]
    pub fn is_representable_on_this_platform(&self) -> bool {
        self.to_os_path().is_some()
    }

    /// The UTF-8 spelling, when this is a [`Utf8`](Self::Utf8) path.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Utf8(text) => Some(text),
            Self::UnixBytes(_) => None,
        }
    }

    /// A human-facing rendering for messages and UI only. For raw-byte
    /// paths this is lossy by design; it must never be used as a
    /// filesystem path (use [`to_os_path`](Self::to_os_path)).
    #[must_use]
    pub fn display_lossy(&self) -> String {
        match self {
            Self::Utf8(text) => text.clone(),
            Self::UnixBytes(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        }
    }

    /// The `/`-separated components of the UTF-8 form, if any.
    fn utf8_components(&self) -> Option<Vec<&str>> {
        match self {
            Self::Utf8(text) => Some(text.split('/').collect()),
            Self::UnixBytes(_) => None,
        }
    }

    /// The final component as text for *metadata fallback* titles:
    /// exact for UTF-8 paths, lossy for raw-byte paths (mirroring the
    /// legacy scanner's filename-title fallback; never a path).
    fn file_name_for_fallback(&self) -> String {
        match self {
            Self::Utf8(text) => text.rsplit('/').next().unwrap_or(text).to_owned(),
            Self::UnixBytes(bytes) => {
                let name = bytes.rsplit(|b| *b == b'/').next().unwrap_or(bytes);
                String::from_utf8_lossy(name).into_owned()
            }
        }
    }
}

/// How path comparison keys treat letter case for duplicate detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathCasePolicy {
    /// Keys compare byte-for-byte (Linux and other Unix volumes).
    Sensitive,
    /// Keys are Unicode-lowercased (Windows; the conservative fallback
    /// for macOS until per-volume probing lands with the library
    /// lifecycle milestone).
    Insensitive,
}

impl PathCasePolicy {
    /// The default policy for the current platform.
    ///
    /// Windows is case-insensitive. macOS defaults to case-insensitive:
    /// the approved contract probes the volume at library activation
    /// and falls back to case-insensitive keys when the behavior cannot
    /// be determined — the direction that can over-merge two
    /// case-distinct files into a reported collision but can never
    /// split one physical file into two track identities. Probing
    /// itself is lifecycle work (this storage layer never writes during
    /// [`PortableStore::open`]); until it lands, macOS uses that
    /// fallback. Linux/other Unix is case-sensitive.
    #[must_use]
    pub fn platform_default() -> Self {
        #[cfg(any(windows, target_os = "macos"))]
        {
            Self::Insensitive
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            Self::Sensitive
        }
    }
}

/// Derived runtime key for path-identity comparisons (duplicate
/// detection). Never serialized; the stored [`RelPath`] spelling is
/// never altered to match it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PathKey {
    /// Text key, case-folded per policy.
    Text(String),
    /// Raw-byte key for non-UTF-8 Unix paths (never case-folded).
    Bytes(Vec<u8>),
}

/// Derives the comparison key for `path` under `policy`. Raw Unix
/// bytes that happen to be valid UTF-8 key as text, so a `Utf8` record
/// and a `UnixBytes` twin can never both claim the same OS file.
#[must_use]
pub fn path_key(path: &RelPath, policy: PathCasePolicy) -> PathKey {
    fn fold(text: &str, policy: PathCasePolicy) -> String {
        match policy {
            PathCasePolicy::Sensitive => text.to_owned(),
            PathCasePolicy::Insensitive => text.to_lowercase(),
        }
    }
    match path {
        RelPath::Utf8(text) => PathKey::Text(fold(text, policy)),
        RelPath::UnixBytes(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => PathKey::Text(fold(text, policy)),
            Err(_) => PathKey::Bytes(bytes.clone()),
        },
    }
}

// ----------------------------------------------------------- metadata ---

/// Last-known presence of a track's audio file, as refreshed by the
/// most recent reconciliation. (A library whose root is unreachable is
/// *offline* — a runtime state that is never persisted and never
/// rewrites these values.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// The file was present at the stored relative path.
    Present,
    /// The file was not found; the record (and every reference to it)
    /// is retained.
    Missing,
}

/// Inexpensive change-detection hint for a track's file: size plus
/// modification time. Never an identity by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    /// File length in bytes.
    pub size_bytes: u64,
    /// Modification time, milliseconds since the Unix epoch.
    pub modified_unix_ms: u64,
}

/// Snapshot of a track's embedded tags, as last read from the audio
/// file. Every field is optional; `None` means unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TagSet {
    /// Embedded title.
    pub title: Option<String>,
    /// Embedded track artist.
    pub artist: Option<String>,
    /// Embedded album title.
    pub album: Option<String>,
    /// Embedded album artist.
    pub album_artist: Option<String>,
    /// Track number within its disc/album.
    pub track_number: Option<u32>,
    /// Disc number for multi-disc releases.
    pub disc_number: Option<u32>,
    /// Duration in milliseconds, when the tag source reports one.
    pub duration_ms: Option<u64>,
}

/// Sparse user metadata corrections, stored in the index only (audio
/// tags are never rewritten). Each field is tri-state:
///
/// - *absent* (`None`) — no override; the embedded tag (or fallback)
///   applies;
/// - *cleared* (`Some(None)`) — the user deliberately set the field to
///   unknown; neither the tag nor the folder fallback resurrects it
///   (the title is the one exception: its filename fallback still
///   applies so a track always has a display title);
/// - *set* (`Some(Some(value))`) — the user's value wins.
///
/// The distinction survives serialization exactly: cleared fields are
/// written as explicit JSON `null`, absent fields are omitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetadataOverrides {
    title: Option<Option<String>>,
    artist: Option<Option<String>>,
    album: Option<Option<String>>,
    album_artist: Option<Option<String>>,
    track_number: Option<Option<u32>>,
    disc_number: Option<Option<u32>>,
}

macro_rules! override_accessors {
    ($field:ident, $get:ident, $set:ident, $clear:ident, $revert:ident, $ty:ty, $ref_ty:ty) => {
        /// The tri-state override: `None` absent, `Some(None)` cleared,
        /// `Some(Some(value))` set.
        #[must_use]
        pub fn $get(&self) -> Option<Option<$ref_ty>> {
            self.$field
                .as_ref()
                .map(|v| v.as_ref().map(|s| s as $ref_ty))
        }
        /// Sets the override to an explicit value.
        pub fn $set(&mut self, value: $ty) {
            self.$field = Some(Some(value));
        }
        /// Clears the field to explicitly-unknown (serialized `null`).
        pub fn $clear(&mut self) {
            self.$field = Some(None);
        }
        /// Removes the override entirely (the tag applies again).
        pub fn $revert(&mut self) {
            self.$field = None;
        }
    };
}

impl MetadataOverrides {
    /// An empty override set (every field absent).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether no field carries any override (set or cleared).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.artist.is_none()
            && self.album.is_none()
            && self.album_artist.is_none()
            && self.track_number.is_none()
            && self.disc_number.is_none()
    }

    override_accessors!(
        title,
        title,
        set_title,
        clear_title,
        revert_title,
        String,
        &str
    );
    override_accessors!(
        artist,
        artist,
        set_artist,
        clear_artist,
        revert_artist,
        String,
        &str
    );
    override_accessors!(
        album,
        album,
        set_album,
        clear_album,
        revert_album,
        String,
        &str
    );
    override_accessors!(
        album_artist,
        album_artist,
        set_album_artist,
        clear_album_artist,
        revert_album_artist,
        String,
        &str
    );
    override_accessors!(
        track_number,
        track_number,
        set_track_number,
        clear_track_number,
        revert_track_number,
        u32,
        &u32
    );
    override_accessors!(
        disc_number,
        disc_number,
        set_disc_number,
        clear_disc_number,
        revert_disc_number,
        u32,
        &u32
    );
}

/// Metadata after resolution: override (including explicit clear) →
/// embedded tag → filename/folder fallback, with a guaranteed
/// non-empty title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveMetadata {
    /// Always non-empty (filename-stem fallback).
    pub title: String,
    /// Effective track artist, if known.
    pub artist: Option<String>,
    /// Effective album title, if known.
    pub album: Option<String>,
    /// Effective album artist, if known.
    pub album_artist: Option<String>,
    /// Effective track number, if known.
    pub track_number: Option<u32>,
    /// Effective disc number, if known.
    pub disc_number: Option<u32>,
    /// Duration from the tag snapshot, if known (never overridable).
    pub duration_ms: Option<u64>,
}

// -------------------------------------------------------------- records ---

/// One track record as held by the portable store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredTrack {
    id: TrackId,
    path: RelPath,
    availability: Availability,
    file: Option<FileFingerprint>,
    tags: TagSet,
    overrides: MetadataOverrides,
    added_unix_ms: u64,
    updated_unix_ms: u64,
}

impl StoredTrack {
    /// The stable identity.
    #[must_use]
    pub fn id(&self) -> TrackId {
        self.id
    }

    /// The library-relative locator (mutable only via
    /// [`PortableStore::set_path`], never part of the identity).
    #[must_use]
    pub fn path(&self) -> &RelPath {
        &self.path
    }

    /// Last-known availability.
    #[must_use]
    pub fn availability(&self) -> Availability {
        self.availability
    }

    /// Size/mtime change-detection hint, when known.
    #[must_use]
    pub fn file(&self) -> Option<FileFingerprint> {
        self.file
    }

    /// The embedded-tag snapshot.
    #[must_use]
    pub fn tags(&self) -> &TagSet {
        &self.tags
    }

    /// The sparse user overrides.
    #[must_use]
    pub fn overrides(&self) -> &MetadataOverrides {
        &self.overrides
    }

    /// When the record was added (ms since the Unix epoch).
    #[must_use]
    pub fn added_unix_ms(&self) -> u64 {
        self.added_unix_ms
    }

    /// When the record last changed (ms since the Unix epoch).
    #[must_use]
    pub fn updated_unix_ms(&self) -> u64 {
        self.updated_unix_ms
    }

    /// Resolves the effective metadata (see [`EffectiveMetadata`]).
    /// Folder fallback for artist/album uses the two parent components
    /// of a UTF-8 relative path (`Artist/Album/…`); raw-byte paths get
    /// the title fallback only.
    #[must_use]
    pub fn effective_metadata(&self) -> EffectiveMetadata {
        fn pick<'a>(
            override_: Option<Option<&'a str>>,
            tag: Option<&'a String>,
            folder: Option<&'a str>,
        ) -> Option<String> {
            match override_ {
                Some(Some(value)) => Some(value.to_owned()),
                Some(None) => None, // cleared: tag/folder must not resurrect
                None => tag.cloned().or_else(|| folder.map(str::to_owned)),
            }
        }
        fn pick_num(override_: Option<Option<&u32>>, tag: Option<u32>) -> Option<u32> {
            match override_ {
                Some(Some(value)) => Some(*value),
                Some(None) => None,
                None => tag,
            }
        }

        let components = self.path.utf8_components().unwrap_or_default();
        let folder_album = (components.len() >= 2).then(|| components[components.len() - 2]);
        let folder_artist = (components.len() >= 3).then(|| components[components.len() - 3]);

        let title = match self.overrides.title() {
            Some(Some(value)) => value.to_owned(),
            // Cleared or absent: tag title still applies after a clear?
            // No — a clear suppresses the tag like every other field;
            // only the guaranteed-display filename fallback remains.
            Some(None) => filename_stem(&self.path.file_name_for_fallback()),
            None => self
                .tags
                .title
                .clone()
                .unwrap_or_else(|| filename_stem(&self.path.file_name_for_fallback())),
        };

        EffectiveMetadata {
            title,
            artist: pick(
                self.overrides.artist(),
                self.tags.artist.as_ref(),
                folder_artist,
            ),
            album: pick(
                self.overrides.album(),
                self.tags.album.as_ref(),
                folder_album,
            ),
            album_artist: pick(
                self.overrides.album_artist(),
                self.tags.album_artist.as_ref(),
                None,
            ),
            track_number: pick_num(self.overrides.track_number(), self.tags.track_number),
            disc_number: pick_num(self.overrides.disc_number(), self.tags.disc_number),
            duration_ms: self.tags.duration_ms,
        }
    }
}

/// File-stem fallback title: strips the final `.ext`, never empty.
fn filename_stem(file_name: &str) -> String {
    match file_name.rfind('.') {
        Some(index) if index > 0 => file_name[..index].to_owned(),
        _ => file_name.to_owned(),
    }
}

// --------------------------------------------------------------- errors ---

/// Errors produced by the portable `.flux` store.
#[derive(Debug)]
pub enum PortableStoreError {
    /// A filesystem operation failed.
    Io(std::io::Error),
    /// No portable store exists at the given library root (neither a
    /// primary nor a backup catalogue). Distinct from a corrupt store:
    /// opening never creates one implicitly.
    StoreNotFound {
        /// The library root that was opened.
        root: PathBuf,
    },
    /// [`PortableStore::create`] was asked to initialize a root that
    /// already contains a portable store.
    AlreadyExists {
        /// The library root that was to be initialized.
        root: PathBuf,
    },
    /// A store file is not valid JSON of the expected shape.
    Malformed {
        /// The offending file's name inside `.flux/`.
        file: String,
        /// What the JSON parser reported.
        message: String,
    },
    /// A store file's `kind` discriminator does not match its role.
    WrongKind {
        /// The offending file's name inside `.flux/`.
        file: String,
        /// The kind this file must declare.
        expected: &'static str,
        /// The kind found in the file.
        found: String,
    },
    /// A store file declares an unsupported schema version.
    UnsupportedVersion {
        /// The offending file's name inside `.flux/`.
        file: String,
        /// The version found in the file.
        found: u32,
        /// The version this build supports.
        supported: u32,
    },
    /// The catalogue's `library_id` is not the expected shape.
    InvalidLibraryId {
        /// The value found in the catalogue.
        found: String,
    },
    /// The store parsed but violates a structural invariant not covered
    /// by a more specific variant. The message says which.
    InvalidStore(String),
    /// A next-ID counter is impossible (zero, or would reuse an ID
    /// already present).
    InvalidCounter {
        /// What is wrong with the counter.
        reason: String,
    },
    /// Two records share one `TrackId`.
    DuplicateTrackId {
        /// The duplicated raw ID.
        id: u64,
    },
    /// Two segment descriptors share one logical segment ID.
    DuplicateSegmentId {
        /// The duplicated segment ID.
        segment_id: u64,
    },
    /// Two records claim the same path (under the active
    /// [`PathCasePolicy`]), or an add/rename would create that state.
    DuplicatePath {
        /// The conflicting path (lossy rendering for the message only).
        path: String,
        /// The track already holding that path identity.
        existing: TrackId,
    },
    /// A catalogue references a segment file that is not on disk.
    MissingSegment {
        /// The referenced logical segment.
        segment_id: u64,
        /// The expected file name inside `.flux/`.
        file: String,
    },
    /// A segment file disagrees with its catalogue descriptor
    /// (generation, record count, ID bounds, record ordering, …).
    SegmentMismatch {
        /// The segment concerned.
        segment_id: u64,
        /// What disagrees.
        reason: String,
    },
    /// A stored or candidate relative path is invalid.
    InvalidPath(PathValidationError),
    /// Neither the primary nor the backup catalogue could be loaded
    /// and validated. Nothing was opened and nothing was written.
    BothCataloguesInvalid {
        /// Why the primary catalogue failed.
        primary: String,
        /// Why the backup failed (`None`: no backup exists).
        backup: Option<String>,
    },
    /// A mutation named a track that is not in the store.
    TrackNotFound {
        /// The unknown ID.
        id: TrackId,
    },
    /// The path of a record whose stored path cannot be represented
    /// on this platform was about to be mutated; such records' paths
    /// are frozen here (their metadata may still be edited).
    UnrepresentablePath {
        /// The record concerned.
        id: TrackId,
    },
    /// A store recovered from its backup catalogue is read-only until
    /// [`PortableStore::acknowledge_recovery`] is called.
    ReadOnlyRecovered,
    /// A segment file that publication was about to finalize already
    /// exists with different bytes. Committed segment files are
    /// immutable and are never overwritten.
    SegmentFileExists {
        /// The conflicting file name inside `.flux/`.
        file: String,
    },
}

impl fmt::Display for PortableStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "could not access the portable store files: {err}"),
            Self::StoreNotFound { root } => write!(
                f,
                "no portable Flux library store exists at {}",
                root.display()
            ),
            Self::AlreadyExists { root } => write!(
                f,
                "a portable Flux library store already exists at {}",
                root.display()
            ),
            Self::Malformed { file, message } => {
                write!(f, "store file {file} is malformed: {message}")
            }
            Self::WrongKind {
                file,
                expected,
                found,
            } => write!(
                f,
                "store file {file} declares kind {found:?}, expected {expected:?}"
            ),
            Self::UnsupportedVersion {
                file,
                found,
                supported,
            } => write!(
                f,
                "store file {file} uses unsupported schema version {found} (this build supports version {supported})"
            ),
            Self::InvalidLibraryId { found } => {
                write!(f, "catalogue has an invalid library_id {found:?}")
            }
            Self::InvalidStore(reason) => write!(f, "invalid portable store: {reason}"),
            Self::InvalidCounter { reason } => {
                write!(f, "invalid ID counter in portable store: {reason}")
            }
            Self::DuplicateTrackId { id } => {
                write!(f, "portable store contains track ID {id} more than once")
            }
            Self::DuplicateSegmentId { segment_id } => {
                write!(f, "catalogue lists segment {segment_id} more than once")
            }
            Self::DuplicatePath { path, existing } => write!(
                f,
                "path {path} is already held by track {existing} in this library"
            ),
            Self::MissingSegment { segment_id, file } => write!(
                f,
                "catalogue references segment {segment_id} but {file} is missing"
            ),
            Self::SegmentMismatch { segment_id, reason } => {
                write!(
                    f,
                    "segment {segment_id} disagrees with the catalogue: {reason}"
                )
            }
            Self::InvalidPath(err) => write!(f, "invalid library-relative path: {err}"),
            Self::BothCataloguesInvalid { primary, backup } => match backup {
                Some(backup) => write!(
                    f,
                    "neither the primary catalogue ({primary}) nor the backup catalogue ({backup}) could be loaded"
                ),
                None => write!(
                    f,
                    "the primary catalogue could not be loaded ({primary}) and no backup catalogue exists"
                ),
            },
            Self::TrackNotFound { id } => write!(f, "track {id} is not in this library"),
            Self::UnrepresentablePath { id } => write!(
                f,
                "track {id}'s stored path cannot be represented on this platform; its path is frozen here"
            ),
            Self::ReadOnlyRecovered => write!(
                f,
                "this library was recovered from its backup catalogue and is read-only until the recovery is acknowledged"
            ),
            Self::SegmentFileExists { file } => write!(
                f,
                "segment file {file} already exists with different contents; committed segments are never overwritten"
            ),
        }
    }
}

impl std::error::Error for PortableStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::InvalidPath(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for PortableStoreError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<PathValidationError> for PortableStoreError {
    fn from(err: PathValidationError) -> Self {
        Self::InvalidPath(err)
    }
}

// ---------------------------------------------------------------- DTOs ---

#[derive(Debug, Serialize, Deserialize)]
struct CatalogDto {
    schema_version: u32,
    kind: String,
    library_id: String,
    created_unix_ms: u64,
    created_by: String,
    generation: u64,
    next_track_id: u64,
    next_segment_id: u64,
    #[serde(default)]
    segments: Vec<SegmentDescriptorDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SegmentDescriptorDto {
    segment_id: u64,
    file: String,
    generation: u64,
    record_count: u64,
    first_id: u64,
    last_id: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SegmentDto {
    schema_version: u32,
    kind: String,
    segment_id: u64,
    generation: u64,
    records: Vec<TrackRecordDto>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TrackRecordDto {
    id: u64,
    path: RelPathDto,
    availability: AvailabilityDto,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    file: Option<FileFingerprintDto>,
    #[serde(default)]
    tags: TagSetDto,
    #[serde(default)]
    overrides: OverridesDto,
    added_unix_ms: u64,
    updated_unix_ms: u64,
}

/// Same externally-tagged shape as the legacy store's `StoredPath`
/// (`{"utf8": "…"}` / `{"unix_bytes": [...]}`), with a relative payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RelPathDto {
    Utf8(String),
    UnixBytes(Vec<u8>),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AvailabilityDto {
    Present,
    Missing,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct FileFingerprintDto {
    size_bytes: u64,
    modified_unix_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct TagSetDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    album: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    album_artist: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
}

/// Deserializer for tri-state override fields: key present ⇒
/// `Some(inner)` where JSON `null` ⇒ `None` (explicit clear) and a
/// value ⇒ `Some(value)`. Key absent is handled by `#[serde(default)]`.
fn de_tri_state<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct OverridesDto {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_tri_state"
    )]
    title: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_tri_state"
    )]
    artist: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_tri_state"
    )]
    album: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_tri_state"
    )]
    album_artist: Option<Option<String>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_tri_state"
    )]
    track_number: Option<Option<u32>>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "de_tri_state"
    )]
    disc_number: Option<Option<u32>>,
}

impl From<&RelPath> for RelPathDto {
    fn from(path: &RelPath) -> Self {
        match path {
            RelPath::Utf8(text) => Self::Utf8(text.clone()),
            RelPath::UnixBytes(bytes) => Self::UnixBytes(bytes.clone()),
        }
    }
}

impl RelPathDto {
    fn into_runtime(self) -> Result<RelPath, PortableStoreError> {
        Ok(match self {
            Self::Utf8(text) => RelPath::from_utf8(&text)?,
            Self::UnixBytes(bytes) => RelPath::from_unix_bytes(bytes)?,
        })
    }
}

impl From<&StoredTrack> for TrackRecordDto {
    fn from(track: &StoredTrack) -> Self {
        Self {
            id: track.id.get(),
            path: RelPathDto::from(&track.path),
            availability: match track.availability {
                Availability::Present => AvailabilityDto::Present,
                Availability::Missing => AvailabilityDto::Missing,
            },
            file: track.file.map(|f| FileFingerprintDto {
                size_bytes: f.size_bytes,
                modified_unix_ms: f.modified_unix_ms,
            }),
            tags: TagSetDto {
                title: track.tags.title.clone(),
                artist: track.tags.artist.clone(),
                album: track.tags.album.clone(),
                album_artist: track.tags.album_artist.clone(),
                track_number: track.tags.track_number,
                disc_number: track.tags.disc_number,
                duration_ms: track.tags.duration_ms,
            },
            overrides: OverridesDto {
                title: track.overrides.title.clone(),
                artist: track.overrides.artist.clone(),
                album: track.overrides.album.clone(),
                album_artist: track.overrides.album_artist.clone(),
                track_number: track.overrides.track_number,
                disc_number: track.overrides.disc_number,
            },
            added_unix_ms: track.added_unix_ms,
            updated_unix_ms: track.updated_unix_ms,
        }
    }
}

impl TrackRecordDto {
    fn into_runtime(self) -> Result<StoredTrack, PortableStoreError> {
        if self.id == 0 {
            return Err(PortableStoreError::InvalidStore(
                "a track record has ID 0 (IDs start at 1)".to_owned(),
            ));
        }
        Ok(StoredTrack {
            id: TrackId::new(self.id),
            path: self.path.into_runtime()?,
            availability: match self.availability {
                AvailabilityDto::Present => Availability::Present,
                AvailabilityDto::Missing => Availability::Missing,
            },
            file: self.file.map(|f| FileFingerprint {
                size_bytes: f.size_bytes,
                modified_unix_ms: f.modified_unix_ms,
            }),
            tags: TagSet {
                title: self.tags.title,
                artist: self.tags.artist,
                album: self.tags.album,
                album_artist: self.tags.album_artist,
                track_number: self.tags.track_number,
                disc_number: self.tags.disc_number,
                duration_ms: self.tags.duration_ms,
            },
            overrides: MetadataOverrides {
                title: self.overrides.title,
                artist: self.overrides.artist,
                album: self.overrides.album,
                album_artist: self.overrides.album_artist,
                track_number: self.overrides.track_number,
                disc_number: self.overrides.disc_number,
            },
            added_unix_ms: self.added_unix_ms,
            updated_unix_ms: self.updated_unix_ms,
        })
    }
}

// ------------------------------------------------- runtime segment state ---

#[derive(Debug)]
struct Segment {
    id: SegmentId,
    /// Committed generation on disk; 0 until first published.
    generation: u64,
    /// Records in strictly ascending `TrackId` order.
    records: Vec<StoredTrack>,
    estimated_bytes: usize,
    dirty: bool,
}

impl Segment {
    fn new(id: SegmentId) -> Self {
        Self {
            id,
            generation: 0,
            records: Vec::new(),
            estimated_bytes: SEGMENT_OVERHEAD_BYTES,
            dirty: true,
        }
    }

    fn refresh_estimate(&mut self) {
        self.estimated_bytes =
            SEGMENT_OVERHEAD_BYTES + self.records.iter().map(record_estimate).sum::<usize>();
    }

    fn fits(&self, additional_bytes: usize) -> bool {
        self.records.len() < MAX_SEGMENT_RECORDS
            && (self.records.is_empty()
                || self.estimated_bytes.saturating_add(additional_bytes)
                    <= MAX_SEGMENT_SERIALIZED_BYTES)
    }
}

fn record_estimate(track: &StoredTrack) -> usize {
    match serde_json::to_vec_pretty(&TrackRecordDto::from(track)) {
        Ok(bytes) => {
            // Inside a segment file the record is nested two
            // pretty-print levels deep (the `records` array), so every
            // line carries four more spaces of indentation than this
            // top-level serialization; add those plus the ",\n"
            // separator between records.
            let lines = bytes.iter().filter(|b| **b == b'\n').count() + 1;
            bytes.len() + 4 * lines + 2
        }
        // Serialization of this DTO cannot fail in practice; if it
        // ever did, a huge estimate keeps the record alone in a segment
        // instead of undercounting.
        Err(_) => usize::MAX / 4,
    }
}

fn segment_file_name(segment_id: SegmentId, generation: u64) -> String {
    format!("tracks-{:04}@{generation}.flux", segment_id.get())
}

/// Parses `tracks-<id>@<generation>.flux` names (GC pattern matching;
/// anything else in `.flux/` is never touched).
fn parse_segment_file_name(name: &str) -> Option<(u64, u64)> {
    let rest = name.strip_prefix("tracks-")?.strip_suffix(".flux")?;
    let (id, generation) = rest.split_once('@')?;
    Some((id.parse().ok()?, generation.parse().ok()?))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Uniqueness-grade (not cryptographic) 128-bit library identity from
/// time, PID, and a process counter, mixed with splitmix64. The ID
/// only ever answers "is this the same library as before?".
fn generate_library_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos
        ^ (u64::from(std::process::id()) << 32)
        ^ COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out
}

fn valid_library_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

// ------------------------------------------------------- file primitives ---

fn temp_path_for(dir: &Path, final_name: &str) -> PathBuf {
    dir.join(format!("{final_name}.tmp"))
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), PortableStoreError> {
    let mut file = fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Directory durability: on Unix, syncing the directory persists the
/// rename. `std` has no directory sync on other platforms (notably
/// Windows); there this is a documented no-op and durability rests on
/// the filesystem plus the backup ladder. Unix sync failures are real
/// errors and are propagated, never swallowed.
fn sync_dir(dir: &Path) -> Result<(), PortableStoreError> {
    #[cfg(unix)]
    {
        fs::File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

/// Renames `temp` to `final_name` inside `dir`, never overwriting: a
/// pre-existing file with identical bytes (a retried commit after a
/// crash between finalize and catalogue publish) is accepted
/// idempotently; different bytes are a typed error.
fn finalize_no_overwrite(
    dir: &Path,
    temp: &Path,
    final_name: &str,
    bytes: &[u8],
) -> Result<(), PortableStoreError> {
    let target = dir.join(final_name);
    if target.exists() {
        let existing = fs::read(&target)?;
        if existing == bytes {
            let _ = fs::remove_file(temp);
            return Ok(());
        }
        let _ = fs::remove_file(temp);
        return Err(PortableStoreError::SegmentFileExists {
            file: final_name.to_owned(),
        });
    }
    fs::rename(temp, &target)?;
    Ok(())
}

/// Renames `temp` over `target`, replacing it. On Unix the rename is
/// atomic; on Windows renaming over an existing file fails in `std`,
/// so fall back to remove-then-rename (the legacy store's approach) —
/// callers order operations so a crash in that window is recoverable.
fn replace_file(temp: &Path, target: &Path) -> Result<(), PortableStoreError> {
    match fs::rename(temp, target) {
        Ok(()) => Ok(()),
        Err(first) => {
            if target.exists() {
                fs::remove_file(target)?;
                fs::rename(temp, target)?;
                Ok(())
            } else {
                Err(first.into())
            }
        }
    }
}

// ----------------------------------------------------------------- store ---

/// How an opened store was obtained, when it was recovered from the
/// backup catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryInfo {
    /// Why the primary catalogue could not be used.
    pub primary_error: String,
    /// The catalogue generation that was recovered from the backup.
    pub recovered_generation: u64,
}

/// The result of [`PortableStore::open`]: the store, plus explicit
/// recovery information when it came from the backup catalogue.
#[derive(Debug)]
pub struct OpenedStore {
    /// The opened store (read-only while `recovery` is unacknowledged).
    pub store: PortableStore,
    /// `Some` iff the store was recovered from `catalog.flux.bak`.
    pub recovery: Option<RecoveryInfo>,
}

/// A portable `.flux` library store rooted at one library directory.
///
/// Records are held in memory (grouped into segments) and published
/// explicitly with [`commit`](Self::commit); mutations before a commit
/// are visible through the API but are not durable, and a failed
/// commit leaves them pending for a retry without exposing any
/// partially published state.
#[derive(Debug)]
pub struct PortableStore {
    root: PathBuf,
    flux_dir: PathBuf,
    policy: PathCasePolicy,
    library_id: String,
    created_unix_ms: u64,
    created_by: String,
    /// Committed catalogue generation.
    generation: u64,
    next_track_id: u64,
    next_segment_id: u64,
    /// Segments in catalogue order; concatenating their records yields
    /// strictly ascending track IDs (insertion order).
    segments: Vec<Segment>,
    /// Derived runtime index: track → its logical segment.
    track_segment: HashMap<TrackId, SegmentId>,
    /// Derived runtime index: path key → track.
    path_index: HashMap<PathKey, TrackId>,
    recovered_read_only: bool,
    /// A structural change (a repack) is waiting to be published.
    /// Tracked separately from the per-segment dirty flags because
    /// repacking an emptied store leaves no segment to carry a flag,
    /// yet the on-disk catalogue still has to be replaced.
    pending_structure_change: bool,
}

#[derive(Debug)]
struct LoadedGeneration {
    catalog: CatalogDto,
    segments: Vec<Segment>,
}

impl PortableStore {
    /// Creates a new, empty portable store under `root` (writing the
    /// initial catalogue). Fails if a store already exists there.
    ///
    /// # Errors
    ///
    /// [`PortableStoreError::AlreadyExists`] if a catalogue or backup
    /// is present; I/O failures as [`PortableStoreError::Io`].
    pub fn create(root: &Path) -> Result<Self, PortableStoreError> {
        Self::create_with_case_policy(root, PathCasePolicy::platform_default())
    }

    /// [`create`](Self::create) with an explicit path case policy
    /// (used by tests and, later, by volume-probing lifecycle code).
    pub fn create_with_case_policy(
        root: &Path,
        policy: PathCasePolicy,
    ) -> Result<Self, PortableStoreError> {
        let flux_dir = root.join(FLUX_DIR_NAME);
        if flux_dir.join(CATALOG_FILE_NAME).exists()
            || flux_dir.join(CATALOG_BACKUP_FILE_NAME).exists()
        {
            return Err(PortableStoreError::AlreadyExists {
                root: root.to_owned(),
            });
        }
        fs::create_dir_all(&flux_dir)?;
        let created_unix_ms = now_unix_ms();
        let library_id = generate_library_id();
        let created_by = concat!("flux ", env!("CARGO_PKG_VERSION")).to_owned();
        let catalog = CatalogDto {
            schema_version: PORTABLE_SCHEMA_VERSION,
            kind: CATALOG_KIND.to_owned(),
            library_id: library_id.clone(),
            created_unix_ms,
            created_by: created_by.clone(),
            generation: 1,
            next_track_id: 1,
            next_segment_id: 1,
            segments: Vec::new(),
        };
        let bytes =
            serde_json::to_vec_pretty(&catalog).map_err(|err| PortableStoreError::Malformed {
                file: CATALOG_FILE_NAME.to_owned(),
                message: err.to_string(),
            })?;
        let temp = temp_path_for(&flux_dir, CATALOG_FILE_NAME);
        write_synced(&temp, &bytes)?;
        // Nothing to back up yet; no-overwrite guards against racing
        // creators (single-writer locking is a later milestone).
        finalize_no_overwrite(&flux_dir, &temp, CATALOG_FILE_NAME, &bytes)?;
        sync_dir(&flux_dir)?;
        Ok(Self {
            root: root.to_owned(),
            flux_dir,
            policy,
            library_id,
            created_unix_ms,
            created_by,
            generation: 1,
            next_track_id: 1,
            next_segment_id: 1,
            segments: Vec::new(),
            track_segment: HashMap::new(),
            path_index: HashMap::new(),
            recovered_read_only: false,
            pending_structure_change: false,
        })
    }

    /// Opens the portable store under `root`, following the recovery
    /// ladder described in the module docs. Never creates anything and
    /// never writes during open.
    ///
    /// # Errors
    ///
    /// [`PortableStoreError::StoreNotFound`] when no catalogue exists;
    /// [`PortableStoreError::BothCataloguesInvalid`] when data is
    /// present but neither catalogue validates.
    pub fn open(root: &Path) -> Result<OpenedStore, PortableStoreError> {
        Self::open_with_case_policy(root, PathCasePolicy::platform_default())
    }

    /// [`open`](Self::open) with an explicit path case policy.
    pub fn open_with_case_policy(
        root: &Path,
        policy: PathCasePolicy,
    ) -> Result<OpenedStore, PortableStoreError> {
        let flux_dir = root.join(FLUX_DIR_NAME);
        let primary_path = flux_dir.join(CATALOG_FILE_NAME);
        let backup_path = flux_dir.join(CATALOG_BACKUP_FILE_NAME);
        let (primary_exists, backup_exists) = (primary_path.exists(), backup_path.exists());
        if !primary_exists && !backup_exists {
            return Err(PortableStoreError::StoreNotFound {
                root: root.to_owned(),
            });
        }

        let primary_result = if primary_exists {
            load_generation(&flux_dir, CATALOG_FILE_NAME, policy)
        } else {
            Err(PortableStoreError::InvalidStore(
                "catalog.flux is missing".to_owned(),
            ))
        };
        let loaded = match primary_result {
            Ok(loaded) => {
                return Ok(OpenedStore {
                    store: Self::from_loaded(root, flux_dir, policy, loaded, false),
                    recovery: None,
                });
            }
            Err(err) => err,
        };

        if !backup_exists {
            return Err(PortableStoreError::BothCataloguesInvalid {
                primary: loaded.to_string(),
                backup: None,
            });
        }
        match load_generation(&flux_dir, CATALOG_BACKUP_FILE_NAME, policy) {
            Ok(recovered) => {
                let recovered_generation = recovered.catalog.generation;
                Ok(OpenedStore {
                    store: Self::from_loaded(root, flux_dir, policy, recovered, true),
                    recovery: Some(RecoveryInfo {
                        primary_error: loaded.to_string(),
                        recovered_generation,
                    }),
                })
            }
            Err(backup_err) => Err(PortableStoreError::BothCataloguesInvalid {
                primary: loaded.to_string(),
                backup: Some(backup_err.to_string()),
            }),
        }
    }

    fn from_loaded(
        root: &Path,
        flux_dir: PathBuf,
        policy: PathCasePolicy,
        loaded: LoadedGeneration,
        recovered_read_only: bool,
    ) -> Self {
        let mut track_segment = HashMap::new();
        let mut path_index = HashMap::new();
        for segment in &loaded.segments {
            for record in &segment.records {
                track_segment.insert(record.id, segment.id);
                path_index.insert(path_key(&record.path, policy), record.id);
            }
        }
        Self {
            root: root.to_owned(),
            flux_dir,
            policy,
            library_id: loaded.catalog.library_id,
            created_unix_ms: loaded.catalog.created_unix_ms,
            created_by: loaded.catalog.created_by,
            generation: loaded.catalog.generation,
            next_track_id: loaded.catalog.next_track_id,
            next_segment_id: loaded.catalog.next_segment_id,
            segments: loaded.segments,
            track_segment,
            path_index,
            recovered_read_only,
            pending_structure_change: false,
        }
    }

    /// Acknowledges a backup recovery: restores `catalog.flux` from
    /// the validated backup (temp + sync + rename) and makes the store
    /// writable. A no-op when the store was not recovered.
    pub fn acknowledge_recovery(&mut self) -> Result<(), PortableStoreError> {
        if !self.recovered_read_only {
            return Ok(());
        }
        let backup = self.flux_dir.join(CATALOG_BACKUP_FILE_NAME);
        let bytes = fs::read(&backup)?;
        let temp = temp_path_for(&self.flux_dir, CATALOG_FILE_NAME);
        write_synced(&temp, &bytes)?;
        replace_file(&temp, &self.flux_dir.join(CATALOG_FILE_NAME))?;
        sync_dir(&self.flux_dir)?;
        self.recovered_read_only = false;
        Ok(())
    }

    // -------------------------------------------------------- accessors ---

    /// The library root this store belongs to.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The stable identity of this library (32 hex chars).
    #[must_use]
    pub fn library_id(&self) -> &str {
        &self.library_id
    }

    /// The committed catalogue generation (uncommitted edits do not
    /// advance it).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The active path case policy.
    #[must_use]
    pub fn case_policy(&self) -> PathCasePolicy {
        self.policy
    }

    /// Whether this store is in the read-only recovered state.
    #[must_use]
    pub fn is_recovered_read_only(&self) -> bool {
        self.recovered_read_only
    }

    /// Whether mutations are waiting for [`commit`](Self::commit).
    #[must_use]
    pub fn has_uncommitted_changes(&self) -> bool {
        self.pending_structure_change || self.segments.iter().any(|s| s.dirty)
    }

    /// Number of track records in the store.
    #[must_use]
    pub fn len(&self) -> usize {
        self.track_segment.len()
    }

    /// Whether the store holds no track records.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.track_segment.is_empty()
    }

    /// Number of logical segments currently holding records or pending
    /// publication.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// The record with the given ID.
    #[must_use]
    pub fn get(&self, id: TrackId) -> Option<&StoredTrack> {
        let (seg, rec) = self.locate(id)?;
        self.segments.get(seg)?.records.get(rec)
    }

    /// The logical segment currently holding a track.
    #[must_use]
    pub fn segment_of(&self, id: TrackId) -> Option<SegmentId> {
        self.track_segment.get(&id).copied()
    }

    /// The track holding `path` under the active case policy.
    #[must_use]
    pub fn id_for_path(&self, path: &RelPath) -> Option<TrackId> {
        self.path_index.get(&path_key(path, self.policy)).copied()
    }

    /// All records in insertion (ascending `TrackId`) order.
    pub fn iter(&self) -> impl Iterator<Item = &StoredTrack> {
        self.segments.iter().flat_map(|s| s.records.iter())
    }

    fn locate(&self, id: TrackId) -> Option<(usize, usize)> {
        let segment_id = *self.track_segment.get(&id)?;
        let seg = self.segments.iter().position(|s| s.id == segment_id)?;
        let rec = self.segments[seg]
            .records
            .binary_search_by_key(&id.get(), |r| r.id.get())
            .ok()?;
        Some((seg, rec))
    }

    fn ensure_writable(&self) -> Result<(), PortableStoreError> {
        if self.recovered_read_only {
            Err(PortableStoreError::ReadOnlyRecovered)
        } else {
            Ok(())
        }
    }

    // -------------------------------------------------------- mutations ---

    /// Adds a track record and returns its freshly allocated stable ID.
    /// The record starts with empty overrides; availability and the
    /// fingerprint record what the caller last observed.
    ///
    /// # Errors
    ///
    /// [`PortableStoreError::DuplicatePath`] if another record holds
    /// the path identity; [`PortableStoreError::ReadOnlyRecovered`]
    /// in the recovered state.
    pub fn add_track(
        &mut self,
        path: RelPath,
        availability: Availability,
        file: Option<FileFingerprint>,
        tags: TagSet,
    ) -> Result<TrackId, PortableStoreError> {
        self.ensure_writable()?;
        let key = path_key(&path, self.policy);
        if let Some(existing) = self.path_index.get(&key) {
            return Err(PortableStoreError::DuplicatePath {
                path: path.display_lossy(),
                existing: *existing,
            });
        }
        let now = now_unix_ms();
        let id = TrackId::new(self.next_track_id);
        let record = StoredTrack {
            id,
            path,
            availability,
            file,
            tags,
            overrides: MetadataOverrides::new(),
            added_unix_ms: now,
            updated_unix_ms: now,
        };
        let estimate = record_estimate(&record);
        let seg_index = self.placement_segment(estimate);
        let segment = &mut self.segments[seg_index];
        segment.records.push(record);
        segment.estimated_bytes = segment.estimated_bytes.saturating_add(estimate);
        segment.dirty = true;
        let segment_id = segment.id;
        self.next_track_id += 1;
        self.track_segment.insert(id, segment_id);
        self.path_index.insert(key, id);
        Ok(id)
    }

    /// Chooses (creating if needed) the segment a new record of
    /// `estimate` bytes is appended to: the last segment while it fits
    /// under both caps, else a fresh logical segment. A record larger
    /// than the byte cap always fits an empty segment (alone).
    fn placement_segment(&mut self, estimate: usize) -> usize {
        if let Some(last) = self.segments.last()
            && last.fits(estimate)
        {
            return self.segments.len() - 1;
        }
        let id = SegmentId(self.next_segment_id);
        self.next_segment_id += 1;
        self.segments.push(Segment::new(id));
        self.segments.len() - 1
    }

    /// Removes a track record entirely (its ID is retired forever).
    /// Returns whether a record was present. The audio file, if any,
    /// is not touched — this is an index operation only.
    pub fn remove_track(&mut self, id: TrackId) -> Result<bool, PortableStoreError> {
        self.ensure_writable()?;
        let Some((seg, rec)) = self.locate(id) else {
            return Ok(false);
        };
        let removed = self.segments[seg].records.remove(rec);
        self.segments[seg].refresh_estimate();
        self.segments[seg].dirty = true;
        self.track_segment.remove(&id);
        self.path_index
            .remove(&path_key(&removed.path, self.policy));
        Ok(true)
    }

    fn record_mut(&mut self, id: TrackId) -> Result<&mut StoredTrack, PortableStoreError> {
        let (seg, rec) = self
            .locate(id)
            .ok_or(PortableStoreError::TrackNotFound { id })?;
        Ok(&mut self.segments[seg].records[rec])
    }

    fn touch_segment(&mut self, id: TrackId) {
        if let Some((seg, _)) = self.locate(id) {
            self.segments[seg].refresh_estimate();
            self.segments[seg].dirty = true;
        }
    }

    /// Replaces the embedded-tag snapshot (e.g. after a re-read).
    /// Overrides are untouched.
    pub fn set_tags(&mut self, id: TrackId, tags: TagSet) -> Result<(), PortableStoreError> {
        self.ensure_writable()?;
        let record = self.record_mut(id)?;
        record.tags = tags;
        record.updated_unix_ms = now_unix_ms();
        self.touch_segment(id);
        Ok(())
    }

    /// Replaces the user metadata overrides.
    pub fn set_overrides(
        &mut self,
        id: TrackId,
        overrides: MetadataOverrides,
    ) -> Result<(), PortableStoreError> {
        self.ensure_writable()?;
        let record = self.record_mut(id)?;
        record.overrides = overrides;
        record.updated_unix_ms = now_unix_ms();
        self.touch_segment(id);
        Ok(())
    }

    /// Records the last-known availability.
    pub fn set_availability(
        &mut self,
        id: TrackId,
        availability: Availability,
    ) -> Result<(), PortableStoreError> {
        self.ensure_writable()?;
        let record = self.record_mut(id)?;
        record.availability = availability;
        record.updated_unix_ms = now_unix_ms();
        self.touch_segment(id);
        Ok(())
    }

    /// Records the size/mtime change-detection fingerprint.
    pub fn set_file_fingerprint(
        &mut self,
        id: TrackId,
        file: Option<FileFingerprint>,
    ) -> Result<(), PortableStoreError> {
        self.ensure_writable()?;
        let record = self.record_mut(id)?;
        record.file = file;
        record.updated_unix_ms = now_unix_ms();
        self.touch_segment(id);
        Ok(())
    }

    /// Moves a record's locator to a new relative path (the identity
    /// does not change). Rejected when another record holds the target
    /// path, and when the record's *current* path cannot be
    /// represented on this platform (opaque Unix-byte paths are frozen
    /// on platforms that cannot resolve them).
    pub fn set_path(&mut self, id: TrackId, path: RelPath) -> Result<(), PortableStoreError> {
        self.ensure_writable()?;
        let (seg, rec) = self
            .locate(id)
            .ok_or(PortableStoreError::TrackNotFound { id })?;
        if !self.segments[seg].records[rec]
            .path
            .is_representable_on_this_platform()
        {
            return Err(PortableStoreError::UnrepresentablePath { id });
        }
        let new_key = path_key(&path, self.policy);
        if let Some(existing) = self.path_index.get(&new_key)
            && *existing != id
        {
            return Err(PortableStoreError::DuplicatePath {
                path: path.display_lossy(),
                existing: *existing,
            });
        }
        let record = &mut self.segments[seg].records[rec];
        let old_key = path_key(&record.path, self.policy);
        record.path = path;
        record.updated_unix_ms = now_unix_ms();
        self.path_index.remove(&old_key);
        self.path_index.insert(new_key, id);
        self.touch_segment(id);
        Ok(())
    }

    /// Redistributes all records into fresh logical segments under the
    /// segment caps, preserving record order and every `TrackId` (only
    /// physical placement changes). Takes effect on the next
    /// [`commit`](Self::commit).
    pub fn repack(&mut self) -> Result<(), PortableStoreError> {
        self.ensure_writable()?;
        let records: Vec<StoredTrack> = self
            .segments
            .drain(..)
            .flat_map(|segment| segment.records)
            .collect();
        let mut segments: Vec<Segment> = Vec::new();
        for record in records {
            let estimate = record_estimate(&record);
            let fits = segments.last().is_some_and(|s: &Segment| s.fits(estimate));
            if !fits {
                let id = SegmentId(self.next_segment_id);
                self.next_segment_id += 1;
                segments.push(Segment::new(id));
            }
            let segment = segments.last_mut().expect("segment just ensured");
            segment.records.push(record);
            segment.estimated_bytes = segment.estimated_bytes.saturating_add(estimate);
        }
        self.track_segment.clear();
        for segment in &segments {
            for record in &segment.records {
                self.track_segment.insert(record.id, segment.id);
            }
        }
        self.segments = segments;
        self.pending_structure_change = true;
        Ok(())
    }

    // ----------------------------------------------------------- commit ---

    /// Publishes the current in-memory state as the next catalogue
    /// generation, following the protocol in the module docs. A failed
    /// commit publishes nothing, leaves the pending edits intact for a
    /// retry, and never advances the in-memory committed generation.
    /// Committing with no pending changes is a no-op.
    pub fn commit(&mut self) -> Result<(), PortableStoreError> {
        self.ensure_writable()?;
        // Live segments (publication drops emptied ones).
        let live: Vec<usize> = (0..self.segments.len())
            .filter(|&i| !self.segments[i].records.is_empty())
            .collect();
        if !self.has_uncommitted_changes() && live.len() == self.segments.len() {
            return Ok(());
        }

        // 1. Write, sync, and finalize every dirty live segment,
        //    allocating each one's next generation.
        let mut plan: Vec<(usize, u64)> = Vec::with_capacity(live.len());
        for &i in &live {
            let segment = &self.segments[i];
            let generation = if segment.dirty {
                self.write_segment_generation(segment)?
            } else {
                segment.generation
            };
            plan.push((i, generation));
        }
        // 2. Directory sync: segment finalizations are durable before
        //    anything may reference them.
        sync_dir(&self.flux_dir)?;

        // 3. Write + sync the new catalogue to a temp file.
        let new_generation =
            self.generation
                .checked_add(1)
                .ok_or_else(|| PortableStoreError::InvalidCounter {
                    reason: "catalogue generation overflow".to_owned(),
                })?;
        let descriptors: Vec<SegmentDescriptorDto> = plan
            .iter()
            .map(|&(i, generation)| {
                let segment = &self.segments[i];
                SegmentDescriptorDto {
                    segment_id: segment.id.get(),
                    file: segment_file_name(segment.id, generation),
                    generation,
                    record_count: u64::try_from(segment.records.len()).unwrap_or(u64::MAX),
                    first_id: segment.records.first().map_or(0, |r| r.id.get()),
                    last_id: segment.records.last().map_or(0, |r| r.id.get()),
                }
            })
            .collect();
        let catalog = CatalogDto {
            schema_version: PORTABLE_SCHEMA_VERSION,
            kind: CATALOG_KIND.to_owned(),
            library_id: self.library_id.clone(),
            created_unix_ms: self.created_unix_ms,
            created_by: self.created_by.clone(),
            generation: new_generation,
            next_track_id: self.next_track_id,
            next_segment_id: self.next_segment_id,
            segments: descriptors,
        };
        let catalog_bytes =
            serde_json::to_vec_pretty(&catalog).map_err(|err| PortableStoreError::Malformed {
                file: CATALOG_FILE_NAME.to_owned(),
                message: err.to_string(),
            })?;
        let catalog_temp = temp_path_for(&self.flux_dir, CATALOG_FILE_NAME);
        write_synced(&catalog_temp, &catalog_bytes)?;

        // 4. Rotate the backup from the currently published catalogue.
        let catalog_path = self.flux_dir.join(CATALOG_FILE_NAME);
        if catalog_path.exists() {
            let current = fs::read(&catalog_path)?;
            let backup_temp = temp_path_for(&self.flux_dir, CATALOG_BACKUP_FILE_NAME);
            write_synced(&backup_temp, &current)?;
            replace_file(&backup_temp, &self.flux_dir.join(CATALOG_BACKUP_FILE_NAME))?;
            sync_dir(&self.flux_dir)?;
        }

        // 5. Publish the new catalogue (the commit record), then sync.
        replace_file(&catalog_temp, &catalog_path)?;
        sync_dir(&self.flux_dir)?;

        // 6. Only now expose the new generation in memory.
        self.generation = new_generation;
        for &(i, generation) in &plan {
            self.segments[i].generation = generation;
            self.segments[i].dirty = false;
        }
        self.segments.retain(|s| !s.records.is_empty());
        self.pending_structure_change = false;

        // 7. Garbage-collect unreferenced internal files (best effort).
        self.collect_garbage();
        Ok(())
    }

    /// Serializes, writes, syncs, and finalizes one dirty segment
    /// under its next free generation, returning the generation used.
    ///
    /// Generation allocation starts at `committed + 1` and only moves
    /// forward. A file already occupying the candidate name is never
    /// overwritten: byte-identical content (this same commit retried
    /// after a crash between segment finalization and catalogue
    /// publication) is reused as-is, and different content — typically
    /// an orphaned segment from a catalogue generation that was lost
    /// and recovered from the backup — is stepped past, so recovery
    /// can never deadlock future commits against its own history. The
    /// orphan stays on disk, unreferenced, until garbage collection.
    fn write_segment_generation(&self, segment: &Segment) -> Result<u64, PortableStoreError> {
        const MAX_SKIPPED_GENERATIONS: u64 = 1024;
        let mut candidate = segment.generation.checked_add(1).ok_or_else(|| {
            PortableStoreError::InvalidCounter {
                reason: format!("segment {} generation overflow", segment.id),
            }
        })?;
        loop {
            if candidate > segment.generation.saturating_add(MAX_SKIPPED_GENERATIONS) {
                return Err(PortableStoreError::SegmentFileExists {
                    file: segment_file_name(segment.id, candidate),
                });
            }
            let dto = SegmentDto {
                schema_version: PORTABLE_SCHEMA_VERSION,
                kind: SEGMENT_KIND.to_owned(),
                segment_id: segment.id.get(),
                generation: candidate,
                records: segment.records.iter().map(TrackRecordDto::from).collect(),
            };
            let bytes =
                serde_json::to_vec_pretty(&dto).map_err(|err| PortableStoreError::Malformed {
                    file: segment_file_name(segment.id, candidate),
                    message: err.to_string(),
                })?;
            let final_name = segment_file_name(segment.id, candidate);
            let target = self.flux_dir.join(&final_name);
            if target.exists() {
                if fs::read(&target)? == bytes {
                    return Ok(candidate);
                }
                candidate += 1;
                continue;
            }
            let temp = temp_path_for(&self.flux_dir, &final_name);
            write_synced(&temp, &bytes)?;
            finalize_no_overwrite(&self.flux_dir, &temp, &final_name, &bytes)?;
            return Ok(candidate);
        }
    }

    /// Removes `.flux/` files referenced by neither the current nor the
    /// backup catalogue (orphaned segment generations and our own temp
    /// files). Conservative: if the backup catalogue cannot be parsed,
    /// GC is skipped entirely rather than risking recovery data. Never
    /// touches anything outside the segment/temp patterns.
    fn collect_garbage(&self) {
        let mut referenced: HashSet<String> = HashSet::new();
        for segment in &self.segments {
            referenced.insert(segment_file_name(segment.id, segment.generation));
        }
        let backup_path = self.flux_dir.join(CATALOG_BACKUP_FILE_NAME);
        if backup_path.exists() {
            match fs::read(&backup_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<CatalogDto>(&bytes).ok())
            {
                Some(catalog) => {
                    for descriptor in catalog.segments {
                        referenced.insert(descriptor.file);
                    }
                }
                None => return,
            }
        }
        let Ok(entries) = fs::read_dir(&self.flux_dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let is_own_temp = name.ends_with(".tmp");
            let is_segment = parse_segment_file_name(name).is_some();
            if (is_own_temp || is_segment) && !referenced.contains(name) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

// ---------------------------------------------------------------- loading ---

fn load_generation(
    flux_dir: &Path,
    catalog_file: &str,
    policy: PathCasePolicy,
) -> Result<LoadedGeneration, PortableStoreError> {
    let catalog_bytes = fs::read(flux_dir.join(catalog_file))?;
    let catalog: CatalogDto =
        serde_json::from_slice(&catalog_bytes).map_err(|err| PortableStoreError::Malformed {
            file: catalog_file.to_owned(),
            message: err.to_string(),
        })?;
    if catalog.schema_version != PORTABLE_SCHEMA_VERSION {
        return Err(PortableStoreError::UnsupportedVersion {
            file: catalog_file.to_owned(),
            found: catalog.schema_version,
            supported: PORTABLE_SCHEMA_VERSION,
        });
    }
    if catalog.kind != CATALOG_KIND {
        return Err(PortableStoreError::WrongKind {
            file: catalog_file.to_owned(),
            expected: CATALOG_KIND,
            found: catalog.kind.clone(),
        });
    }
    if !valid_library_id(&catalog.library_id) {
        return Err(PortableStoreError::InvalidLibraryId {
            found: catalog.library_id.clone(),
        });
    }
    if catalog.generation == 0 {
        return Err(PortableStoreError::InvalidStore(
            "catalogue generation is 0".to_owned(),
        ));
    }
    if catalog.next_track_id == 0 {
        return Err(PortableStoreError::InvalidCounter {
            reason: "next_track_id is 0 (IDs start at 1)".to_owned(),
        });
    }
    if catalog.next_segment_id == 0 {
        return Err(PortableStoreError::InvalidCounter {
            reason: "next_segment_id is 0 (segment IDs start at 1)".to_owned(),
        });
    }

    let mut seen_segments: HashSet<u64> = HashSet::new();
    let mut seen_tracks: HashSet<u64> = HashSet::new();
    let mut seen_paths: HashMap<PathKey, TrackId> = HashMap::new();
    let mut previous_id = 0u64;
    let mut segments = Vec::with_capacity(catalog.segments.len());

    for descriptor in &catalog.segments {
        let segment_id = descriptor.segment_id;
        if segment_id == 0 {
            return Err(PortableStoreError::InvalidStore(
                "a segment descriptor has segment ID 0".to_owned(),
            ));
        }
        if !seen_segments.insert(segment_id) {
            return Err(PortableStoreError::DuplicateSegmentId { segment_id });
        }
        if segment_id >= catalog.next_segment_id {
            return Err(PortableStoreError::InvalidCounter {
                reason: format!(
                    "next_segment_id {} would reuse segment {segment_id}",
                    catalog.next_segment_id
                ),
            });
        }
        // The descriptor's file name is data, never a trusted path:
        // derive the only name this segment may have and require
        // agreement.
        let expected_file = segment_file_name(SegmentId(segment_id), descriptor.generation);
        if descriptor.file != expected_file {
            return Err(PortableStoreError::SegmentMismatch {
                segment_id,
                reason: format!(
                    "descriptor names file {:?}, expected {expected_file:?}",
                    descriptor.file
                ),
            });
        }
        if descriptor.record_count == 0 {
            return Err(PortableStoreError::SegmentMismatch {
                segment_id,
                reason: "descriptor declares zero records".to_owned(),
            });
        }

        let segment_bytes = match fs::read(flux_dir.join(&expected_file)) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(PortableStoreError::MissingSegment {
                    segment_id,
                    file: expected_file,
                });
            }
            Err(err) => return Err(err.into()),
        };
        let segment_dto: SegmentDto = serde_json::from_slice(&segment_bytes).map_err(|err| {
            PortableStoreError::Malformed {
                file: expected_file.clone(),
                message: err.to_string(),
            }
        })?;
        if segment_dto.schema_version != PORTABLE_SCHEMA_VERSION {
            return Err(PortableStoreError::UnsupportedVersion {
                file: expected_file.clone(),
                found: segment_dto.schema_version,
                supported: PORTABLE_SCHEMA_VERSION,
            });
        }
        if segment_dto.kind != SEGMENT_KIND {
            return Err(PortableStoreError::WrongKind {
                file: expected_file.clone(),
                expected: SEGMENT_KIND,
                found: segment_dto.kind.clone(),
            });
        }
        if segment_dto.segment_id != segment_id {
            return Err(PortableStoreError::SegmentMismatch {
                segment_id,
                reason: format!("file declares segment {} instead", segment_dto.segment_id),
            });
        }
        if segment_dto.generation != descriptor.generation {
            return Err(PortableStoreError::SegmentMismatch {
                segment_id,
                reason: format!(
                    "file is generation {}, catalogue references {}",
                    segment_dto.generation, descriptor.generation
                ),
            });
        }
        if u64::try_from(segment_dto.records.len()).unwrap_or(u64::MAX) != descriptor.record_count {
            return Err(PortableStoreError::SegmentMismatch {
                segment_id,
                reason: format!(
                    "file holds {} records, catalogue declares {}",
                    segment_dto.records.len(),
                    descriptor.record_count
                ),
            });
        }

        let mut records = Vec::with_capacity(segment_dto.records.len());
        for record_dto in segment_dto.records {
            let record = record_dto.into_runtime()?;
            let raw_id = record.id.get();
            if !seen_tracks.insert(raw_id) {
                return Err(PortableStoreError::DuplicateTrackId { id: raw_id });
            }
            if raw_id <= previous_id {
                return Err(PortableStoreError::SegmentMismatch {
                    segment_id,
                    reason: format!(
                        "track IDs are not in strictly ascending order (…, {previous_id}, {raw_id})"
                    ),
                });
            }
            previous_id = raw_id;
            if raw_id >= catalog.next_track_id {
                return Err(PortableStoreError::InvalidCounter {
                    reason: format!(
                        "next_track_id {} would reuse existing track ID {raw_id}",
                        catalog.next_track_id
                    ),
                });
            }
            let key = path_key(&record.path, policy);
            if let Some(existing) = seen_paths.get(&key) {
                return Err(PortableStoreError::DuplicatePath {
                    path: record.path.display_lossy(),
                    existing: *existing,
                });
            }
            seen_paths.insert(key, record.id);
            records.push(record);
        }
        if records.first().map(|r| r.id.get()) != Some(descriptor.first_id)
            || records.last().map(|r| r.id.get()) != Some(descriptor.last_id)
        {
            return Err(PortableStoreError::SegmentMismatch {
                segment_id,
                reason: format!(
                    "record ID bounds differ from the catalogue's [{}, {}]",
                    descriptor.first_id, descriptor.last_id
                ),
            });
        }

        let mut segment = Segment {
            id: SegmentId(segment_id),
            generation: descriptor.generation,
            records,
            estimated_bytes: 0,
            dirty: false,
        };
        segment.refresh_estimate();
        segments.push(segment);
    }

    Ok(LoadedGeneration { catalog, segments })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use flux_core::TrackId;

    use super::*;
    use crate::test_support::TempDir;

    fn rel(path: &str) -> RelPath {
        RelPath::from_utf8(path).expect("valid relative path")
    }

    fn flux_dir(dir: &TempDir) -> PathBuf {
        dir.path().join(FLUX_DIR_NAME)
    }

    fn title_tags(title: &str) -> TagSet {
        TagSet {
            title: Some(title.to_owned()),
            ..TagSet::default()
        }
    }

    /// Creates a store with `n` simple committed tracks (IDs 1..=n).
    /// The records are deliberately minimal (~230 serialized bytes
    /// each: short path, no tags, no fingerprint) so that the
    /// 1,000-record segment cap binds well before the 256 KiB byte
    /// cap (~262 bytes/record average); tests for the byte cap build
    /// their own fat fixtures. (Sizing matters: an earlier version of
    /// this fixture produced ~300-byte records, the byte cap bound at
    /// ~850 records per segment, and every segment-count expectation
    /// built on the record cap failed.)
    fn seeded(dir: &TempDir, n: usize) -> PortableStore {
        let mut store = PortableStore::create(dir.path()).expect("create store");
        for i in 0..n {
            store
                .add_track(
                    rel(&format!("{i:04}.flac")),
                    Availability::Present,
                    None,
                    TagSet::default(),
                )
                .expect("add track");
        }
        store.commit().expect("commit seed");
        store
    }

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(path).expect("read json file")).expect("parse json")
    }

    fn write_json(path: &Path, value: &serde_json::Value) {
        fs::write(
            path,
            serde_json::to_vec_pretty(value).expect("serialize json"),
        )
        .expect("write json file");
    }

    fn segment_files(dir: &TempDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(flux_dir(dir))
            .expect("read .flux dir")
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| parse_segment_file_name(n).is_some())
            .collect();
        names.sort();
        names
    }

    fn catalog_value(dir: &TempDir) -> serde_json::Value {
        read_json(&flux_dir(dir).join(CATALOG_FILE_NAME))
    }

    #[test]
    fn open_without_store_is_not_found_and_create_is_explicit() {
        let dir = TempDir::new("portable_create");
        let err = PortableStore::open(dir.path()).expect_err("no store yet");
        assert!(
            matches!(err, PortableStoreError::StoreNotFound { .. }),
            "got: {err}"
        );
        assert!(!flux_dir(&dir).exists(), "open must never create anything");

        let store = PortableStore::create(dir.path()).expect("create");
        assert!(store.is_empty());
        assert_eq!(store.generation(), 1);
        assert_eq!(store.library_id().len(), 32);
        drop(store);

        assert!(matches!(
            PortableStore::create(dir.path()),
            Err(PortableStoreError::AlreadyExists { .. })
        ));

        let opened = PortableStore::open(dir.path()).expect("open created store");
        assert!(opened.recovery.is_none());
        assert!(opened.store.is_empty());
        assert!(!opened.store.is_recovered_read_only());
    }

    #[test]
    fn round_trip_multiple_segments_by_record_cap() {
        let dir = TempDir::new("portable_record_cap");
        let store = seeded(&dir, 2500);
        assert_eq!(store.len(), 2500);
        assert_eq!(store.segment_count(), 3);
        let library_id = store.library_id().to_owned();
        drop(store);

        assert_eq!(segment_files(&dir).len(), 3);
        let catalog = catalog_value(&dir);
        let counts: Vec<i64> = catalog["segments"]
            .as_array()
            .expect("segments array")
            .iter()
            .map(|s| s["record_count"].as_i64().expect("count"))
            .collect();
        assert_eq!(counts, vec![1000, 1000, 500]);
        assert!(
            counts.iter().all(|c| *c <= MAX_SEGMENT_RECORDS as i64),
            "segment record cap respected"
        );

        let opened = PortableStore::open(dir.path()).expect("reopen");
        let store = opened.store;
        assert_eq!(store.library_id(), library_id);
        assert_eq!(store.len(), 2500);
        let ids: Vec<u64> = store.iter().map(|t| t.id().get()).collect();
        assert_eq!(ids, (1..=2500).collect::<Vec<_>>(), "insertion order kept");
        assert_ne!(
            store.segment_of(TrackId::new(1)),
            store.segment_of(TrackId::new(2500))
        );
        assert_eq!(
            store.get(TrackId::new(1234)).expect("track").path(),
            &rel("1233.flac")
        );
    }

    #[test]
    fn byte_cap_rolls_segments_and_oversized_record_stands_alone() {
        let dir = TempDir::new("portable_byte_cap");
        let mut store = PortableStore::create(dir.path()).expect("create");
        let fat_title = "x".repeat(2048);
        for i in 0..300 {
            store
                .add_track(
                    rel(&format!("A/B/{i:04}.flac")),
                    Availability::Present,
                    None,
                    title_tags(&fat_title),
                )
                .expect("add fat track");
        }
        store.commit().expect("commit");
        assert!(
            store.segment_count() >= 2,
            "256 KiB cap must roll before 300 fat records accumulate, got {} segment(s)",
            store.segment_count()
        );
        // The cap is a property of the files on disk, not just of the
        // packing estimate.
        for name in segment_files(&dir) {
            let len = fs::metadata(flux_dir(&dir).join(&name))
                .expect("segment metadata")
                .len();
            assert!(
                usize::try_from(len).unwrap_or(usize::MAX) <= MAX_SEGMENT_SERIALIZED_BYTES,
                "multi-record segment {name} is {len} bytes, over the 256 KiB cap"
            );
        }
        drop(store);

        // One record larger than the whole byte cap: it gets its own
        // segment, deterministically, between its neighbours.
        let dir2 = TempDir::new("portable_oversized");
        let mut store = PortableStore::create(dir2.path()).expect("create");
        for i in 0..10 {
            store
                .add_track(
                    rel(&format!("A/B/small{i}.flac")),
                    Availability::Present,
                    None,
                    title_tags("small"),
                )
                .expect("add small");
        }
        let big = store
            .add_track(
                rel("A/B/big.flac"),
                Availability::Present,
                None,
                title_tags(&"y".repeat(300_000)),
            )
            .expect("add oversized");
        for i in 0..10 {
            store
                .add_track(
                    rel(&format!("A/B/tail{i}.flac")),
                    Availability::Present,
                    None,
                    title_tags("small"),
                )
                .expect("add tail");
        }
        store.commit().expect("commit");
        assert_eq!(store.segment_count(), 3);
        let big_segment = store.segment_of(big).expect("big located");
        let in_big_segment = store
            .iter()
            .filter(|t| store.segment_of(t.id()) == Some(big_segment))
            .count();
        assert_eq!(in_big_segment, 1, "oversized record stands alone");
        drop(store);

        let opened = PortableStore::open(dir2.path()).expect("reopen oversized");
        assert_eq!(opened.store.len(), 21);
        assert_eq!(
            opened
                .store
                .get(big)
                .expect("big")
                .tags()
                .title
                .as_ref()
                .map(String::len),
            Some(300_000)
        );
    }

    #[test]
    fn track_ids_are_never_reused_across_remove_and_reopen() {
        let dir = TempDir::new("portable_id_gaps");
        let mut store = seeded(&dir, 3);
        assert!(store.remove_track(TrackId::new(2)).expect("remove"));
        store.commit().expect("commit");
        drop(store);

        let mut store = PortableStore::open(dir.path()).expect("reopen").store;
        assert_eq!(store.len(), 2);
        assert!(store.get(TrackId::new(2)).is_none());
        let fresh = store
            .add_track(
                rel("Artist/Album/new.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add after reopen");
        assert_eq!(fresh, TrackId::new(4), "counter survives with its gap");
        store.commit().expect("commit");
        drop(store);

        let store = PortableStore::open(dir.path()).expect("reopen again").store;
        let ids: Vec<u64> = store.iter().map(|t| t.id().get()).collect();
        assert_eq!(ids, vec![1, 3, 4]);
    }

    #[test]
    fn repack_preserves_ids_order_and_mints_new_segments() {
        let dir = TempDir::new("portable_repack");
        let mut store = seeded(&dir, 1500);
        let old_first_segment = store.segment_of(TrackId::new(950));
        for raw in 1..=900 {
            assert!(store.remove_track(TrackId::new(raw)).expect("remove"));
        }
        store.commit().expect("commit removals");
        assert_eq!(store.segment_count(), 2, "100 + 500 across two segments");

        store.repack().expect("repack");
        assert_eq!(
            store.segment_count(),
            1,
            "600 small records repack into one"
        );
        let new_segment = store.segment_of(TrackId::new(950));
        assert_ne!(
            new_segment, old_first_segment,
            "repack mints fresh segment IDs"
        );
        store.commit().expect("commit repack");
        drop(store);

        let store = PortableStore::open(dir.path()).expect("reopen").store;
        let ids: Vec<u64> = store.iter().map(|t| t.id().get()).collect();
        assert_eq!(
            ids,
            (901..=1500).collect::<Vec<_>>(),
            "IDs and order untouched"
        );
        assert_eq!(store.segment_of(TrackId::new(950)), new_segment);
    }

    #[test]
    fn duplicate_paths_rejected_on_add_and_set_path() {
        let dir = TempDir::new("portable_dup_path");
        let mut store = PortableStore::create(dir.path()).expect("create");
        let first = store
            .add_track(
                rel("A/B.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("first");
        let err = store
            .add_track(
                rel("A/B.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect_err("duplicate add");
        assert!(
            matches!(err, PortableStoreError::DuplicatePath { existing, .. } if existing == first),
            "got: {err}"
        );
        let second = store
            .add_track(
                rel("A/C.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("second");
        let err = store
            .set_path(second, rel("A/B.flac"))
            .expect_err("duplicate set_path");
        assert!(
            matches!(err, PortableStoreError::DuplicatePath { .. }),
            "got: {err}"
        );
        // Case-sensitive by default on this platform policy:
        assert_eq!(store.id_for_path(&rel("a/b.flac")), None);
        assert_eq!(store.id_for_path(&rel("A/B.flac")), Some(first));

        // Under the insensitive policy, case variants collide and
        // lookups fold — while the stored spelling is preserved.
        let dir2 = TempDir::new("portable_dup_path_ci");
        let mut store =
            PortableStore::create_with_case_policy(dir2.path(), PathCasePolicy::Insensitive)
                .expect("create ci");
        let first = store
            .add_track(
                rel("A/B.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("first ci");
        assert!(matches!(
            store.add_track(
                rel("a/b.flac"),
                Availability::Present,
                None,
                TagSet::default()
            ),
            Err(PortableStoreError::DuplicatePath { .. })
        ));
        assert_eq!(store.id_for_path(&rel("a/B.FLAC")), Some(first));
        assert_eq!(
            store.get(first).expect("ci record").path(),
            &rel("A/B.flac")
        );
    }

    #[test]
    fn set_path_moves_locator_and_updates_index() {
        let dir = TempDir::new("portable_set_path");
        let mut store = seeded(&dir, 2);
        let id = TrackId::new(1);
        let old = store.get(id).expect("seeded").path().clone();
        store
            .set_path(id, rel("Moved/Elsewhere.flac"))
            .expect("set_path");
        assert_eq!(store.id_for_path(&old), None);
        assert_eq!(store.id_for_path(&rel("Moved/Elsewhere.flac")), Some(id));
        store.commit().expect("commit");
        drop(store);

        let store = PortableStore::open(dir.path()).expect("reopen").store;
        assert_eq!(
            store.get(id).expect("moved").path(),
            &rel("Moved/Elsewhere.flac")
        );
        assert_eq!(store.id_for_path(&rel("Moved/Elsewhere.flac")), Some(id));
    }

    #[test]
    fn commit_without_changes_is_a_noop() {
        let dir = TempDir::new("portable_noop_commit");
        let mut store = seeded(&dir, 2);
        let generation = store.generation();
        let catalog_before = fs::read(flux_dir(&dir).join(CATALOG_FILE_NAME)).expect("catalog");
        assert!(!store.has_uncommitted_changes());
        store.commit().expect("noop commit");
        assert_eq!(store.generation(), generation);
        let catalog_after = fs::read(flux_dir(&dir).join(CATALOG_FILE_NAME)).expect("catalog");
        assert_eq!(catalog_before, catalog_after, "nothing republished");
    }

    #[test]
    fn uncommitted_changes_are_not_visible_after_reopen() {
        let dir = TempDir::new("portable_uncommitted");
        let mut store = seeded(&dir, 2);
        store
            .add_track(
                rel("A/Uncommitted.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add");
        assert!(store.has_uncommitted_changes());
        drop(store); // "crash": no commit

        let store = PortableStore::open(dir.path()).expect("reopen").store;
        assert_eq!(store.len(), 2);
        assert_eq!(store.id_for_path(&rel("A/Uncommitted.flac")), None);
    }
    // ---------------------------------------------------- loader rejection ---

    /// Seeds a store, corrupts it on disk via `mutate`, then runs the
    /// (private) single-generation loader directly so the exact typed
    /// error can be asserted (the public `open` ladder composes these).
    fn load_err_after(
        name: &str,
        tracks: usize,
        mutate: impl FnOnce(&TempDir),
    ) -> PortableStoreError {
        let dir = TempDir::new(name);
        let store = seeded(&dir, tracks);
        drop(store);
        mutate(&dir);
        load_generation(
            &flux_dir(&dir),
            CATALOG_FILE_NAME,
            PathCasePolicy::Sensitive,
        )
        .expect_err("corrupted store must be rejected")
    }

    fn mutate_catalog(dir: &TempDir, f: impl FnOnce(&mut serde_json::Value)) {
        let path = flux_dir(dir).join(CATALOG_FILE_NAME);
        let mut value = read_json(&path);
        f(&mut value);
        write_json(&path, &value);
    }

    fn mutate_first_segment(dir: &TempDir, f: impl FnOnce(&mut serde_json::Value)) {
        let name = segment_files(dir)
            .into_iter()
            .next()
            .expect("a segment file");
        let path = flux_dir(dir).join(name);
        let mut value = read_json(&path);
        f(&mut value);
        write_json(&path, &value);
    }

    #[test]
    fn loader_rejects_duplicate_track_id() {
        let err = load_err_after("portable_bad_dupid", 3, |dir| {
            mutate_first_segment(dir, |seg| {
                seg["records"][1]["id"] = seg["records"][0]["id"].clone();
            });
        });
        assert!(
            matches!(err, PortableStoreError::DuplicateTrackId { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn loader_rejects_duplicate_path_key() {
        let err = load_err_after("portable_bad_duppath", 3, |dir| {
            mutate_first_segment(dir, |seg| {
                seg["records"][1]["path"] = seg["records"][0]["path"].clone();
            });
        });
        assert!(
            matches!(err, PortableStoreError::DuplicatePath { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn loader_path_keys_respect_case_policy() {
        let dir = TempDir::new("portable_bad_duppath_ci");
        let store = seeded(&dir, 2);
        drop(store);
        mutate_first_segment(&dir, |seg| {
            // Record 2 gets a case-variant spelling of record 1's path.
            let first = seg["records"][0]["path"]["utf8"]
                .as_str()
                .expect("utf8 path")
                .to_uppercase();
            seg["records"][1]["path"] = serde_json::json!({ "utf8": first });
        });
        // Case-sensitive: distinct paths, loads fine.
        load_generation(
            &flux_dir(&dir),
            CATALOG_FILE_NAME,
            PathCasePolicy::Sensitive,
        )
        .expect("sensitive load");
        // Case-insensitive: same key as record 1 → duplicate.
        let err = load_generation(
            &flux_dir(&dir),
            CATALOG_FILE_NAME,
            PathCasePolicy::Insensitive,
        )
        .expect_err("insensitive load must reject");
        assert!(
            matches!(err, PortableStoreError::DuplicatePath { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn loader_rejects_duplicate_segment_id() {
        let err = load_err_after("portable_bad_dupseg", 1500, |dir| {
            mutate_catalog(dir, |cat| {
                cat["segments"][1]["segment_id"] = cat["segments"][0]["segment_id"].clone();
            });
        });
        assert!(
            matches!(err, PortableStoreError::DuplicateSegmentId { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn loader_rejects_invalid_counters() {
        let err = load_err_after("portable_bad_next_track", 3, |dir| {
            mutate_catalog(dir, |cat| cat["next_track_id"] = serde_json::json!(2));
        });
        assert!(
            matches!(err, PortableStoreError::InvalidCounter { .. }),
            "got: {err}"
        );

        let err = load_err_after("portable_bad_next_segment", 3, |dir| {
            mutate_catalog(dir, |cat| cat["next_segment_id"] = serde_json::json!(1));
        });
        assert!(
            matches!(err, PortableStoreError::InvalidCounter { .. }),
            "got: {err}"
        );

        let err = load_err_after("portable_bad_zero_counter", 3, |dir| {
            mutate_catalog(dir, |cat| cat["next_track_id"] = serde_json::json!(0));
        });
        assert!(
            matches!(err, PortableStoreError::InvalidCounter { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn loader_rejects_segment_mismatches() {
        let err = load_err_after("portable_bad_count", 3, |dir| {
            mutate_catalog(dir, |cat| {
                cat["segments"][0]["record_count"] = serde_json::json!(99)
            });
        });
        assert!(
            matches!(err, PortableStoreError::SegmentMismatch { .. }),
            "count: got: {err}"
        );

        let err = load_err_after("portable_bad_bounds", 3, |dir| {
            mutate_catalog(dir, |cat| {
                cat["segments"][0]["last_id"] = serde_json::json!(999)
            });
        });
        assert!(
            matches!(err, PortableStoreError::SegmentMismatch { .. }),
            "bounds: got: {err}"
        );

        let err = load_err_after("portable_bad_seggen", 3, |dir| {
            mutate_first_segment(dir, |seg| seg["generation"] = serde_json::json!(99));
        });
        assert!(
            matches!(err, PortableStoreError::SegmentMismatch { .. }),
            "generation: got: {err}"
        );

        // A crafted descriptor must not be able to point outside .flux:
        let err = load_err_after("portable_bad_segfile", 3, |dir| {
            mutate_catalog(dir, |cat| {
                cat["segments"][0]["file"] = serde_json::json!("../evil.flux");
            });
        });
        assert!(
            matches!(err, PortableStoreError::SegmentMismatch { .. }),
            "traversal name: got: {err}"
        );
    }

    #[test]
    fn loader_rejects_missing_segment_file() {
        let err = load_err_after("portable_bad_missingseg", 3, |dir| {
            let name = segment_files(dir).into_iter().next().expect("segment");
            fs::remove_file(flux_dir(dir).join(name)).expect("delete segment");
        });
        assert!(
            matches!(err, PortableStoreError::MissingSegment { .. }),
            "got: {err}"
        );
    }

    #[test]
    fn loader_rejects_version_kind_and_garbage() {
        let err = load_err_after("portable_bad_version", 3, |dir| {
            mutate_catalog(dir, |cat| cat["schema_version"] = serde_json::json!(99));
        });
        assert!(
            matches!(err, PortableStoreError::UnsupportedVersion { .. }),
            "got: {err}"
        );

        let err = load_err_after("portable_bad_segversion", 3, |dir| {
            mutate_first_segment(dir, |seg| seg["schema_version"] = serde_json::json!(99));
        });
        assert!(
            matches!(err, PortableStoreError::UnsupportedVersion { .. }),
            "got: {err}"
        );

        let err = load_err_after("portable_bad_kind", 3, |dir| {
            mutate_catalog(dir, |cat| cat["kind"] = serde_json::json!("flux-tracks"));
        });
        assert!(
            matches!(err, PortableStoreError::WrongKind { .. }),
            "got: {err}"
        );

        let err = load_err_after("portable_bad_libid", 3, |dir| {
            mutate_catalog(dir, |cat| cat["library_id"] = serde_json::json!("xyz"));
        });
        assert!(
            matches!(err, PortableStoreError::InvalidLibraryId { .. }),
            "got: {err}"
        );

        let err = load_err_after("portable_bad_json", 3, |dir| {
            fs::write(flux_dir(dir).join(CATALOG_FILE_NAME), b"{not json").expect("garbage");
        });
        assert!(
            matches!(err, PortableStoreError::Malformed { .. }),
            "got: {err}"
        );
    }

    // ------------------------------------------------------------- paths ---

    #[test]
    fn relpath_validation_matrix() {
        for ok in ["a.flac", "A/B/C.flac", "a b/ç - é.flac", "A/01 - X.flac"] {
            assert!(RelPath::from_utf8(ok).is_ok(), "should accept {ok:?}");
        }
        let cases: Vec<(&str, PathValidationError)> = vec![
            ("", PathValidationError::Empty),
            ("/abs.flac", PathValidationError::Absolute),
            ("a/../b", PathValidationError::ParentTraversal),
            ("a//b", PathValidationError::EmptyComponent),
            ("a/", PathValidationError::EmptyComponent),
            ("./a", PathValidationError::CurrentDirComponent),
            ("a/./b", PathValidationError::CurrentDirComponent),
            ("C:/x", PathValidationError::ForbiddenCharacter(':')),
            ("a\\b", PathValidationError::ForbiddenCharacter('\\')),
            ("a\u{1}b", PathValidationError::ForbiddenCharacter('\u{1}')),
        ];
        for (input, want) in cases {
            assert_eq!(RelPath::from_utf8(input), Err(want), "from_utf8 {input:?}");
        }

        assert!(RelPath::from_unix_bytes(b"a/b.flac".to_vec()).is_ok());
        assert!(RelPath::from_unix_bytes(vec![0xFF, b'.', b'f']).is_ok());
        assert_eq!(
            RelPath::from_unix_bytes(Vec::new()),
            Err(PathValidationError::Empty)
        );
        assert_eq!(
            RelPath::from_unix_bytes(b"/x".to_vec()),
            Err(PathValidationError::Absolute)
        );
        assert_eq!(
            RelPath::from_unix_bytes(b"a/../b".to_vec()),
            Err(PathValidationError::ParentTraversal)
        );
        assert_eq!(
            RelPath::from_unix_bytes(b"a\0b".to_vec()),
            Err(PathValidationError::NulByte)
        );
        assert_eq!(
            RelPath::from_unix_bytes(b"a//b".to_vec()),
            Err(PathValidationError::EmptyComponent)
        );

        assert_eq!(
            RelPath::from_os_path(Path::new("A/B.flac")),
            Ok(RelPath::Utf8("A/B.flac".to_owned()))
        );
        assert_eq!(
            RelPath::from_os_path(Path::new("./a.flac")),
            Ok(RelPath::Utf8("a.flac".to_owned()))
        );
        assert_eq!(
            RelPath::from_os_path(Path::new("")),
            Err(PathValidationError::Empty)
        );
        assert_eq!(
            RelPath::from_os_path(Path::new("../x")),
            Err(PathValidationError::ParentTraversal)
        );
        #[cfg(unix)]
        assert_eq!(
            RelPath::from_os_path(Path::new("/abs/x")),
            Err(PathValidationError::Absolute)
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_non_utf8_paths_round_trip_byte_exact() {
        use std::os::unix::ffi::OsStringExt;

        let dir = TempDir::new("portable_unix_bytes");
        // "A/\xFF\xFE.flac" — not valid UTF-8.
        let raw: Vec<u8> = vec![b'A', b'/', 0xFF, 0xFE, b'.', b'f', b'l', b'a', b'c'];
        let os_path = PathBuf::from(std::ffi::OsString::from_vec(raw.clone()));
        let path = RelPath::from_os_path(&os_path).expect("from_os_path");
        assert_eq!(path, RelPath::UnixBytes(raw.clone()));

        let mut store = PortableStore::create(dir.path()).expect("create");
        let id = store
            .add_track(path.clone(), Availability::Present, None, TagSet::default())
            .expect("add");
        store.commit().expect("commit");

        // The serialized form holds the exact byte array.
        let segment = segment_files(&dir).into_iter().next().expect("segment");
        let value = read_json(&flux_dir(&dir).join(segment));
        assert_eq!(
            value["records"][0]["path"]["unix_bytes"],
            serde_json::json!([65, 47, 255, 254, 46, 102, 108, 97, 99])
        );
        drop(store);

        let store = PortableStore::open(dir.path()).expect("reopen").store;
        let record = store.get(id).expect("record");
        assert_eq!(record.path(), &path, "byte-exact after reload");
        assert_eq!(record.path().to_os_path(), Some(os_path));
        assert!(record.path().is_representable_on_this_platform());
    }

    #[test]
    fn opaque_unix_byte_paths_survive_unrelated_edits() {
        // Raw-byte paths must be carried through the DTO layer untouched
        // (never via PathBuf/String), so editing other records — or this
        // record's metadata — cannot corrupt them. This exercises the
        // same code path a non-Unix platform would use, where the path
        // is additionally unrepresentable.
        let dir = TempDir::new("portable_opaque");
        let opaque = RelPath::from_unix_bytes(vec![0x80, b'/', 0x81, b'.', b'f']).expect("bytes");
        let mut store = PortableStore::create(dir.path()).expect("create");
        let opaque_id = store
            .add_track(
                opaque.clone(),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add opaque");
        let normal_id = store
            .add_track(
                rel("A/B.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add normal");
        store.commit().expect("commit");

        store
            .set_tags(normal_id, title_tags("Edited"))
            .expect("edit other");
        let mut overrides = MetadataOverrides::new();
        overrides.set_title("Opaque Title".to_owned());
        store
            .set_overrides(opaque_id, overrides)
            .expect("metadata of opaque records stays editable");
        store.commit().expect("commit edits");
        drop(store);

        let store = PortableStore::open(dir.path()).expect("reopen").store;
        assert_eq!(store.get(opaque_id).expect("opaque").path(), &opaque);
        assert_eq!(
            store
                .get(opaque_id)
                .expect("opaque")
                .effective_metadata()
                .title,
            "Opaque Title"
        );
        assert_eq!(
            store
                .get(normal_id)
                .expect("normal")
                .tags()
                .title
                .as_deref(),
            Some("Edited")
        );
    }

    // ---------------------------------------------------------- overrides ---

    #[test]
    fn overrides_tri_state_survives_round_trip() {
        let dir = TempDir::new("portable_overrides");
        let mut store = PortableStore::create(dir.path()).expect("create");
        let mut tags = title_tags("Tag Title");
        tags.artist = Some("Tag Artist".to_owned());
        tags.album = Some("Tag Album".to_owned());
        let id = store
            .add_track(rel("A/B/Song.flac"), Availability::Present, None, tags)
            .expect("add");
        let other = store
            .add_track(
                rel("A/B/Other.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add other");

        let mut overrides = MetadataOverrides::new();
        overrides.set_title("My Title".to_owned()); // present value
        overrides.clear_artist(); // present null
        // album: absent
        store.set_overrides(id, overrides).expect("set overrides");
        store.commit().expect("commit");

        // Serialized shape: set → value, cleared → explicit null,
        // absent → no key at all.
        let segment = segment_files(&dir).into_iter().next().expect("segment");
        let value = read_json(&flux_dir(&dir).join(segment));
        let serialized = &value["records"][0]["overrides"];
        assert_eq!(serialized["title"], serde_json::json!("My Title"));
        assert_eq!(serialized["artist"], serde_json::Value::Null);
        assert!(
            serialized.get("album").is_none(),
            "absent override must have no key: {serialized}"
        );

        // An unrelated edit + recommit must not collapse the states.
        store
            .set_tags(other, title_tags("Unrelated"))
            .expect("edit other");
        store.commit().expect("commit again");
        drop(store);

        let store = PortableStore::open(dir.path()).expect("reopen").store;
        let record = store.get(id).expect("record");
        assert_eq!(record.overrides().title(), Some(Some("My Title")));
        assert_eq!(record.overrides().artist(), Some(None), "still cleared");
        assert_eq!(record.overrides().album(), None, "still absent");
        let effective = record.effective_metadata();
        assert_eq!(effective.title, "My Title");
        assert_eq!(effective.artist, None, "clear beats the embedded tag");
        assert_eq!(effective.album.as_deref(), Some("Tag Album"));
    }

    #[test]
    fn effective_metadata_resolution_matrix() {
        let dir = TempDir::new("portable_effective");
        let mut store = PortableStore::create(dir.path()).expect("create");

        // No tags at all: title from filename, artist/album from folders.
        let bare = store
            .add_track(
                rel("The Artist/The Album/07 - Hello.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add bare");
        let eff = store.get(bare).expect("bare").effective_metadata();
        assert_eq!(eff.title, "07 - Hello");
        assert_eq!(eff.artist.as_deref(), Some("The Artist"));
        assert_eq!(eff.album.as_deref(), Some("The Album"));

        // Cleared album suppresses the folder fallback; cleared title
        // suppresses the tag but keeps the guaranteed filename title.
        let mut tags = title_tags("Tagged");
        tags.track_number = Some(3);
        let cleared = store
            .add_track(rel("X/Y/09 - Stem.flac"), Availability::Present, None, tags)
            .expect("add cleared");
        let mut overrides = MetadataOverrides::new();
        overrides.clear_album();
        overrides.clear_title();
        overrides.set_track_number(9);
        store.set_overrides(cleared, overrides).expect("overrides");
        let eff = store.get(cleared).expect("cleared").effective_metadata();
        assert_eq!(
            eff.title, "09 - Stem",
            "cleared title falls back to filename, not the tag"
        );
        assert_eq!(eff.album, None);
        assert_eq!(eff.track_number, Some(9), "set beats tag");

        // Shallow path: no folder fallback available.
        let shallow = store
            .add_track(
                rel("song.flac"),
                Availability::Present,
                None,
                TagSet::default(),
            )
            .expect("add shallow");
        let eff = store.get(shallow).expect("shallow").effective_metadata();
        assert_eq!(eff.title, "song");
        assert_eq!(eff.artist, None);
        assert_eq!(eff.album, None);
    }
    // --------------------------------------------------------- publication ---

    #[test]
    fn orphan_segment_files_are_ignored_on_open() {
        let dir = TempDir::new("portable_orphans");
        let store = seeded(&dir, 3);
        drop(store);
        // A plausible orphaned segment (finalized before a crash, never
        // referenced) and outright garbage under a segment name.
        let real = segment_files(&dir).into_iter().next().expect("segment");
        let bytes = fs::read(flux_dir(&dir).join(&real)).expect("read segment");
        fs::write(flux_dir(&dir).join("tracks-0999@1.flux"), &bytes).expect("plant orphan copy");
        fs::write(flux_dir(&dir).join("tracks-0555@3.flux"), b"garbage").expect("plant garbage");

        let store = PortableStore::open(dir.path())
            .expect("open ignores orphans")
            .store;
        assert_eq!(store.len(), 3, "orphan records are not adopted");
    }

    #[test]
    fn finalize_never_overwrites_and_is_idempotent_for_identical_bytes() {
        let dir = TempDir::new("portable_finalize");
        let flux = flux_dir(&dir);
        fs::create_dir_all(&flux).expect("mk .flux");
        let temp = flux.join("tracks-0001@1.flux.tmp");
        let target = flux.join("tracks-0001@1.flux");

        // Different pre-existing bytes: hard error, target untouched.
        fs::write(&target, b"committed").expect("plant target");
        write_synced(&temp, b"impostor").expect("write temp");
        let err = finalize_no_overwrite(&flux, &temp, "tracks-0001@1.flux", b"impostor")
            .expect_err("must refuse to overwrite");
        assert!(
            matches!(err, PortableStoreError::SegmentFileExists { .. }),
            "got: {err}"
        );
        assert_eq!(fs::read(&target).expect("target"), b"committed");

        // Identical bytes (a commit retried after crashing between
        // finalize and catalogue publish): success, temp cleaned up.
        write_synced(&temp, b"committed").expect("rewrite temp");
        finalize_no_overwrite(&flux, &temp, "tracks-0001@1.flux", b"committed")
            .expect("idempotent finalize");
        assert!(!temp.exists());
        assert_eq!(fs::read(&target).expect("target"), b"committed");
    }

    #[test]
    fn commit_skips_taken_segment_names_instead_of_overwriting() {
        // Regression test for the recovery deadlock: a segment file
        // left behind by a lost catalogue generation occupies the next
        // generation name. Publication must step forward to a free
        // generation — never overwrite the squatter, never wedge.
        let dir = TempDir::new("portable_skip_forward");
        let mut store = seeded(&dir, 1); // segment 1 @ generation 1
        let squatter = flux_dir(&dir).join("tracks-0001@2.flux");
        fs::write(&squatter, b"orphan from a lost generation").expect("plant squatter");

        store
            .set_tags(TrackId::new(1), title_tags("New"))
            .expect("edit");
        store.commit().expect("commit steps past the taken name");
        assert!(
            flux_dir(&dir).join("tracks-0001@3.flux").exists(),
            "generation 3 allocated past the taken name"
        );
        assert!(
            !squatter.exists(),
            "the unreferenced squatter is garbage-collected after publication"
        );
        drop(store);

        let store = PortableStore::open(dir.path()).expect("reopen").store;
        assert_eq!(
            store
                .get(TrackId::new(1))
                .expect("track")
                .tags()
                .title
                .as_deref(),
            Some("New")
        );
    }

    #[test]
    fn gc_removes_only_unreferenced_internal_files() {
        let dir = TempDir::new("portable_gc");
        let mut store = seeded(&dir, 1); // catalogue gen2 → tracks-0001@1
        let f1 = flux_dir(&dir).join("tracks-0001@1.flux");
        let f2 = flux_dir(&dir).join("tracks-0001@2.flux");
        let f3 = flux_dir(&dir).join("tracks-0001@3.flux");
        assert!(f1.exists());

        store
            .set_tags(TrackId::new(1), title_tags("v2"))
            .expect("edit");
        store.commit().expect("commit gen3"); // → @2; backup (gen2) still references @1
        assert!(f1.exists() && f2.exists(), "backup generation protected");

        store
            .set_tags(TrackId::new(1), title_tags("v3"))
            .expect("edit");
        store.commit().expect("commit gen4"); // → @3; backup (gen3) references @2
        assert!(!f1.exists(), "generation 1 now unreferenced and collected");
        assert!(f2.exists() && f3.exists());

        // Orphaned segment-named files and our own temp leftovers are
        // collected; unrelated files are never touched.
        fs::write(flux_dir(&dir).join("tracks-0777@9.flux"), b"orphan").expect("orphan");
        fs::write(flux_dir(&dir).join("catalog.flux.tmp"), b"temp").expect("temp");
        fs::write(flux_dir(&dir).join("notes.txt"), b"keep me").expect("notes");
        store
            .set_tags(TrackId::new(1), title_tags("v4"))
            .expect("edit");
        store.commit().expect("commit gen5");
        assert!(!flux_dir(&dir).join("tracks-0777@9.flux").exists());
        assert!(!flux_dir(&dir).join("catalog.flux.tmp").exists());
        assert!(
            flux_dir(&dir).join("notes.txt").exists(),
            "user file preserved"
        );
    }

    // ------------------------------------------------------------ recovery ---

    /// Two data commits: primary holds 4 tracks (catalogue gen 3),
    /// backup holds the first 3 (catalogue gen 2).
    fn two_generation_store(dir: &TempDir) {
        let mut store = seeded(dir, 3);
        store
            .add_track(
                rel("Artist/Album/extra.flac"),
                Availability::Present,
                None,
                title_tags("Extra"),
            )
            .expect("add extra");
        store.commit().expect("second commit");
    }

    #[test]
    fn recovery_from_backup_catalogue() {
        let dir = TempDir::new("portable_recovery");
        two_generation_store(&dir);
        fs::write(flux_dir(&dir).join(CATALOG_FILE_NAME), b"corrupted!").expect("corrupt");

        let opened = PortableStore::open(dir.path()).expect("recovered open");
        let recovery = opened.recovery.expect("recovery reported");
        assert_eq!(recovery.recovered_generation, 2);
        assert!(!recovery.primary_error.is_empty());
        let mut store = opened.store;
        assert_eq!(store.len(), 3, "exactly the backup generation, no mixing");
        assert!(store.is_recovered_read_only());
        assert!(matches!(
            store.add_track(
                rel("A/X.flac"),
                Availability::Present,
                None,
                TagSet::default()
            ),
            Err(PortableStoreError::ReadOnlyRecovered)
        ));
        assert!(matches!(
            store.commit(),
            Err(PortableStoreError::ReadOnlyRecovered)
        ));

        store.acknowledge_recovery().expect("acknowledge");
        assert!(!store.is_recovered_read_only());
        // Recovery rolled back the lost generation wholesale, including
        // its ID allocation: the next ID is the backup's counter value.
        // The re-added record deliberately differs from the lost
        // generation's ("Extra") so the new segment bytes can never
        // accidentally match the orphaned tracks-0001@2.flux — the
        // commit must allocate generation 3 rather than collide
        // (this is the exact intermittency this test used to flake on
        // when millisecond timestamps made the bytes coincide).
        let id = store
            .add_track(
                rel("Artist/Album/extra.flac"),
                Availability::Present,
                None,
                title_tags("Extra Once More"),
            )
            .expect("add after acknowledge");
        assert_eq!(id, TrackId::new(4));
        store.commit().expect("commit after acknowledge");
        assert!(
            flux_dir(&dir).join("tracks-0001@3.flux").exists(),
            "segment generation stepped past the lost generation's file"
        );
        drop(store);

        let opened = PortableStore::open(dir.path()).expect("normal reopen");
        assert!(opened.recovery.is_none());
        assert_eq!(opened.store.len(), 4);
    }

    #[test]
    fn missing_primary_recovers_from_backup() {
        let dir = TempDir::new("portable_recovery_missing");
        two_generation_store(&dir);
        fs::remove_file(flux_dir(&dir).join(CATALOG_FILE_NAME)).expect("delete primary");

        let opened = PortableStore::open(dir.path()).expect("recovered open");
        assert!(opened.recovery.is_some());
        assert_eq!(opened.store.len(), 3);
        assert!(opened.store.is_recovered_read_only());
    }

    #[test]
    fn both_catalogues_invalid_is_a_typed_error_and_writes_nothing() {
        let dir = TempDir::new("portable_both_bad");
        two_generation_store(&dir);
        fs::write(flux_dir(&dir).join(CATALOG_FILE_NAME), b"bad primary").expect("corrupt p");
        fs::write(flux_dir(&dir).join(CATALOG_BACKUP_FILE_NAME), b"bad backup").expect("corrupt b");

        let err = PortableStore::open(dir.path()).expect_err("must refuse");
        assert!(
            matches!(
                err,
                PortableStoreError::BothCataloguesInvalid {
                    backup: Some(_),
                    ..
                }
            ),
            "got: {err}"
        );
        // Refusing must not "repair" anything behind the caller's back.
        assert_eq!(
            fs::read(flux_dir(&dir).join(CATALOG_FILE_NAME)).expect("primary bytes"),
            b"bad primary"
        );
    }

    #[test]
    fn corrupt_primary_without_backup_is_a_typed_error() {
        let dir = TempDir::new("portable_no_backup");
        let store = PortableStore::create(dir.path()).expect("create");
        drop(store);
        assert!(
            !flux_dir(&dir).join(CATALOG_BACKUP_FILE_NAME).exists(),
            "no backup exists before the first republish"
        );
        fs::write(flux_dir(&dir).join(CATALOG_FILE_NAME), b"bad").expect("corrupt");

        let err = PortableStore::open(dir.path()).expect_err("must refuse");
        assert!(
            matches!(
                err,
                PortableStoreError::BothCataloguesInvalid { backup: None, .. }
            ),
            "got: {err}"
        );
    }

    // ------------------------------------------- segmentation regressions ---

    #[test]
    fn fat_records_uneven_split_removal_and_repack_keep_invariants() {
        // Regression for the original S2 failures: records sized so
        // the byte cap binds before the record cap split unevenly
        // (854/646-shaped, not 1000/500). Removing the entire first
        // segment's range and then repacking must keep every invariant
        // without depending on where exactly the split landed.
        let dir = TempDir::new("portable_fat_repack");
        let mut store = PortableStore::create(dir.path()).expect("create");
        for i in 0..1500u64 {
            store
                .add_track(
                    rel(&format!("Artist/Album/{i:04} - Song {i}.flac")),
                    Availability::Present,
                    Some(FileFingerprint {
                        size_bytes: 1000 + i,
                        modified_unix_ms: 42,
                    }),
                    title_tags(&format!("Song {i}")),
                )
                .expect("add");
        }
        store.commit().expect("commit");
        assert!(store.segment_count() >= 2, "byte cap split the fixture");
        for name in segment_files(&dir) {
            let len = fs::metadata(flux_dir(&dir).join(&name))
                .expect("meta")
                .len();
            assert!(
                usize::try_from(len).unwrap_or(usize::MAX) <= MAX_SEGMENT_SERIALIZED_BYTES,
                "segment {name} is {len} bytes, over the cap"
            );
        }

        // Remove every record of the first segment, plus a few more:
        // the first segment becomes empty in memory.
        let first_segment = store.segment_of(TrackId::new(1)).expect("segment of 1");
        let first_count = u64::try_from(
            store
                .iter()
                .filter(|t| store.segment_of(t.id()) == Some(first_segment))
                .count(),
        )
        .expect("small count");
        assert!(first_count < 1000, "byte cap bound before the record cap");
        let cutoff = first_count + 10;
        for raw in 1..=cutoff {
            assert!(store.remove_track(TrackId::new(raw)).expect("remove"));
        }
        store.commit().expect("commit removals");

        // The published catalogue contains no empty segment, ever.
        let catalog = catalog_value(&dir);
        for descriptor in catalog["segments"].as_array().expect("descriptors") {
            assert!(
                descriptor["record_count"].as_u64().expect("count") >= 1,
                "no empty segment may be published"
            );
        }
        let expected: Vec<u64> = (cutoff + 1..=1500).collect();
        assert_eq!(
            store.iter().map(|t| t.id().get()).collect::<Vec<_>>(),
            expected
        );

        store.repack().expect("repack");
        store.commit().expect("commit repack");
        drop(store);

        let store = PortableStore::open(dir.path()).expect("reopen").store;
        assert_eq!(
            store.iter().map(|t| t.id().get()).collect::<Vec<_>>(),
            expected,
            "every remaining record exactly once, order and IDs preserved"
        );
        assert_eq!(store.len(), expected.len());
    }

    #[test]
    fn removing_everything_publishes_an_empty_store() {
        // Both shapes must publish the emptied catalogue: plain
        // removals (dirty segments carry the signal) and removal
        // followed by a repack (no segment remains to be dirty — the
        // structural-change flag has to carry it).
        for (name, use_repack) in [
            ("portable_empty_plain", false),
            ("portable_empty_repack", true),
        ] {
            let dir = TempDir::new(name);
            let mut store = seeded(&dir, 5);
            let generation_before = store.generation();
            for raw in 1..=5 {
                assert!(store.remove_track(TrackId::new(raw)).expect("remove"));
            }
            if use_repack {
                store.repack().expect("repack");
            }
            assert!(
                store.has_uncommitted_changes(),
                "emptying must remain publishable (repack={use_repack})"
            );
            store.commit().expect("commit");
            assert!(store.generation() > generation_before);
            drop(store);

            let mut store = PortableStore::open(dir.path()).expect("reopen").store;
            assert_eq!(
                store.len(),
                0,
                "emptied store stays empty (repack={use_repack})"
            );
            let catalog = catalog_value(&dir);
            assert_eq!(catalog["segments"].as_array().expect("segments").len(), 0);
            // The ID counter never rewinds, even across total removal.
            let fresh = store
                .add_track(
                    rel("again.flac"),
                    Availability::Present,
                    None,
                    TagSet::default(),
                )
                .expect("add after emptying");
            assert_eq!(fresh, TrackId::new(6));
        }
    }
}
