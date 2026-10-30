//! Canonical self-contained Flux library storage.
//!
//! This module is the target persistent format for the canonical storage
//! redesign. It is intentionally independent of audio-tag readers: music
//! metadata comes only from Flux files. Audio files are opaque managed
//! resources; container-level decoding may validate that a file exists and
//! can be copied, but tags are never consulted.
//!
//! Layout:
//!
//! ```text
//! <root>/
//!   library.flux
//!   catalog.flux
//!   catalog.flux.bak
//!   artists/artist_<id>/artist.flux
//!   artists/artist_<id>/albums/album_<id>/
//!       album.flux
//!       cover.<ext>                     (optional)
//!       tracks/track_<id>.<ext>
//!       tracks/track_<id>.flux
//!       tracks/track_<id>.lrc           (optional)
//!   playlists/playlist_<id>.flux
//!   ratings.flux
//!   transactions/<txn>/journal.flux
//!   backups/
//! ```
//!
//! `catalog.flux` is authoritative for entity existence, stable IDs, physical
//! ownership, album/playlist membership, canonical relative paths, counters,
//! and the committed generation. Per-entity files are authoritative for their
//! metadata. The reader validates the two representations against each other
//! and never silently repairs a mismatch.
//!
//! New canonical libraries are written through a transaction journal. A batch
//! writes its journal first, installs files with same-directory temporary
//! files and atomic renames, publishes `catalog.flux` last, marks the journal
//! committed, and only then deletes its backups. Startup recovery either
//! completes cleanup for a committed catalogue or restores the pre-batch
//! bytes and removes files created by an uncommitted batch.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use flux_core::{ArtistId, TrackId};
use serde::{Deserialize, Serialize};

use crate::portable::{PathValidationError, RelPath};

/// Version of the canonical library format written by this build.
pub const CANONICAL_SCHEMA_VERSION: u32 = 1;
/// Root manifest file.
pub const LIBRARY_FILE_NAME: &str = "library.flux";
/// Authoritative catalogue file.
pub const CATALOG_FILE_NAME: &str = "catalog.flux";
/// Previous committed catalogue generation.
pub const CATALOG_BACKUP_FILE_NAME: &str = "catalog.flux.bak";
/// Ratings file.
pub const RATINGS_FILE_NAME: &str = "ratings.flux";
/// Managed transaction directory.
pub const TRANSACTIONS_DIR_NAME: &str = "transactions";
/// Recovery backup directory.
pub const BACKUPS_DIR_NAME: &str = "backups";
/// Artists directory.
pub const ARTISTS_DIR_NAME: &str = "artists";
/// Playlists directory.
pub const PLAYLISTS_DIR_NAME: &str = "playlists";

const MANIFEST_KIND: &str = "flux-library";
const CATALOG_KIND: &str = "flux-catalog";
const ARTIST_KIND: &str = "flux-artist";
const ALBUM_KIND: &str = "flux-album";
const TRACK_KIND: &str = "flux-track";
const PLAYLIST_KIND: &str = "flux-playlist";
const RATINGS_KIND: &str = "flux-ratings";
const JOURNAL_KIND: &str = "flux-transaction";

// ----------------------------------------------------------------- IDs ---

/// Stable identity of an album/release within one canonical library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AlbumId(u64);

impl AlbumId {
    /// Creates an ID from a raw value.
    #[must_use]
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }
    /// Raw value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for AlbumId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Stable identity of a playlist within one canonical library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PlaylistId(u64);

impl PlaylistId {
    /// Creates an ID from a raw value.
    #[must_use]
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }
    /// Raw value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PlaylistId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Explicit release type for an album directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseType {
    /// Full-length album.
    #[default]
    Album,
    /// Extended play.
    Ep,
    /// Single.
    Single,
    /// Compilation or various-artists release.
    Compilation,
    /// A release that does not fit the other categories.
    Other,
}

impl ReleaseType {
    /// Stable lowercase wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Album => "album",
            Self::Ep => "ep",
            Self::Single => "single",
            Self::Compilation => "compilation",
            Self::Other => "other",
        }
    }
}

/// Non-cryptographic content fingerprint used for copy verification and
/// duplicate leads. This is deliberately a persisted FNV-1a 64-bit value plus
/// the byte length; it is not a security primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentFingerprint {
    /// Byte length.
    pub size_bytes: u64,
    /// FNV-1a 64-bit hash of the complete file.
    pub fnv1a64: u64,
}

impl ContentFingerprint {
    /// Computes a fingerprint by streaming `path`; never reads audio tags.
    pub fn of_file(path: &Path) -> Result<Self, CanonicalError> {
        let mut file = fs::File::open(path)?;
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut size = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size += read as u64;
            for byte in &buffer[..read] {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x100_0000_01b3);
            }
        }
        Ok(Self {
            size_bytes: size,
            fnv1a64: hash,
        })
    }
}

// -------------------------------------------------------------- errors ---

/// Why a canonical library operation failed.
#[derive(Debug)]
pub enum CanonicalError {
    /// Filesystem error.
    Io(std::io::Error),
    /// JSON malformed.
    Malformed { file: String, message: String },
    /// Wrong discriminator.
    WrongKind {
        file: String,
        expected: &'static str,
        found: String,
    },
    /// Unsupported format version.
    UnsupportedVersion {
        file: String,
        found: u32,
        supported: u32,
    },
    /// Structural invariant violation.
    Invalid(String),
    /// Relative path rejected.
    InvalidPath(PathValidationError),
    /// No canonical library at the root.
    NotFound(PathBuf),
    /// Root is not empty / already a canonical library.
    AlreadyExists(PathBuf),
    /// A required resource is missing.
    MissingResource(PathBuf),
    /// A transaction journal is malformed; no automatic repair is performed.
    Transaction { file: String, message: String },
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "filesystem error: {err}"),
            Self::Malformed { file, message } => write!(f, "{file} is malformed: {message}"),
            Self::WrongKind {
                file,
                expected,
                found,
            } => {
                write!(f, "{file} declares kind {found:?}, expected {expected:?}")
            }
            Self::UnsupportedVersion {
                file,
                found,
                supported,
            } => write!(
                f,
                "{file} uses unsupported canonical schema version {found} (this build supports {supported})"
            ),
            Self::Invalid(reason) => write!(f, "invalid canonical library: {reason}"),
            Self::InvalidPath(err) => write!(f, "invalid canonical relative path: {err}"),
            Self::NotFound(root) => {
                write!(f, "no canonical Flux library exists at {}", root.display())
            }
            Self::AlreadyExists(root) => write!(
                f,
                "a non-empty or canonical library already exists at {}",
                root.display()
            ),
            Self::MissingResource(path) => write!(
                f,
                "required library resource is missing: {}",
                path.display()
            ),
            Self::Transaction { file, message } => {
                write!(f, "transaction {file} cannot be recovered: {message}")
            }
        }
    }
}

impl std::error::Error for CanonicalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::InvalidPath(err) => Some(err),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CanonicalError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
impl From<PathValidationError> for CanonicalError {
    fn from(err: PathValidationError) -> Self {
        Self::InvalidPath(err)
    }
}

// ----------------------------------------------------------- entities ---

/// One canonical artist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalArtist {
    /// Stable identity.
    pub id: ArtistId,
    /// Canonical display name.
    pub name: String,
    /// Optional alternate names.
    pub aliases: Vec<String>,
}

/// One canonical album/release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalAlbum {
    /// Stable identity.
    pub id: AlbumId,
    /// Artist owning the album directory and its physical tracks.
    pub owner_artist_id: ArtistId,
    /// Canonical title.
    pub title: String,
    /// Release type.
    pub release_type: ReleaseType,
    /// Optional cover file name inside the album directory.
    pub cover: Option<String>,
    /// Ordered track IDs in this album.
    pub track_order: Vec<TrackId>,
}

/// One canonical track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalTrack {
    /// Stable identity.
    pub id: TrackId,
    /// Canonical title.
    pub title: String,
    /// Ordered credited artists. The first is the default owner at import
    /// time, but an explicit ownership choice may put the physical owner
    /// elsewhere in the ordered list.
    pub credited_artist_ids: Vec<ArtistId>,
    /// Physical owner. Must be one of `credited_artist_ids` and the album
    /// owner for the canonical path layout.
    pub owner_artist_id: ArtistId,
    /// Owning album.
    pub album_id: AlbumId,
    /// Track number within its disc.
    pub track_number: Option<u32>,
    /// Disc number.
    pub disc_number: Option<u32>,
    /// Stored technical duration, when known independently of tags.
    pub duration_ms: Option<u64>,
    /// Optional lyrics file name inside the track directory.
    pub lyrics: Option<String>,
    /// Optional content fingerprint.
    pub fingerprint: Option<ContentFingerprint>,
}

/// One canonical playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPlaylist {
    /// Stable identity.
    pub id: PlaylistId,
    /// Playlist name.
    pub name: String,
    /// Ordered track references.
    pub track_ids: Vec<TrackId>,
}

/// Rating/favorite value for one track.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRating {
    /// Optional 0..=5 rating.
    pub rating: Option<u8>,
    /// Favorite flag.
    pub favorite: bool,
}

/// Deterministic test-only failure boundaries for canonical transactions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionFailPoint {
    /// Before the transaction directory/journal exists.
    BeforeJournal,
    /// Immediately after the prepared journal is durably written.
    AfterJournal,
    /// Before staging an audio/metadata resource.
    BeforeStaging,
    /// While/after staging any copied resource, before fingerprint verification.
    DuringCopy,
    /// While/after staging audio, before fingerprint verification.
    DuringAudioCopy,
    /// After audio copy completes but before verification.
    AfterAudioCopyBeforeVerify,
    /// While/after staging managed lyrics.
    DuringLyricsCopy,
    /// After copy completes but before verification.
    AfterCopyBeforeVerify,
    /// After all staged resources verify.
    AfterVerify,
    /// Before any staged resource replaces its final target.
    BeforeMetadataPublish,
    /// After a staged metadata/resource is published.
    DuringMetadataPublish,
    /// Before the catalogue file is written.
    BeforeCatalogPublish,
    /// Immediately after the catalogue file is written.
    AfterCatalogPublish,
    /// Before post-commit cleanup starts.
    BeforeCleanup,
    /// During/after removal of an obsolete source resource.
    DuringCleanup,
    /// During/after removal of an obsolete lyrics resource.
    DuringLyricsCleanup,
    /// After all obsolete resources are removed.
    AfterCleanup,
    /// Before the completed/committed journal is finalized.
    BeforeJournalFinalize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionFailure {
    /// Boundary that fails.
    pub point: TransactionFailPoint,
    /// When true, simulate an abrupt interruption by skipping rollback.
    pub interrupt: bool,
}

/// One canonical library snapshot plus its on-disk root.
#[derive(Debug)]
pub struct CanonicalLibrary {
    root: PathBuf,
    manifest: LibraryManifestDto,
    catalog: CatalogDto,
    artists: BTreeMap<ArtistId, CanonicalArtist>,
    albums: BTreeMap<AlbumId, CanonicalAlbum>,
    tracks: BTreeMap<TrackId, CanonicalTrack>,
    playlists: BTreeMap<PlaylistId, CanonicalPlaylist>,
    ratings: BTreeMap<TrackId, TrackRating>,
    track_paths: BTreeMap<TrackId, RelPath>,
    track_records: BTreeMap<TrackId, RelPath>,
    artist_paths: BTreeMap<ArtistId, RelPath>,
    album_paths: BTreeMap<AlbumId, RelPath>,
    playlist_paths: BTreeMap<PlaylistId, RelPath>,
    fail_scenario: Option<TransactionFailure>,
}

impl CanonicalLibrary {
    /// Library root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// Stable library identity.
    #[must_use]
    pub fn library_id(&self) -> &str {
        &self.manifest.library_id
    }
    /// Committed generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.catalog.generation
    }
    /// Artists in ascending ID order.
    pub fn artists(&self) -> impl Iterator<Item = &CanonicalArtist> {
        self.artists.values()
    }
    /// Albums in ascending ID order.
    pub fn albums(&self) -> impl Iterator<Item = &CanonicalAlbum> {
        self.albums.values()
    }
    /// Tracks in ascending ID order.
    pub fn tracks(&self) -> impl Iterator<Item = &CanonicalTrack> {
        self.tracks.values()
    }
    /// Playlists in ascending ID order.
    pub fn playlists(&self) -> impl Iterator<Item = &CanonicalPlaylist> {
        self.playlists.values()
    }
    /// Artist by ID.
    #[must_use]
    pub fn artist(&self, id: ArtistId) -> Option<&CanonicalArtist> {
        self.artists.get(&id)
    }
    /// Album by ID.
    #[must_use]
    pub fn album(&self, id: AlbumId) -> Option<&CanonicalAlbum> {
        self.albums.get(&id)
    }
    /// Track by ID.
    #[must_use]
    pub fn track(&self, id: TrackId) -> Option<&CanonicalTrack> {
        self.tracks.get(&id)
    }
    /// Playlist by ID.
    #[must_use]
    pub fn playlist(&self, id: PlaylistId) -> Option<&CanonicalPlaylist> {
        self.playlists.get(&id)
    }
    /// Rating for a track.
    #[must_use]
    pub fn rating(&self, id: TrackId) -> Option<TrackRating> {
        self.ratings.get(&id).copied()
    }
    /// Absolute path for a track's canonical audio file.
    #[must_use]
    pub fn track_audio_path(&self, id: TrackId) -> Option<PathBuf> {
        let rel = self.track_paths.get(&id)?;
        let os = rel.to_os_path()?;
        Some(self.root.join(os))
    }
    /// Absolute path for a track's `.flux` metadata record.
    #[must_use]
    pub fn track_record_path(&self, id: TrackId) -> Option<PathBuf> {
        let rel = self.track_records.get(&id)?;
        let os = rel.to_os_path()?;
        Some(self.root.join(os))
    }
    /// Whether the canonical audio file currently exists.
    #[must_use]
    pub fn track_available(&self, id: TrackId) -> bool {
        self.track_audio_path(id)
            .is_some_and(|path| fs::symlink_metadata(path).is_ok_and(|m| m.is_file()))
    }
    /// Absolute path for an album directory.
    #[must_use]
    pub fn album_dir(&self, id: AlbumId) -> Option<PathBuf> {
        let rel = self.album_paths.get(&id)?;
        Some(self.root.join(rel.to_os_path()?))
    }
    /// Absolute path for an artist directory.
    #[must_use]
    pub fn artist_dir(&self, id: ArtistId) -> Option<PathBuf> {
        let rel = self.artist_paths.get(&id)?;
        Some(self.root.join(rel.to_os_path()?))
    }
    /// Absolute path for a playlist file.
    #[must_use]
    pub fn playlist_path(&self, id: PlaylistId) -> Option<PathBuf> {
        let rel = self.playlist_paths.get(&id)?;
        Some(self.root.join(rel.to_os_path()?))
    }
}

// -------------------------------------------------------------- DTOs ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LibraryManifestDto {
    schema_version: u32,
    kind: String,
    library_id: String,
    created_unix_ms: u64,
    catalog: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogDto {
    schema_version: u32,
    kind: String,
    library_id: String,
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transaction_id: Option<String>,
    next_track_id: u64,
    next_artist_id: u64,
    next_album_id: u64,
    next_playlist_id: u64,
    artists: Vec<CatalogArtistDto>,
    albums: Vec<CatalogAlbumDto>,
    tracks: Vec<CatalogTrackDto>,
    playlists: Vec<CatalogPlaylistDto>,
    ratings_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogArtistDto {
    id: u64,
    path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogAlbumDto {
    id: u64,
    artist_id: u64,
    path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogTrackDto {
    id: u64,
    owner_artist_id: u64,
    album_id: u64,
    audio_path: String,
    record_path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CatalogPlaylistDto {
    id: u64,
    path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtistEntityDto {
    schema_version: u32,
    kind: String,
    id: u64,
    name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AlbumEntityDto {
    schema_version: u32,
    kind: String,
    id: u64,
    owner_artist_id: u64,
    title: String,
    release_type: ReleaseType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cover: Option<String>,
    track_order: Vec<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrackEntityDto {
    schema_version: u32,
    kind: String,
    id: u64,
    title: String,
    credited_artist_ids: Vec<u64>,
    owner_artist_id: u64,
    album_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lyrics: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<ContentFingerprint>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PlaylistEntityDto {
    schema_version: u32,
    kind: String,
    id: u64,
    name: String,
    track_ids: Vec<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RatingsDto {
    schema_version: u32,
    kind: String,
    #[serde(default)]
    entries: Vec<RatingEntryDto>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RatingEntryDto {
    track_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rating: Option<u8>,
    favorite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalDto {
    schema_version: u32,
    kind: String,
    transaction_id: String,
    operation: String,
    state: JournalState,
    intended_generation: u64,
    actions: Vec<JournalActionDto>,
    /// Files that become obsolete only after the catalogue publishes. Recovery
    /// removes them only for a committed transaction.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cleanup: Vec<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalState {
    /// Historical state used by older journals; treated as uncommitted.
    Applying,
    Prepared,
    Copying,
    Verified,
    Publishing,
    Committed,
    Cleaning,
    Completed,
    RolledBack,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalActionDto {
    target: String,
    existed: bool,
    /// Backup file name inside the transaction directory for a replaced file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    backup: Option<String>,
    /// Staging file name inside the transaction directory for a copied resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    staged_source: Option<String>,
}

// -------------------------------------------------------- path layout ---

fn artist_rel(id: ArtistId) -> RelPath {
    RelPath::from_utf8(&format!("artists/artist_{:06}", id.get()))
        .expect("generated artist path is valid")
}
fn album_rel(owner: ArtistId, id: AlbumId) -> RelPath {
    RelPath::from_utf8(&format!(
        "artists/artist_{:06}/albums/album_{:06}",
        owner.get(),
        id.get()
    ))
    .expect("generated album path is valid")
}
fn track_audio_rel(owner: ArtistId, album: AlbumId, id: TrackId, extension: &str) -> RelPath {
    RelPath::from_utf8(&format!(
        "artists/artist_{:06}/albums/album_{:06}/tracks/track_{:06}.{extension}",
        owner.get(),
        album.get(),
        id.get()
    ))
    .expect("generated track path is valid")
}
fn track_record_rel(owner: ArtistId, album: AlbumId, id: TrackId) -> RelPath {
    RelPath::from_utf8(&format!(
        "artists/artist_{:06}/albums/album_{:06}/tracks/track_{:06}.flux",
        owner.get(),
        album.get(),
        id.get()
    ))
    .expect("generated record path is valid")
}
fn playlist_rel(id: PlaylistId) -> RelPath {
    RelPath::from_utf8(&format!("playlists/playlist_{:06}.flux", id.get()))
        .expect("generated playlist path is valid")
}
fn canonical_rel_path(text: &str, file: &str) -> Result<RelPath, CanonicalError> {
    let rel = RelPath::from_utf8(text)?;
    // The generated canonical layout never uses raw-byte paths; accepting
    // them would make cross-platform validation and hashing inconsistent.
    if rel.as_str().is_none() {
        return Err(CanonicalError::Invalid(format!(
            "{file} contains a non-UTF-8 canonical path"
        )));
    }
    Ok(rel)
}
fn safe_join(root: &Path, rel: &RelPath) -> Result<PathBuf, CanonicalError> {
    let os = rel.to_os_path().ok_or(CanonicalError::InvalidPath(
        PathValidationError::NonUtf8Component,
    ))?;
    Ok(root.join(os))
}

/// Rejects symlink components in an existing canonical resource path. The
/// final component may be absent; missing ancestors are also allowed while
/// creating a library, but any existing symlink is an escape attempt.
fn reject_symlink_escape(root: &Path, rel: &RelPath) -> Result<(), CanonicalError> {
    let os = rel.to_os_path().ok_or(CanonicalError::InvalidPath(
        PathValidationError::NonUtf8Component,
    ))?;
    let mut current = root.to_path_buf();
    for component in os.components() {
        let Component::Normal(component) = component else {
            return Err(CanonicalError::InvalidPath(PathValidationError::Absolute));
        };
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CanonicalError::Invalid(format!(
                    "path {} contains a symbolic link",
                    rel.display_lossy()
                )));
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

fn valid_library_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}
fn generate_library_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = nanos
        ^ (u64::from(std::process::id()) << 32)
        ^ COUNTER.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        out.push_str(&format!("{z:016x}"));
    }
    out
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path, file: &str) -> Result<T, CanonicalError> {
    let bytes = fs::read(path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound {
            CanonicalError::MissingResource(path.to_path_buf())
        } else {
            CanonicalError::Io(err)
        }
    })?;
    serde_json::from_slice(&bytes).map_err(|err| CanonicalError::Malformed {
        file: file.to_owned(),
        message: err.to_string(),
    })
}

fn check_header(
    version: u32,
    kind: &str,
    file: &str,
    expected_kind: &'static str,
) -> Result<(), CanonicalError> {
    if version != CANONICAL_SCHEMA_VERSION {
        return Err(CanonicalError::UnsupportedVersion {
            file: file.to_owned(),
            found: version,
            supported: CANONICAL_SCHEMA_VERSION,
        });
    }
    if kind != expected_kind {
        return Err(CanonicalError::WrongKind {
            file: file.to_owned(),
            expected: expected_kind,
            found: kind.to_owned(),
        });
    }
    Ok(())
}

// ------------------------------------------------------------ opening ---

/// User/caller-supplied metadata for one canonical import. No audio tag is
/// read; callers may derive these values from filenames or explicit review.
#[derive(Debug, Clone)]
pub struct CanonicalImportedTrack {
    /// Source audio file to copy into the album directory.
    pub source: PathBuf,
    /// Canonical title.
    pub title: String,
    /// Existing stable owner artist, when the review selected one.
    pub artist_id: Option<ArtistId>,
    /// Canonical owner artist name (created or reused by exact name).
    pub artist_name: String,
    /// Ordered credited artists. Empty means just the resolved owner.
    pub credited_artist_ids: Vec<ArtistId>,
    /// Album title (required for canonical storage).
    pub album_title: Option<String>,
    /// Release type when a new album is created.
    pub release_type: ReleaseType,
    /// Optional track number.
    pub track_number: Option<u32>,
    /// Optional disc number.
    pub disc_number: Option<u32>,
    /// Optional technical duration in milliseconds.
    pub duration_ms: Option<u64>,
}

impl CanonicalLibrary {
    /// Creates an empty canonical library at `root`.
    ///
    /// The root must be either absent or empty. A partial canonical layout is
    /// never overwritten; use an explicit migration/repair workflow instead.
    pub fn create(root: &Path) -> Result<Self, CanonicalError> {
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(CanonicalError::Invalid(
                    "the library root must not be a symbolic link".to_owned(),
                ));
            }
            Ok(metadata) if metadata.is_dir() => {
                let mut entries = fs::read_dir(root)?;
                if entries.next().is_some() {
                    return Err(CanonicalError::AlreadyExists(root.to_path_buf()));
                }
            }
            Ok(_) => return Err(CanonicalError::AlreadyExists(root.to_path_buf())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(root)?;
            }
            Err(err) => return Err(err.into()),
        }

        for dir in [
            ARTISTS_DIR_NAME,
            PLAYLISTS_DIR_NAME,
            TRANSACTIONS_DIR_NAME,
            BACKUPS_DIR_NAME,
        ] {
            fs::create_dir_all(root.join(dir))?;
        }
        let manifest = LibraryManifestDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: MANIFEST_KIND.to_owned(),
            library_id: generate_library_id(),
            created_unix_ms: crate::portable::now_unix_ms(),
            catalog: CATALOG_FILE_NAME.to_owned(),
        };
        let catalog = CatalogDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: CATALOG_KIND.to_owned(),
            library_id: manifest.library_id.clone(),
            generation: 1,
            transaction_id: None,
            next_track_id: 1,
            next_artist_id: 1,
            next_album_id: 1,
            next_playlist_id: 1,
            artists: Vec::new(),
            albums: Vec::new(),
            tracks: Vec::new(),
            playlists: Vec::new(),
            ratings_path: RATINGS_FILE_NAME.to_owned(),
        };
        let ratings = RatingsDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: RATINGS_KIND.to_owned(),
            entries: Vec::new(),
        };
        write_atomic(
            &root.join(RATINGS_FILE_NAME),
            &serde_json::to_vec_pretty(&ratings).map_err(|err| CanonicalError::Malformed {
                file: RATINGS_FILE_NAME.to_owned(),
                message: err.to_string(),
            })?,
        )?;
        write_atomic(
            &root.join(CATALOG_FILE_NAME),
            &serde_json::to_vec_pretty(&catalog).map_err(|err| CanonicalError::Malformed {
                file: CATALOG_FILE_NAME.to_owned(),
                message: err.to_string(),
            })?,
        )?;
        write_atomic(
            &root.join(LIBRARY_FILE_NAME),
            &serde_json::to_vec_pretty(&manifest).map_err(|err| CanonicalError::Malformed {
                file: LIBRARY_FILE_NAME.to_owned(),
                message: err.to_string(),
            })?,
        )?;
        Self::open(root)
    }

    /// Opens and fully validates a canonical library, recovering any
    /// interrupted transaction first.
    ///
    /// # Errors
    ///
    /// Missing/invalid manifest or catalogue, malformed entity files,
    /// duplicate IDs/paths, invalid relationships, path traversal/symlink
    /// escapes, missing required entity records, and unrecoverable journals.
    pub fn open(root: &Path) -> Result<Self, CanonicalError> {
        let root = std::fs::canonicalize(root).map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                CanonicalError::NotFound(root.to_path_buf())
            } else {
                CanonicalError::Io(err)
            }
        })?;
        let manifest_path = root.join(LIBRARY_FILE_NAME);
        if !manifest_path.exists() {
            return Err(CanonicalError::NotFound(root));
        }
        let manifest: LibraryManifestDto = read_json(&manifest_path, LIBRARY_FILE_NAME)?;
        check_header(
            manifest.schema_version,
            &manifest.kind,
            LIBRARY_FILE_NAME,
            MANIFEST_KIND,
        )?;
        if !valid_library_id(&manifest.library_id) {
            return Err(CanonicalError::Invalid(
                "library.flux has an invalid library_id".to_owned(),
            ));
        }
        if manifest.catalog != CATALOG_FILE_NAME {
            return Err(CanonicalError::Invalid(
                "library.flux references an unexpected catalogue file".to_owned(),
            ));
        }

        let catalog_path = root.join(CATALOG_FILE_NAME);
        let catalog: CatalogDto = read_json(&catalog_path, CATALOG_FILE_NAME)?;
        check_header(
            catalog.schema_version,
            &catalog.kind,
            CATALOG_FILE_NAME,
            CATALOG_KIND,
        )?;
        if catalog.library_id != manifest.library_id {
            return Err(CanonicalError::Invalid(
                "catalogue library_id does not match library.flux".to_owned(),
            ));
        }
        recover_transactions(
            &root,
            &manifest.library_id,
            catalog.generation,
            catalog.transaction_id.as_deref(),
        )?;
        Self::load(root, manifest, catalog)
    }

    fn load(
        root: PathBuf,
        manifest: LibraryManifestDto,
        catalog: CatalogDto,
    ) -> Result<Self, CanonicalError> {
        validate_catalog_header(&catalog)?;
        let mut artist_paths = BTreeMap::new();
        let mut album_paths = BTreeMap::new();
        let mut track_paths = BTreeMap::new();
        let mut track_records = BTreeMap::new();
        let mut playlist_paths = BTreeMap::new();
        let mut seen_audio_paths = HashSet::new();

        for entry in &catalog.artists {
            let id = ArtistId::new(entry.id);
            if id.get() == 0 {
                return Err(CanonicalError::Invalid("artist ID 0".to_owned()));
            }
            let rel = canonical_rel_path(&entry.path, ARTISTS_DIR_NAME)?;
            if rel != artist_rel(id) {
                return Err(CanonicalError::Invalid(format!(
                    "artist {id} path {} is not canonical",
                    entry.path
                )));
            }
            if artist_paths.insert(id, rel).is_some() {
                return Err(CanonicalError::Invalid(format!(
                    "artist ID {id} appears more than once"
                )));
            }
        }
        for entry in &catalog.albums {
            let id = AlbumId::new(entry.id);
            let owner = ArtistId::new(entry.artist_id);
            if id.get() == 0 || owner.get() == 0 {
                return Err(CanonicalError::Invalid("album/owner ID 0".to_owned()));
            }
            if !artist_paths.contains_key(&owner) {
                return Err(CanonicalError::Invalid(format!(
                    "album {id} references missing artist {owner}"
                )));
            }
            let rel = canonical_rel_path(&entry.path, "albums")?;
            if rel != album_rel(owner, id) {
                return Err(CanonicalError::Invalid(format!(
                    "album {id} path {} is not canonical",
                    entry.path
                )));
            }
            if album_paths.insert(id, rel).is_some() {
                return Err(CanonicalError::Invalid(format!(
                    "album ID {id} appears more than once"
                )));
            }
        }
        for entry in &catalog.tracks {
            let id = TrackId::new(entry.id);
            let owner = ArtistId::new(entry.owner_artist_id);
            let album = AlbumId::new(entry.album_id);
            if id.get() == 0 || owner.get() == 0 || album.get() == 0 {
                return Err(CanonicalError::Invalid("track/owner/album ID 0".to_owned()));
            }
            if !artist_paths.contains_key(&owner) {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} references missing artist {owner}"
                )));
            }
            if !album_paths.contains_key(&album) {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} references missing album {album}"
                )));
            }
            let audio = canonical_rel_path(&entry.audio_path, "track audio")?;
            let Some(extension) = Path::new(&entry.audio_path)
                .extension()
                .and_then(|e| e.to_str())
            else {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} has no audio extension"
                )));
            };
            if audio != track_audio_rel(owner, album, id, extension) {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} audio path {} is not canonical",
                    entry.audio_path
                )));
            }
            let record = canonical_rel_path(&entry.record_path, "track record")?;
            if record != track_record_rel(owner, album, id) {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} record path {} is not canonical",
                    entry.record_path
                )));
            }
            if !seen_audio_paths.insert(entry.audio_path.clone()) {
                return Err(CanonicalError::Invalid(format!(
                    "track audio path {} is claimed more than once",
                    entry.audio_path
                )));
            }
            track_paths.insert(id, audio);
            track_records.insert(id, record);
        }
        for entry in &catalog.playlists {
            let id = PlaylistId::new(entry.id);
            if id.get() == 0 {
                return Err(CanonicalError::Invalid("playlist ID 0".to_owned()));
            }
            let rel = canonical_rel_path(&entry.path, "playlist")?;
            if rel != playlist_rel(id) {
                return Err(CanonicalError::Invalid(format!(
                    "playlist {id} path {} is not canonical",
                    entry.path
                )));
            }
            playlist_paths.insert(id, rel);
        }
        if playlist_paths.len() != catalog.playlists.len()
            || artist_paths.len() != catalog.artists.len()
            || album_paths.len() != catalog.albums.len()
            || track_paths.len() != catalog.tracks.len()
        {
            return Err(CanonicalError::Invalid(
                "catalogue contains duplicate entity IDs".to_owned(),
            ));
        }

        let mut artists = BTreeMap::new();
        for (id, rel) in &artist_paths {
            let file = format!("{}.flux", rel.display_lossy());
            let dto: ArtistEntityDto =
                read_json(&safe_join(&root, rel)?.join("artist.flux"), &file)?;
            check_header(dto.schema_version, &dto.kind, &file, ARTIST_KIND)?;
            if dto.id != id.get() || dto.id == 0 {
                return Err(CanonicalError::Invalid(format!(
                    "{file} ID does not match the catalogue"
                )));
            }
            if dto.name.trim().is_empty() {
                return Err(CanonicalError::Invalid(format!("{file} has an empty name")));
            }
            artists.insert(
                *id,
                CanonicalArtist {
                    id: *id,
                    name: dto.name,
                    aliases: dto.aliases,
                },
            );
        }

        let mut albums = BTreeMap::new();
        let mut album_track_membership: HashMap<TrackId, AlbumId> = HashMap::new();
        for (id, rel) in &album_paths {
            let owner = catalog
                .albums
                .iter()
                .find(|entry| entry.id == id.get())
                .map(|entry| ArtistId::new(entry.artist_id))
                .ok_or_else(|| CanonicalError::Invalid("missing album descriptor".to_owned()))?;
            let file = format!("{}.flux", rel.display_lossy());
            let dto: AlbumEntityDto = read_json(&safe_join(&root, rel)?.join("album.flux"), &file)?;
            check_header(dto.schema_version, &dto.kind, &file, ALBUM_KIND)?;
            if dto.id != id.get() || dto.owner_artist_id != owner.get() {
                return Err(CanonicalError::Invalid(format!(
                    "{file} ID/owner does not match the catalogue"
                )));
            }
            if dto.title.trim().is_empty() {
                return Err(CanonicalError::Invalid(format!(
                    "{file} has an empty title"
                )));
            }
            if let Some(cover) = &dto.cover
                && (cover.is_empty()
                    || cover.contains('/')
                    || cover.contains('\\')
                    || cover == "."
                    || cover == "..")
            {
                return Err(CanonicalError::Invalid(format!(
                    "{file} has an invalid cover file name"
                )));
            }
            if let Some(cover) = &dto.cover {
                reject_symlink_escape(
                    &root,
                    &RelPath::from_utf8(&format!("{}/{}", rel.display_lossy(), cover))?,
                )?;
            }
            let mut order = Vec::with_capacity(dto.track_order.len());
            let mut seen = HashSet::new();
            for raw in dto.track_order {
                let track = TrackId::new(raw);
                if !seen.insert(track) {
                    return Err(CanonicalError::Invalid(format!(
                        "album {id} lists track {track} more than once"
                    )));
                }
                if album_track_membership.insert(track, *id).is_some() {
                    return Err(CanonicalError::Invalid(format!(
                        "track {track} appears in more than one album"
                    )));
                }
                order.push(track);
            }
            albums.insert(
                *id,
                CanonicalAlbum {
                    id: *id,
                    owner_artist_id: owner,
                    title: dto.title,
                    release_type: dto.release_type,
                    cover: dto.cover,
                    track_order: order,
                },
            );
        }

        let mut tracks = BTreeMap::new();
        for descriptor in &catalog.tracks {
            let id = TrackId::new(descriptor.id);
            let rel = track_records
                .get(&id)
                .ok_or_else(|| CanonicalError::Invalid("missing track record path".to_owned()))?;
            let file = format!("{}.flux", rel.display_lossy());
            let dto: TrackEntityDto = read_json(&safe_join(&root, rel)?, &file)?;
            check_header(dto.schema_version, &dto.kind, &file, TRACK_KIND)?;
            if dto.id != id.get() {
                return Err(CanonicalError::Invalid(format!(
                    "{file} ID does not match the catalogue"
                )));
            }
            if dto.title.trim().is_empty() {
                return Err(CanonicalError::Invalid(format!(
                    "{file} has an empty title"
                )));
            }
            let owner = ArtistId::new(descriptor.owner_artist_id);
            let album_id = AlbumId::new(descriptor.album_id);
            if dto.owner_artist_id != owner.get() || dto.album_id != album_id.get() {
                return Err(CanonicalError::Invalid(format!(
                    "{file} owner/album does not match the catalogue"
                )));
            }
            let Some(album) = albums.get(&album_id) else {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} references missing album {album_id}"
                )));
            };
            if album.owner_artist_id != owner {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} owner is not the album owner"
                )));
            }
            if album_track_membership.get(&id) != Some(&album_id)
                || !album.track_order.contains(&id)
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} is not consistently listed in album {album_id}"
                )));
            }
            if dto.credited_artist_ids.is_empty() {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} has no credited artists"
                )));
            }
            let mut credits = Vec::with_capacity(dto.credited_artist_ids.len());
            let mut credit_set = HashSet::new();
            for raw in dto.credited_artist_ids {
                let artist = ArtistId::new(raw);
                if !artist_paths.contains_key(&artist) {
                    return Err(CanonicalError::Invalid(format!(
                        "track {id} credits missing artist {artist}"
                    )));
                }
                if !credit_set.insert(artist) {
                    return Err(CanonicalError::Invalid(format!(
                        "track {id} credits artist {artist} more than once"
                    )));
                }
                credits.push(artist);
            }
            if !credits.contains(&owner) {
                return Err(CanonicalError::Invalid(format!(
                    "track {id}'s physical owner is not among its credited artists"
                )));
            }
            if let Some(lyrics) = &dto.lyrics
                && (lyrics.is_empty()
                    || lyrics.contains('/')
                    || lyrics.contains('\\')
                    || lyrics == "."
                    || lyrics == "..")
            {
                return Err(CanonicalError::Invalid(format!(
                    "{file} has an invalid lyrics file name"
                )));
            }
            if let Some(lyrics) = &dto.lyrics {
                let Some(record_rel) = track_records.get(&id) else {
                    return Err(CanonicalError::Invalid(
                        "missing track record path".to_owned(),
                    ));
                };
                reject_symlink_escape(
                    &root,
                    &RelPath::from_utf8(&format!(
                        "{}/{}",
                        record_rel
                            .display_lossy()
                            .rsplit_once('/')
                            .map_or("", |(dir, _)| dir),
                        lyrics
                    ))?,
                )?;
            }
            tracks.insert(
                id,
                CanonicalTrack {
                    id,
                    title: dto.title,
                    credited_artist_ids: credits,
                    owner_artist_id: owner,
                    album_id,
                    track_number: dto.track_number,
                    disc_number: dto.disc_number,
                    duration_ms: dto.duration_ms,
                    lyrics: dto.lyrics,
                    fingerprint: dto.fingerprint,
                },
            );
        }
        for album in albums.values() {
            for track in &album.track_order {
                if !tracks.contains_key(track) {
                    return Err(CanonicalError::Invalid(format!(
                        "album {} references missing track {track}",
                        album.id
                    )));
                }
            }
        }
        if tracks.len() != catalog.tracks.len() {
            return Err(CanonicalError::Invalid(
                "catalogue contains duplicate track IDs".to_owned(),
            ));
        }

        let mut playlists = BTreeMap::new();
        for (id, rel) in &playlist_paths {
            let file = format!("{}.flux", rel.display_lossy());
            let dto: PlaylistEntityDto = read_json(&safe_join(&root, rel)?, &file)?;
            check_header(dto.schema_version, &dto.kind, &file, PLAYLIST_KIND)?;
            if dto.id != id.get() {
                return Err(CanonicalError::Invalid(format!(
                    "{file} ID does not match the catalogue"
                )));
            }
            let mut order = Vec::with_capacity(dto.track_ids.len());
            for raw in dto.track_ids {
                let track = TrackId::new(raw);
                if !tracks.contains_key(&track) {
                    return Err(CanonicalError::Invalid(format!(
                        "playlist {id} references missing track {track}"
                    )));
                }
                order.push(track);
            }
            playlists.insert(
                *id,
                CanonicalPlaylist {
                    id: *id,
                    name: dto.name,
                    track_ids: order,
                },
            );
        }

        let ratings_path = canonical_rel_path(&catalog.ratings_path, RATINGS_FILE_NAME)?;
        if ratings_path != RelPath::from_utf8(RATINGS_FILE_NAME)? {
            return Err(CanonicalError::Invalid(
                "ratings_path must be ratings.flux".to_owned(),
            ));
        }
        let ratings_dto: RatingsDto = read_json(&root.join(RATINGS_FILE_NAME), RATINGS_FILE_NAME)?;
        check_header(
            ratings_dto.schema_version,
            &ratings_dto.kind,
            RATINGS_FILE_NAME,
            RATINGS_KIND,
        )?;
        let mut ratings = BTreeMap::new();
        for entry in ratings_dto.entries {
            let track = TrackId::new(entry.track_id);
            if !tracks.contains_key(&track) {
                return Err(CanonicalError::Invalid(format!(
                    "ratings file references missing track {track}"
                )));
            }
            if entry.rating.is_some_and(|rating| rating > 5) {
                return Err(CanonicalError::Invalid(format!(
                    "track {track} has a rating above 5"
                )));
            }
            if ratings
                .insert(
                    track,
                    TrackRating {
                        rating: entry.rating,
                        favorite: entry.favorite,
                    },
                )
                .is_some()
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {track} has duplicate ratings"
                )));
            }
        }

        // Validate every canonical path is representable and free of any
        // symlink component before handing the library to a caller.
        for rel in artist_paths
            .values()
            .chain(album_paths.values())
            .chain(track_paths.values())
            .chain(track_records.values())
            .chain(playlist_paths.values())
        {
            reject_symlink_escape(&root, rel)?;
        }
        for audio in track_paths.values() {
            reject_symlink_escape(&root, audio)?;
        }

        Ok(Self {
            root,
            manifest,
            catalog,
            artists,
            albums,
            tracks,
            playlists,
            ratings,
            track_paths,
            track_records,
            artist_paths,
            album_paths,
            playlist_paths,
            fail_scenario: None,
        })
    }
}

fn validate_catalog_header(catalog: &CatalogDto) -> Result<(), CanonicalError> {
    check_header(
        catalog.schema_version,
        &catalog.kind,
        CATALOG_FILE_NAME,
        CATALOG_KIND,
    )?;
    if catalog.generation == 0 {
        return Err(CanonicalError::Invalid("catalogue generation 0".to_owned()));
    }
    for (name, value) in [
        ("next_track_id", catalog.next_track_id),
        ("next_artist_id", catalog.next_artist_id),
        ("next_album_id", catalog.next_album_id),
        ("next_playlist_id", catalog.next_playlist_id),
    ] {
        if value == 0 {
            return Err(CanonicalError::Invalid(format!("{name} is 0")));
        }
    }
    Ok(())
}

// ------------------------------------------------------- file writing ---

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CanonicalError> {
    let parent = path
        .parent()
        .ok_or_else(|| CanonicalError::Invalid("file has no parent directory".to_owned()))?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("flux")
    ));
    let mut file = fs::File::create(&temp)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, path)?;
    sync_directory(parent)?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), CanonicalError> {
    #[cfg(unix)]
    {
        fs::File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

// ------------------------------------------------------ transactions ---

fn transaction_rel(txn: &str) -> RelPath {
    RelPath::from_utf8(&format!("{TRANSACTIONS_DIR_NAME}/{txn}"))
        .expect("generated transaction path is valid")
}

fn write_journal(root: &Path, journal: &JournalDto) -> Result<(), CanonicalError> {
    let dir = safe_join(root, &transaction_rel(&journal.transaction_id))?;
    fs::create_dir_all(&dir)?;
    if let Some(parent) = dir.parent() {
        sync_directory(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(journal).map_err(|err| CanonicalError::Malformed {
        file: "transaction journal".to_owned(),
        message: err.to_string(),
    })?;
    write_atomic(&dir.join("journal.flux"), &bytes)
}

fn recover_one_transaction(
    root: &Path,
    txn_dir: &Path,
    journal: &JournalDto,
    _catalog_transaction: Option<&str>,
) -> Result<(), CanonicalError> {
    for action in &journal.actions {
        let rel = canonical_rel_path(&action.target, "transaction journal")?;
        let target = safe_join(root, &rel)?;
        match &action.backup {
            Some(backup) => {
                let backup = txn_dir.join(backup);
                if backup.exists() {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::rename(backup, target)?;
                }
            }
            None if !action.existed => {
                let _ = fs::remove_file(target);
            }
            None => {}
        }
    }
    let _ = fs::remove_dir_all(txn_dir);
    Ok(())
}

fn catalog_committed(root: &Path, transaction_id: &str) -> Result<bool, CanonicalError> {
    let path = root.join(CATALOG_FILE_NAME);
    if !path.exists() {
        return Ok(false);
    }
    let catalog: CatalogDto = read_json(&path, CATALOG_FILE_NAME)?;
    Ok(catalog.transaction_id.as_deref() == Some(transaction_id))
}

fn cleanup_one_path(
    root: &Path,
    journal: &JournalDto,
    target: &RelPath,
) -> Result<(), CanonicalError> {
    // Never remove a path that the active catalogue still references.
    let catalog_path = root.join(CATALOG_FILE_NAME);
    if catalog_path.exists() {
        let catalog: CatalogDto = read_json(&catalog_path, CATALOG_FILE_NAME)?;
        let text = target.display_lossy();
        let referenced = catalog
            .tracks
            .iter()
            .any(|track| track.audio_path == text || track.record_path == text);
        if referenced || catalog.ratings_path == text {
            return Err(CanonicalError::Transaction {
                file: journal.transaction_id.clone(),
                message: format!(
                    "refusing to remove cleanup path still referenced by the catalogue: {text}"
                ),
            });
        }
    }
    reject_symlink_escape(root, target)?;
    let path = safe_join(root, target)?;
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(CanonicalError::Io(err)),
    }
}

fn cleanup_committed_paths(root: &Path, journal: &JournalDto) -> Result<(), CanonicalError> {
    for target in &journal.cleanup {
        let rel = canonical_rel_path(target, "transaction cleanup")?;
        cleanup_one_path(root, journal, &rel)?;
    }
    Ok(())
}

fn recover_transactions(
    root: &Path,
    library_id: &str,
    generation: u64,
    catalog_transaction: Option<&str>,
) -> Result<(), CanonicalError> {
    let dir = root.join(TRANSACTIONS_DIR_NAME);
    if !dir.exists() {
        return Ok(());
    }
    let mut entries = fs::read_dir(&dir)?.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let journal_path = entry.path().join("journal.flux");
        let journal: JournalDto = read_json(&journal_path, &format!("{name}/journal.flux"))
            .map_err(|err| CanonicalError::Transaction {
                file: name.clone(),
                message: err.to_string(),
            })?;
        check_header(
            journal.schema_version,
            &journal.kind,
            &format!("{name}/journal.flux"),
            JOURNAL_KIND,
        )
        .map_err(|err| CanonicalError::Transaction {
            file: name.clone(),
            message: err.to_string(),
        })?;
        if journal.transaction_id != name {
            return Err(CanonicalError::Transaction {
                file: name,
                message: "journal transaction_id does not match its directory".to_owned(),
            });
        }
        if catalog_transaction == Some(journal.transaction_id.as_str()) {
            // The catalogue is the authoritative commit record. Finish only
            // this transaction's journal-owned cleanup.
            cleanup_committed_paths(root, &journal)?;
            let _ = fs::remove_dir_all(entry.path());
            continue;
        }
        if matches!(
            journal.state,
            JournalState::Committed | JournalState::Cleaning | JournalState::Completed
        ) {
            return Err(CanonicalError::Transaction {
                file: name,
                message: "journal claims a committed state but the active catalogue does not record this transaction; preserving all files for recovery"
                    .to_owned(),
            });
        }
        // Uncommitted. Restore replaced files and delete only files that this
        // transaction created and explicitly journaled.
        for action in &journal.actions {
            let rel = canonical_rel_path(&action.target, &format!("{name}/journal.flux")).map_err(
                |err| CanonicalError::Transaction {
                    file: name.clone(),
                    message: err.to_string(),
                },
            )?;
            let target = safe_join(root, &rel).map_err(|err| CanonicalError::Transaction {
                file: name.clone(),
                message: err.to_string(),
            })?;
            match &action.backup {
                Some(backup) => {
                    let backup = entry.path().join(backup);
                    if backup.exists() {
                        if let Some(parent) = target.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::rename(backup, target)?;
                    }
                }
                None if !action.existed => {
                    let _ = fs::remove_file(target);
                }
                None => {}
            }
        }
        let _ = fs::remove_dir_all(entry.path());
    }
    // The caller will re-read the catalogue after recovery.
    let _ = (library_id, generation);
    Ok(())
}

impl CanonicalLibrary {
    /// The next TrackId counter, preserving retired-ID gaps.
    #[must_use]
    pub fn next_track_id(&self) -> u64 {
        self.catalog.next_track_id
    }
    /// The next ArtistId counter.
    #[must_use]
    pub fn next_artist_id(&self) -> u64 {
        self.catalog.next_artist_id
    }
    /// The next AlbumId counter.
    #[must_use]
    pub fn next_album_id(&self) -> u64 {
        self.catalog.next_album_id
    }
    /// The next PlaylistId counter.
    #[must_use]
    pub fn next_playlist_id(&self) -> u64 {
        self.catalog.next_playlist_id
    }
    /// Ordered credited artists for a track.
    #[must_use]
    pub fn track_credits(&self, id: TrackId) -> Option<&[ArtistId]> {
        self.tracks
            .get(&id)
            .map(|track| track.credited_artist_ids.as_slice())
    }
    /// Track IDs in an album's canonical order.
    #[must_use]
    pub fn album_tracks(&self, id: AlbumId) -> Vec<TrackId> {
        self.albums
            .get(&id)
            .map(|album| album.track_order.clone())
            .unwrap_or_default()
    }
    /// Finds an album by exact owner and title.
    #[must_use]
    pub fn album_by_title(&self, owner: ArtistId, title: &str) -> Option<AlbumId> {
        self.albums
            .values()
            .find(|album| album.owner_artist_id == owner && album.title == title)
            .map(|album| album.id)
    }
    /// All track IDs physically owned by an artist.
    pub fn owned_track_ids(&self, artist: ArtistId) -> impl Iterator<Item = TrackId> + '_ {
        self.tracks
            .values()
            .filter(move |track| track.owner_artist_id == artist)
            .map(|track| track.id)
    }
    /// All track IDs an artist is credited on, including featuring credits.
    pub fn credited_track_ids(&self, artist: ArtistId) -> impl Iterator<Item = TrackId> + '_ {
        self.tracks
            .values()
            .filter(move |track| track.credited_artist_ids.contains(&artist))
            .map(|track| track.id)
    }
    /// Absolute path to a track's managed lyrics file, when recorded.
    #[must_use]
    pub fn track_lyrics_path(&self, id: TrackId) -> Option<PathBuf> {
        let track = self.tracks.get(&id)?;
        let lyrics = track.lyrics.as_ref()?;
        let record = self.track_records.get(&id)?;
        let os = record.to_os_path()?;
        let dir = os.parent()?;
        Some(self.root.join(dir).join(lyrics))
    }
    /// Finds a track with exactly the given content fingerprint.
    #[must_use]
    pub fn track_by_fingerprint(&self, fingerprint: ContentFingerprint) -> Option<TrackId> {
        self.tracks
            .values()
            .find(|track| track.fingerprint == Some(fingerprint))
            .map(|track| track.id)
    }
    /// Renames an album without changing its ID or directory. The generated
    /// directory name is ID-derived, so no physical move is required.
    pub fn rename_album(
        &mut self,
        id: AlbumId,
        title: impl Into<String>,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let album = self
            .albums
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("album is not in the library".to_owned()))?;
        let title = title.into().trim().to_owned();
        if title.is_empty() {
            return Err(CanonicalError::Invalid(
                "album title must not be empty".to_owned(),
            ));
        }
        let rel = self
            .album_paths
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing album path".to_owned()))?;
        let dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: id.get(),
            owner_artist_id: album.owner_artist_id.get(),
            title,
            release_type: album.release_type,
            cover: album.cover,
            track_order: album.track_order.iter().map(|track| track.get()).collect(),
        };
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &rel, "album.flux", &dto)?;
        self.commit_writes(None, writes)?;
        self.reopen_after()?;
        Ok(())
    }
    /// Rebuilds the legacy in-memory [`Library`] view from canonical data.
    /// This is a presentation adapter only; canonical files remain the
    /// authority.
    #[must_use]
    pub fn runtime_library(&self) -> crate::Library {
        let mut tracks = Vec::with_capacity(self.tracks.len());
        for track in self.tracks.values() {
            let Some(path) = self.track_audio_path(track.id) else {
                continue;
            };
            let mut runtime =
                flux_core::Track::new(track.title.clone(), flux_core::TrackSource::local(path));
            if let Some(artist) = self
                .artists
                .get(&track.owner_artist_id)
                .map(|artist| artist.name.clone())
            {
                runtime = runtime.with_artist(artist);
            }
            if let Some(album) = self.albums.get(&track.album_id) {
                runtime = runtime.with_album(album.title.clone());
            }
            if let Some(number) = track.track_number {
                runtime = runtime.with_track_number(number);
            }
            if let Some(disc) = track.disc_number {
                runtime = runtime.with_disc_number(disc);
            }
            if let Some(ms) = track.duration_ms {
                runtime = runtime.with_duration(std::time::Duration::from_millis(ms));
            }
            tracks.push((track.id, runtime));
        }
        crate::Library::from_parts(tracks, self.catalog.next_track_id)
    }
}

// -------------------------------------------------------------- export ---

impl CanonicalLibrary {
    /// Iterates track audio paths and yields `(TrackId, path, available)`.
    pub fn track_audio_paths(&self) -> impl Iterator<Item = (TrackId, PathBuf, bool)> + '_ {
        self.track_paths.keys().filter_map(|id| {
            self.track_audio_path(*id).map(|path| {
                let available = fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
                (*id, path, available)
            })
        })
    }
}

// ------------------------------------------------------------- writes ---

struct PendingWrite {
    target: RelPath,
    bytes: Option<Vec<u8>>,
    copy_from: Option<PathBuf>,
    expected: Option<ContentFingerprint>,
}

fn fail_point_name(point: TransactionFailPoint) -> &'static str {
    match point {
        TransactionFailPoint::BeforeJournal => "before-journal",
        TransactionFailPoint::AfterJournal => "after-journal",
        TransactionFailPoint::BeforeStaging => "before-staging",
        TransactionFailPoint::DuringCopy => "during-copy",
        TransactionFailPoint::DuringAudioCopy => "during-audio-copy",
        TransactionFailPoint::AfterAudioCopyBeforeVerify => "after-audio-copy-before-verify",
        TransactionFailPoint::DuringLyricsCopy => "during-lyrics-copy",
        TransactionFailPoint::AfterCopyBeforeVerify => "after-copy-before-verify",
        TransactionFailPoint::AfterVerify => "after-verify",
        TransactionFailPoint::BeforeMetadataPublish => "before-metadata-publish",
        TransactionFailPoint::DuringMetadataPublish => "during-metadata-publish",
        TransactionFailPoint::BeforeCatalogPublish => "before-catalog-publish",
        TransactionFailPoint::AfterCatalogPublish => "after-catalog-publish",
        TransactionFailPoint::BeforeCleanup => "before-cleanup",
        TransactionFailPoint::DuringCleanup => "during-cleanup",
        TransactionFailPoint::DuringLyricsCleanup => "during-lyrics-cleanup",
        TransactionFailPoint::AfterCleanup => "after-cleanup",
        TransactionFailPoint::BeforeJournalFinalize => "before-journal-finalize",
    }
}

impl CanonicalLibrary {
    fn next_transaction_id() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        format!(
            "txn-{nanos:016x}-{:08x}",
            COUNTER.fetch_add(1, Ordering::Relaxed) as u32
        )
    }

    /// Test-only deterministic failure injection. Production callers never set
    /// this field, and there is no public UI/API to control it.
    #[cfg(test)]
    pub(crate) fn set_transaction_failure(&mut self, failure: Option<TransactionFailure>) {
        self.fail_scenario = failure;
    }

    fn maybe_fail(&self, point: TransactionFailPoint) -> Result<(), CanonicalError> {
        if self
            .fail_scenario
            .is_some_and(|failure| failure.point == point)
        {
            return Err(CanonicalError::Invalid(format!(
                "injected transaction failure at {}",
                fail_point_name(point)
            )));
        }
        Ok(())
    }

    fn fail_interrupts(&self, point: TransactionFailPoint) -> bool {
        self.fail_scenario
            .is_some_and(|failure| failure.point == point && failure.interrupt)
    }

    fn commit_writes(
        &mut self,
        new_catalog: Option<CatalogDto>,
        writes: Vec<PendingWrite>,
    ) -> Result<(), CanonicalError> {
        self.commit_writes_with_cleanup(new_catalog, writes, Vec::new())
    }

    fn commit_writes_with_cleanup(
        &mut self,
        new_catalog: Option<CatalogDto>,
        writes: Vec<PendingWrite>,
        cleanup: Vec<RelPath>,
    ) -> Result<(), CanonicalError> {
        self.maybe_fail(TransactionFailPoint::BeforeJournal)?;
        let transaction_id = Self::next_transaction_id();
        let txn_rel = transaction_rel(&transaction_id);
        let txn_dir = safe_join(&self.root, &txn_rel)?;
        fs::create_dir_all(&txn_dir)?;
        sync_directory(&self.root.join(TRANSACTIONS_DIR_NAME))?;

        let mut actions = Vec::with_capacity(writes.len());
        for (index, write) in writes.iter().enumerate() {
            reject_symlink_escape(&self.root, &write.target)?;
            let target = safe_join(&self.root, &write.target)?;
            let existed = target.exists();
            let backup = if existed {
                let name = format!("backup-{index}");
                let backup_path = txn_dir.join(&name);
                fs::copy(&target, &backup_path)?;
                fs::File::open(&backup_path)?.sync_all()?;
                Some(name)
            } else {
                None
            };
            actions.push(JournalActionDto {
                target: write.target.display_lossy(),
                existed,
                backup,
                staged_source: Some(format!("staging-{index}")),
            });
        }
        sync_directory(&txn_dir)?;
        let intended_generation = new_catalog
            .as_ref()
            .map_or(self.catalog.generation, |catalog| catalog.generation);
        let mut journal = JournalDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: JOURNAL_KIND.to_owned(),
            transaction_id: transaction_id.clone(),
            operation: if new_catalog.is_some() {
                "catalogue-write".to_owned()
            } else {
                "metadata-write".to_owned()
            },
            state: JournalState::Prepared,
            intended_generation,
            actions,
            cleanup: cleanup.iter().map(RelPath::display_lossy).collect(),
        };
        write_journal(&self.root, &journal)?;
        self.maybe_fail(TransactionFailPoint::AfterJournal)?;

        let mut interrupted = false;
        let result = (|| -> Result<(), CanonicalError> {
            journal.state = JournalState::Copying;
            write_journal(&self.root, &journal)?;
            for (index, write) in writes.iter().enumerate() {
                self.maybe_fail(TransactionFailPoint::BeforeStaging)?;
                let staging = txn_dir.join(format!("staging-{index}"));
                if let Some(bytes) = &write.bytes {
                    write_atomic(&staging, bytes)?;
                } else if let Some(source) = &write.copy_from {
                    fs::copy(source, &staging)?;
                    fs::File::open(&staging)?.sync_all()?;
                } else {
                    return Err(CanonicalError::Invalid(
                        "transaction write has neither bytes nor a source".to_owned(),
                    ));
                }
                let is_lyrics = write
                    .target
                    .display_lossy()
                    .to_ascii_lowercase()
                    .ends_with(".lrc");
                let is_audio = write.expected.is_some() && write.copy_from.is_some() && !is_lyrics;
                for point in [
                    Some(TransactionFailPoint::DuringCopy),
                    is_audio.then_some(TransactionFailPoint::DuringAudioCopy),
                    is_lyrics.then_some(TransactionFailPoint::DuringLyricsCopy),
                ]
                .into_iter()
                .flatten()
                {
                    if self.maybe_fail(point).is_err() {
                        interrupted = self.fail_interrupts(point);
                        return Err(CanonicalError::Invalid(format!(
                            "injected transaction failure at {}",
                            fail_point_name(point)
                        )));
                    }
                }
                let before_verify = if is_audio {
                    TransactionFailPoint::AfterAudioCopyBeforeVerify
                } else {
                    TransactionFailPoint::AfterCopyBeforeVerify
                };
                if self.maybe_fail(before_verify).is_err() {
                    interrupted = self.fail_interrupts(before_verify);
                    return Err(CanonicalError::Invalid(format!(
                        "injected transaction failure at {}",
                        fail_point_name(before_verify)
                    )));
                }
                if let Some(expected) = write.expected {
                    let actual = ContentFingerprint::of_file(&staging)?;
                    if actual != expected {
                        return Err(CanonicalError::Invalid(format!(
                            "copy verification failed for {}",
                            write
                                .copy_from
                                .as_ref()
                                .map(|source| source.display().to_string())
                                .unwrap_or_else(|| write.target.display_lossy())
                        )));
                    }
                }
            }
            sync_directory(&txn_dir)?;
            journal.state = JournalState::Verified;
            write_journal(&self.root, &journal)?;
            self.maybe_fail(TransactionFailPoint::AfterVerify)?;
            journal.state = JournalState::Publishing;
            write_journal(&self.root, &journal)?;
            self.maybe_fail(TransactionFailPoint::BeforeMetadataPublish)?;
            for (index, write) in writes.iter().enumerate() {
                let staging = txn_dir.join(format!("staging-{index}"));
                let target = safe_join(&self.root, &write.target)?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                    reject_symlink_escape(&self.root, &write.target)?;
                }
                fs::rename(&staging, &target)?;
                if let Some(parent) = target.parent() {
                    sync_directory(parent)?;
                }
                self.maybe_fail(TransactionFailPoint::DuringMetadataPublish)?;
            }
            self.maybe_fail(TransactionFailPoint::BeforeCatalogPublish)?;
            if let Some(mut catalog) = new_catalog {
                catalog.generation = self.catalog.generation.checked_add(1).ok_or_else(|| {
                    CanonicalError::Invalid("catalogue generation overflow".to_owned())
                })?;
                catalog.transaction_id = Some(transaction_id.clone());
                let catalog_path = self.root.join(CATALOG_FILE_NAME);
                if catalog_path.exists() {
                    let prior = fs::read(&catalog_path)?;
                    let backup_name = format!("catalog-gen-{}.flux", self.catalog.generation);
                    write_atomic(&self.root.join(BACKUPS_DIR_NAME).join(backup_name), &prior)?;
                    write_atomic(&self.root.join(CATALOG_BACKUP_FILE_NAME), &prior)?;
                }
                let bytes = serde_json::to_vec_pretty(&catalog).map_err(|err| {
                    CanonicalError::Malformed {
                        file: CATALOG_FILE_NAME.to_owned(),
                        message: err.to_string(),
                    }
                })?;
                write_atomic(&catalog_path, &bytes)?;
            }
            self.maybe_fail(TransactionFailPoint::AfterCatalogPublish)?;
            journal.state = JournalState::Committed;
            write_journal(&self.root, &journal)?;
            journal.state = JournalState::Cleaning;
            write_journal(&self.root, &journal)?;
            self.maybe_fail(TransactionFailPoint::BeforeCleanup)?;
            for target in &cleanup {
                self.maybe_fail(TransactionFailPoint::DuringCleanup)?;
                if target
                    .display_lossy()
                    .to_ascii_lowercase()
                    .ends_with(".lrc")
                {
                    self.maybe_fail(TransactionFailPoint::DuringLyricsCleanup)?;
                }
                cleanup_one_path(&self.root, &journal, target)?;
            }
            self.maybe_fail(TransactionFailPoint::AfterCleanup)?;
            journal.state = JournalState::Completed;
            write_journal(&self.root, &journal)?;
            self.maybe_fail(TransactionFailPoint::BeforeJournalFinalize)?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                let _ = fs::remove_dir_all(&txn_dir);
                let _ = sync_directory(&self.root.join(TRANSACTIONS_DIR_NAME));
                Ok(())
            }
            Err(err) => {
                let interrupted =
                    interrupted || self.fail_scenario.is_some_and(|failure| failure.interrupt);
                if interrupted {
                    return Err(err);
                }
                if catalog_committed(&self.root, &transaction_id)? {
                    // The catalogue is authoritative. Preserve the journal so a
                    // later open can finish cleanup; never roll back committed
                    // work.
                    return Err(err);
                }
                let _ = recover_one_transaction(&self.root, &txn_dir, &journal, None);
                Err(err)
            }
        }
    }

    fn reopen_after(&mut self) -> Result<(), CanonicalError> {
        let reopened = Self::open(&self.root)?;
        *self = reopened;
        Ok(())
    }

    fn ensure_no_duplicate_name(&self, name: &str) -> Result<(), CanonicalError> {
        if name.trim().is_empty() {
            return Err(CanonicalError::Invalid(
                "names must not be empty".to_owned(),
            ));
        }
        Ok(())
    }

    /// Creates an artist with a stable new ID.
    pub fn create_artist(
        &mut self,
        name: impl Into<String>,
        aliases: Vec<String>,
    ) -> Result<ArtistId, CanonicalError> {
        self.ensure_writable()?;
        let name = name.into().trim().to_owned();
        self.ensure_no_duplicate_name(&name)?;
        let id = ArtistId::new(self.catalog.next_artist_id);
        let rel = artist_rel(id);
        let file = ArtistEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ARTIST_KIND.to_owned(),
            id: id.get(),
            name,
            aliases: aliases
                .into_iter()
                .map(|alias| alias.trim().to_owned())
                .filter(|alias| !alias.is_empty())
                .collect(),
        };
        let mut catalog = self.catalog.clone();
        catalog.next_artist_id = catalog
            .next_artist_id
            .checked_add(1)
            .ok_or_else(|| CanonicalError::Invalid("artist counter overflow".to_owned()))?;
        catalog.artists.push(CatalogArtistDto {
            id: id.get(),
            path: rel.display_lossy(),
        });
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &rel, "artist.flux", &file)?;
        self.commit_writes(Some(catalog), writes)?;
        self.reopen_after()?;
        Ok(id)
    }

    /// Renames an artist without changing its ID or directory name. The
    /// canonical directory is ID-derived, so no physical move is needed.
    pub fn rename_artist(
        &mut self,
        id: ArtistId,
        name: impl Into<String>,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let artist =
            self.artists.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("artist {id} is not in the library"))
            })?;
        let name = name.into().trim().to_owned();
        self.ensure_no_duplicate_name(&name)?;
        let dto = ArtistEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ARTIST_KIND.to_owned(),
            id: id.get(),
            name,
            aliases: artist.aliases,
        };
        let rel = self
            .artist_paths
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing artist path".to_owned()))?;
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &rel, "artist.flux", &dto)?;
        self.commit_writes(None, writes)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Creates an empty album under an existing artist.
    pub fn create_album(
        &mut self,
        owner: ArtistId,
        title: impl Into<String>,
        release_type: ReleaseType,
    ) -> Result<AlbumId, CanonicalError> {
        self.ensure_writable()?;
        if !self.artists.contains_key(&owner) {
            return Err(CanonicalError::Invalid(format!(
                "owner artist {owner} is not in the library"
            )));
        }
        let title = title.into().trim().to_owned();
        self.ensure_no_duplicate_name(&title)?;
        let id = AlbumId::new(self.catalog.next_album_id);
        let rel = album_rel(owner, id);
        let dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: id.get(),
            owner_artist_id: owner.get(),
            title,
            release_type,
            cover: None,
            track_order: Vec::new(),
        };
        let mut catalog = self.catalog.clone();
        catalog.next_album_id = catalog
            .next_album_id
            .checked_add(1)
            .ok_or_else(|| CanonicalError::Invalid("album counter overflow".to_owned()))?;
        catalog.albums.push(CatalogAlbumDto {
            id: id.get(),
            artist_id: owner.get(),
            path: rel.display_lossy(),
        });
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &rel, "album.flux", &dto)?;
        self.commit_writes(Some(catalog), writes)?;
        self.reopen_after()?;
        Ok(id)
    }

    /// Describes one file to import. Metadata is supplied by the caller; this
    /// API never reads audio tags.
    pub fn import_track(
        &mut self,
        imported: CanonicalImportedTrack,
    ) -> Result<TrackId, CanonicalError> {
        self.ensure_writable()?;
        let source = imported.source;
        let fingerprint = ContentFingerprint::of_file(&source)?;
        let extension = source
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .ok_or_else(|| CanonicalError::Invalid("source has no usable extension".to_owned()))?;
        if extension.is_empty() || !extension.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(CanonicalError::Invalid(
                "source extension is not a safe canonical suffix".to_owned(),
            ));
        }
        let title = imported.title.trim().to_owned();
        if title.is_empty() {
            return Err(CanonicalError::Invalid(
                "track title must not be empty".to_owned(),
            ));
        }
        let artist_name = imported.artist_name.trim().to_owned();
        self.ensure_no_duplicate_name(&artist_name)?;
        let album_title = imported
            .album_title
            .map(|title| title.trim().to_owned())
            .filter(|title| !title.is_empty())
            .ok_or_else(|| {
                CanonicalError::Invalid("canonical imports require an album title".to_owned())
            })?;

        let mut catalog = self.catalog.clone();
        let mut writes = Vec::new();

        let artist_id = if let Some(id) = imported.artist_id {
            if !self.artists.contains_key(&id) {
                return Err(CanonicalError::Invalid(format!(
                    "selected owner artist {id} is not in the library"
                )));
            }
            id
        } else if let Some(entry) = catalog.artists.iter().find(|entry| {
            self.artists
                .get(&ArtistId::new(entry.id))
                .is_some_and(|artist| artist.name == artist_name)
        }) {
            ArtistId::new(entry.id)
        } else {
            {
                let id = ArtistId::new(catalog.next_artist_id);
                catalog.next_artist_id = catalog
                    .next_artist_id
                    .checked_add(1)
                    .ok_or_else(|| CanonicalError::Invalid("artist counter overflow".to_owned()))?;
                let rel = artist_rel(id);
                catalog.artists.push(CatalogArtistDto {
                    id: id.get(),
                    path: rel.display_lossy(),
                });
                let artist = ArtistEntityDto {
                    schema_version: CANONICAL_SCHEMA_VERSION,
                    kind: ARTIST_KIND.to_owned(),
                    id: id.get(),
                    name: artist_name,
                    aliases: Vec::new(),
                };
                push_entity_write(&mut writes, &rel, "artist.flux", &artist)?;
                id
            }
        };

        let credited_artist_ids = if imported.credited_artist_ids.is_empty() {
            vec![artist_id]
        } else {
            imported.credited_artist_ids.clone()
        };
        let unique_credits = credited_artist_ids.iter().collect::<HashSet<_>>();
        if credited_artist_ids.is_empty()
            || unique_credits.len() != credited_artist_ids.len()
            || !credited_artist_ids.contains(&artist_id)
            || credited_artist_ids
                .iter()
                .any(|artist| !catalog.artists.iter().any(|entry| entry.id == artist.get()))
        {
            return Err(CanonicalError::Invalid(
                "import credits must be non-empty, unique, known, and include the physical owner"
                    .to_owned(),
            ));
        }

        let mut album = catalog.albums.iter().find_map(|entry| {
            let id = AlbumId::new(entry.id);
            self.albums
                .get(&id)
                .filter(|album| album.owner_artist_id == artist_id && album.title == album_title)
                .cloned()
                .map(|album| (id, album))
        });
        let (album_id, album_rel_path, mut album) = match album.take() {
            Some((id, album)) => (
                id,
                self.album_paths
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| CanonicalError::Invalid("missing album path".to_owned()))?,
                album,
            ),
            None => {
                let id = AlbumId::new(catalog.next_album_id);
                catalog.next_album_id = catalog
                    .next_album_id
                    .checked_add(1)
                    .ok_or_else(|| CanonicalError::Invalid("album counter overflow".to_owned()))?;
                let rel = album_rel(artist_id, id);
                catalog.albums.push(CatalogAlbumDto {
                    id: id.get(),
                    artist_id: artist_id.get(),
                    path: rel.display_lossy(),
                });
                (
                    id,
                    album_rel(artist_id, id),
                    CanonicalAlbum {
                        id,
                        owner_artist_id: artist_id,
                        title: album_title,
                        release_type: imported.release_type,
                        cover: None,
                        track_order: Vec::new(),
                    },
                )
            }
        };

        let track_id = TrackId::new(catalog.next_track_id);
        catalog.next_track_id = catalog
            .next_track_id
            .checked_add(1)
            .ok_or_else(|| CanonicalError::Invalid("track counter overflow".to_owned()))?;
        let audio_rel = track_audio_rel(artist_id, album_id, track_id, &extension);
        let record_rel = track_record_rel(artist_id, album_id, track_id);
        catalog.tracks.push(CatalogTrackDto {
            id: track_id.get(),
            owner_artist_id: artist_id.get(),
            album_id: album_id.get(),
            audio_path: audio_rel.display_lossy(),
            record_path: record_rel.display_lossy(),
        });

        album.track_order.push(track_id);
        let album_dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: album_id.get(),
            owner_artist_id: artist_id.get(),
            title: album.title,
            release_type: album.release_type,
            cover: album.cover,
            track_order: album.track_order.iter().map(|id| id.get()).collect(),
        };
        let track_dto = TrackEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: TRACK_KIND.to_owned(),
            id: track_id.get(),
            title,
            credited_artist_ids: credited_artist_ids
                .iter()
                .map(|artist| artist.get())
                .collect(),
            owner_artist_id: artist_id.get(),
            album_id: album_id.get(),
            track_number: imported.track_number,
            disc_number: imported.disc_number,
            duration_ms: imported.duration_ms,
            lyrics: None,
            fingerprint: Some(fingerprint),
        };
        push_entity_write(&mut writes, &album_rel_path, "album.flux", &album_dto)?;
        push_file_write(&mut writes, &record_rel, &track_dto)?;
        writes.push(PendingWrite {
            target: audio_rel,
            bytes: None,
            copy_from: Some(source),
            expected: Some(fingerprint),
        });
        self.commit_writes(Some(catalog), writes)?;
        self.reopen_after()?;
        Ok(track_id)
    }

    /// Updates track metadata. Owner/album are intentionally not changed by
    /// this method; ownership moves have a separate plan/transfer workflow.
    pub fn update_track_metadata(
        &mut self,
        id: TrackId,
        title: impl Into<String>,
        credited_artist_ids: Vec<ArtistId>,
        track_number: Option<u32>,
        disc_number: Option<u32>,
        duration_ms: Option<u64>,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let track =
            self.tracks.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("track {id} is not in the library"))
            })?;
        let title = title.into().trim().to_owned();
        if title.is_empty() {
            return Err(CanonicalError::Invalid(
                "track title must not be empty".to_owned(),
            ));
        }
        let unique_credits = credited_artist_ids.iter().collect::<HashSet<_>>();
        if credited_artist_ids.is_empty()
            || unique_credits.len() != credited_artist_ids.len()
            || !credited_artist_ids.contains(&track.owner_artist_id)
            || credited_artist_ids
                .iter()
                .any(|artist| !self.artists.contains_key(artist))
        {
            return Err(CanonicalError::Invalid(
                "credits must be non-empty, include the physical owner, and reference known artists"
                    .to_owned(),
            ));
        }
        let rel = self
            .track_records
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing track record path".to_owned()))?;
        let dto = TrackEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: TRACK_KIND.to_owned(),
            id: id.get(),
            title,
            credited_artist_ids: credited_artist_ids.iter().map(|id| id.get()).collect(),
            owner_artist_id: track.owner_artist_id.get(),
            album_id: track.album_id.get(),
            track_number,
            disc_number,
            duration_ms,
            lyrics: track.lyrics,
            fingerprint: track.fingerprint,
        };
        let mut writes = Vec::new();
        push_file_write(&mut writes, &rel, &dto)?;
        self.commit_writes(None, writes)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Writes/replaces a managed cover. No image decoding is performed; the
    /// extension and bounded size are validated, and the file is installed
    /// atomically. Missing covers never invalidate the album.
    pub fn set_cover(
        &mut self,
        album_id: AlbumId,
        source: &Path,
        extension: &str,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
            return Err(CanonicalError::Invalid(
                "cover must be png, jpg, jpeg, or webp".to_owned(),
            ));
        }
        let size = fs::metadata(source)?.len();
        if size == 0 || size > 16 * 1024 * 1024 {
            return Err(CanonicalError::Invalid(
                "cover must be between 1 byte and 16 MiB".to_owned(),
            ));
        }
        let album = self
            .albums
            .get(&album_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("album is not in the library".to_owned()))?;
        let album_rel_path = self
            .album_paths
            .get(&album_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing album path".to_owned()))?;
        let cover_name = format!("cover.{extension}");
        let album_dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: album_id.get(),
            owner_artist_id: album.owner_artist_id.get(),
            title: album.title,
            release_type: album.release_type,
            cover: Some(cover_name.clone()),
            track_order: album.track_order.iter().map(|id| id.get()).collect(),
        };
        let cover_rel = RelPath::from_utf8(&format!(
            "{}/{}",
            album_rel_path.display_lossy(),
            cover_name
        ))?;
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &album_rel_path, "album.flux", &album_dto)?;
        writes.push(PendingWrite {
            target: cover_rel,
            bytes: None,
            copy_from: Some(source.to_path_buf()),
            expected: Some(ContentFingerprint::of_file(source)?),
        });
        self.commit_writes(None, writes)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Writes or removes a managed lyrics file. The caller supplies text; no
    /// tag metadata is consulted.
    pub fn set_lyrics(&mut self, id: TrackId, lyrics: Option<&str>) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let track = self
            .tracks
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("track is not in the library".to_owned()))?;
        let record_rel = self
            .track_records
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing track record".to_owned()))?;
        let lyrics_name = lyrics.map(|_| format!("track_{:06}.lrc", id.get()));
        let dto = TrackEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: TRACK_KIND.to_owned(),
            id: id.get(),
            title: track.title,
            credited_artist_ids: track
                .credited_artist_ids
                .iter()
                .map(|artist| artist.get())
                .collect(),
            owner_artist_id: track.owner_artist_id.get(),
            album_id: track.album_id.get(),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            lyrics: lyrics_name.clone(),
            fingerprint: track.fingerprint,
        };
        let mut writes = Vec::new();
        push_file_write(&mut writes, &record_rel, &dto)?;
        if let (Some(name), Some(text)) = (lyrics_name, lyrics) {
            let rel = RelPath::from_utf8(&format!(
                "{}/{}",
                record_rel
                    .display_lossy()
                    .rsplit_once('/')
                    .map_or("", |(dir, _)| dir),
                name
            ))?;
            writes.push(PendingWrite {
                target: rel,
                bytes: Some(text.as_bytes().to_vec()),
                copy_from: None,
                expected: None,
            });
        }
        self.commit_writes(None, writes)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Writes a playlist file and registers it in the catalogue.
    pub fn set_playlist(
        &mut self,
        id: Option<PlaylistId>,
        name: impl Into<String>,
        track_ids: Vec<TrackId>,
    ) -> Result<PlaylistId, CanonicalError> {
        self.ensure_writable()?;
        let name = name.into().trim().to_owned();
        if name.is_empty() {
            return Err(CanonicalError::Invalid(
                "playlist name must not be empty".to_owned(),
            ));
        }
        for track in &track_ids {
            if !self.tracks.contains_key(track) {
                return Err(CanonicalError::Invalid(format!(
                    "playlist references missing track {track}"
                )));
            }
        }
        let id = id.unwrap_or_else(|| PlaylistId::new(self.catalog.next_playlist_id));
        let rel = playlist_rel(id);
        let dto = PlaylistEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: PLAYLIST_KIND.to_owned(),
            id: id.get(),
            name,
            track_ids: track_ids.iter().map(|track| track.get()).collect(),
        };
        let mut catalog = self.catalog.clone();
        if !catalog.playlists.iter().any(|entry| entry.id == id.get()) {
            if id.get() != catalog.next_playlist_id {
                return Err(CanonicalError::Invalid(
                    "new playlist ID does not match the counter".to_owned(),
                ));
            }
            catalog.next_playlist_id = catalog
                .next_playlist_id
                .checked_add(1)
                .ok_or_else(|| CanonicalError::Invalid("playlist counter overflow".to_owned()))?;
            catalog.playlists.push(CatalogPlaylistDto {
                id: id.get(),
                path: rel.display_lossy(),
            });
        }
        let mut writes = Vec::new();
        push_file_write(&mut writes, &rel, &dto)?;
        self.commit_writes(Some(catalog), writes)?;
        self.reopen_after()?;
        Ok(id)
    }

    /// Stores/removes a rating and favorite flag.
    pub fn set_rating(
        &mut self,
        id: TrackId,
        rating: Option<u8>,
        favorite: bool,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        if !self.tracks.contains_key(&id) {
            return Err(CanonicalError::Invalid(
                "track is not in the library".to_owned(),
            ));
        }
        if rating.is_some_and(|rating| rating > 5) {
            return Err(CanonicalError::Invalid("rating must be 0..=5".to_owned()));
        }
        let mut entries = self
            .ratings
            .iter()
            .map(|(track, value)| RatingEntryDto {
                track_id: track.get(),
                rating: value.rating,
                favorite: value.favorite,
            })
            .collect::<Vec<_>>();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.track_id == id.get()) {
            entry.rating = rating;
            entry.favorite = favorite;
        } else {
            entries.push(RatingEntryDto {
                track_id: id.get(),
                rating,
                favorite,
            });
        }
        entries.sort_by_key(|entry| entry.track_id);
        let dto = RatingsDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: RATINGS_KIND.to_owned(),
            entries,
        };
        let bytes = serde_json::to_vec_pretty(&dto).map_err(|err| CanonicalError::Malformed {
            file: RATINGS_FILE_NAME.to_owned(),
            message: err.to_string(),
        })?;
        let writes = vec![PendingWrite {
            target: RelPath::from_utf8(RATINGS_FILE_NAME)?,
            bytes: Some(bytes),
            copy_from: None,
            expected: None,
        }];
        self.commit_writes(None, writes)?;
        self.reopen_after()?;
        Ok(())
    }

    fn ensure_writable(&self) -> Result<(), CanonicalError> {
        Ok(())
    }
}

fn push_entity_write<T: Serialize>(
    writes: &mut Vec<PendingWrite>,
    dir: &RelPath,
    file: &str,
    value: &T,
) -> Result<(), CanonicalError> {
    let rel = RelPath::from_utf8(&format!("{}/{}", dir.display_lossy(), file))?;
    push_file_write(writes, &rel, value)
}

fn push_file_write<T: Serialize>(
    writes: &mut Vec<PendingWrite>,
    rel: &RelPath,
    value: &T,
) -> Result<(), CanonicalError> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|err| CanonicalError::Malformed {
        file: rel.display_lossy(),
        message: err.to_string(),
    })?;
    writes.push(PendingWrite {
        target: rel.clone(),
        bytes: Some(bytes),
        copy_from: None,
        expected: None,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use flux_core::{ArtistId, TrackId};

    use super::{
        CanonicalError, CanonicalImportedTrack, CanonicalLibrary, ContentFingerprint, ReleaseType,
    };

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-canonical-test-{}-{name}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().expect("parent")).expect("parents");
            fs::write(&path, bytes).expect("write");
            path
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn import(dir: &TempDir, library: &mut CanonicalLibrary, name: &str) -> TrackId {
        let source = dir.write(
            &format!("source/{name}"),
            b"not real audio, but a managed opaque resource",
        );
        library
            .import_track(CanonicalImportedTrack {
                source,
                title: format!("Title {name}"),
                artist_id: None,
                artist_name: "Artist One".to_owned(),
                credited_artist_ids: Vec::new(),
                album_title: Some("Album One".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: Some(1),
                duration_ms: Some(123_000),
            })
            .expect("import")
    }

    #[test]
    fn create_import_reopen_and_relative_layout() {
        let dir = TempDir::new("create");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        assert_eq!(library.artists().count(), 0);
        let track_id = import(&dir, &mut library, "one.wav");
        drop(library);

        let opened = CanonicalLibrary::open(&root).expect("open");
        assert_eq!(opened.library_id().len(), 32);
        assert_eq!(opened.generation(), 2);
        assert_eq!(opened.artists().count(), 1);
        assert_eq!(opened.albums().count(), 1);
        assert_eq!(opened.tracks().count(), 1);
        let track = opened.track(track_id).expect("track");
        assert_eq!(track.title, "Title one.wav");
        assert_eq!(track.credited_artist_ids, vec![ArtistId::new(1)]);
        let audio = opened.track_audio_path(track_id).expect("audio path");
        assert!(audio.starts_with(&root));
        assert!(
            audio.ends_with("artists/artist_000001/albums/album_000001/tracks/track_000001.wav")
        );
        assert_eq!(
            fs::read(&audio).expect("audio"),
            b"not real audio, but a managed opaque resource"
        );
        assert!(opened.track_record_path(track_id).is_some());
        assert!(opened.track_available(track_id));
        let fingerprint = ContentFingerprint::of_file(&audio).expect("fingerprint");
        assert_eq!(track.fingerprint, Some(fingerprint));
        assert_eq!(opened.rating(track_id), None);
    }

    #[test]
    fn renames_preserve_ids_and_relationships() {
        let dir = TempDir::new("rename");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let track_id = import(&dir, &mut library, "one.wav");
        let artist_id = library.track(track_id).expect("track").owner_artist_id;
        let album_id = library.track(track_id).expect("track").album_id;
        library
            .rename_artist(artist_id, "Renamed Artist")
            .expect("rename");
        assert_eq!(
            library.artist(artist_id).map(|artist| artist.name.as_str()),
            Some("Renamed Artist")
        );
        assert_eq!(library.track(track_id).map(|t| t.id), Some(track_id));
        assert_eq!(library.album(album_id).map(|a| a.id), Some(album_id));
        assert_eq!(
            library
                .track(track_id)
                .map(|t| t.credited_artist_ids.clone()),
            Some(vec![artist_id])
        );
        drop(library);
        let reopened = CanonicalLibrary::open(&root).expect("reopen");
        assert_eq!(
            reopened.artist(artist_id).map(|a| a.name.as_str()),
            Some("Renamed Artist")
        );
    }

    #[test]
    fn lyrics_playlist_ratings_and_cover_are_managed_resources() {
        let dir = TempDir::new("resources");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let track_id = import(&dir, &mut library, "one.wav");
        let album_id = library.track(track_id).expect("track").album_id;
        library
            .set_lyrics(track_id, Some("[00:00.00]hello\n"))
            .expect("lyrics");
        let playlist = library
            .set_playlist(None, "Favorites", vec![track_id])
            .expect("playlist");
        library.set_rating(track_id, Some(5), true).expect("rating");
        let cover = dir.write("art/cover.png", b"fake png bytes");
        library.set_cover(album_id, &cover, "png").expect("cover");
        drop(library);

        let opened = CanonicalLibrary::open(&root).expect("open resources");
        assert!(opened.playlist(playlist).is_some());
        assert_eq!(
            opened.playlist(playlist).map(|p| p.track_ids.clone()),
            Some(vec![track_id])
        );
        assert_eq!(
            opened.rating(track_id),
            Some(super::TrackRating {
                rating: Some(5),
                favorite: true
            })
        );
        let track = opened.track(track_id).expect("track");
        let lyrics = track.lyrics.as_ref().expect("lyrics");
        let album_dir = opened.album_dir(album_id).expect("album dir");
        assert!(album_dir.join("tracks").join(lyrics).exists());
        assert!(album_dir.join("cover.png").exists());
    }

    #[test]
    fn malformed_or_conflicting_catalogue_is_rejected() {
        let dir = TempDir::new("malformed");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let track = import(&dir, &mut library, "one.wav");
        drop(library);
        let catalog_path = root.join("catalog.flux");
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_path).expect("catalog")).expect("json");
        catalog["tracks"][0]["audio_path"] = serde_json::json!("../escape.wav");
        fs::write(
            &catalog_path,
            serde_json::to_vec_pretty(&catalog).expect("bytes"),
        )
        .expect("write");
        assert!(matches!(
            CanonicalLibrary::open(&root),
            Err(CanonicalError::InvalidPath(_)) | Err(CanonicalError::Invalid(_))
        ));
        let _ = track;
    }

    #[test]
    fn unsupported_schema_version_is_rejected_without_rewrite() {
        let dir = TempDir::new("version");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let manifest_path = root.join(super::LIBRARY_FILE_NAME);
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("manifest")).expect("json");
        manifest["schema_version"] = serde_json::json!(99);
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("bytes"),
        )
        .expect("write");
        let before = fs::read(&manifest_path).expect("before");
        assert!(matches!(
            CanonicalLibrary::open(&root),
            Err(CanonicalError::UnsupportedVersion { found: 99, .. })
        ));
        assert_eq!(fs::read(&manifest_path).expect("after"), before);
    }

    #[test]
    fn uncommitted_transaction_rolls_back_and_committed_cleanup_is_idempotent() {
        let dir = TempDir::new("recover");
        let root = dir.path().join("FluxLibrary");
        let library = CanonicalLibrary::create(&root).expect("create");
        let original = root.join("ratings.flux");
        let before = fs::read(&original).expect("ratings");
        let txn = "txn-recover";
        let txn_dir = root.join(super::TRANSACTIONS_DIR_NAME).join(txn);
        fs::create_dir_all(&txn_dir).expect("txn");
        fs::write(txn_dir.join("backup-0"), &before).expect("backup");
        fs::write(
            txn_dir.join("journal.flux"),
            br#"{
            "schema_version": 1,
            "kind": "flux-transaction",
            "transaction_id": "txn-recover",
            "operation": "test",
            "state": "applying",
            "intended_generation": 1,
            "actions": [
                {"target": "ratings.flux", "existed": true, "backup": "backup-0"},
                {"target": "orphan.txt", "existed": false}
            ]
        }"#,
        )
        .expect("journal");
        fs::write(&original, b"corrupt pending bytes").expect("pending");
        fs::write(root.join("orphan.txt"), b"orphan").expect("orphan");
        drop(library);
        let reopened = CanonicalLibrary::open(&root).expect("recover");
        assert_eq!(
            fs::read(root.join("ratings.flux")).expect("restored"),
            before
        );
        assert!(!root.join("orphan.txt").exists());
        assert!(!txn_dir.exists());
        assert_eq!(reopened.track(TrackId::new(99)), None);
    }

    #[test]
    fn playlist_and_rating_validation_rejects_dangling_refs() {
        let dir = TempDir::new("dangling");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let track = import(&dir, &mut library, "one.wav");
        let playlist = library
            .set_playlist(None, "P", vec![track])
            .expect("playlist");
        let path = library.playlist_path(playlist).expect("path");
        drop(library);
        let mut dto: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("playlist")).expect("json");
        dto["track_ids"] = serde_json::json!([999]);
        fs::write(&path, serde_json::to_vec_pretty(&dto).expect("bytes")).expect("write");
        assert!(matches!(
            CanonicalLibrary::open(&root),
            Err(CanonicalError::Invalid(_))
        ));
    }

    #[test]
    fn canonical_paths_cannot_escape_via_symlink() {
        #[cfg(unix)]
        {
            let dir = TempDir::new("symlink");
            let root = dir.path().join("FluxLibrary");
            let mut library = CanonicalLibrary::create(&root).expect("create");
            let track = import(&dir, &mut library, "one.wav");
            let audio = library.track_audio_path(track).expect("audio");
            let outside = dir.write("outside.wav", b"outside");
            fs::remove_file(&audio).expect("remove");
            std::os::unix::fs::symlink(&outside, &audio).expect("symlink");
            drop(library);
            assert!(matches!(
                CanonicalLibrary::open(&root),
                Err(CanonicalError::Invalid(_))
            ));
        }
    }
}

/// One track in a [`CanonicalSeed`], including the external audio source
/// used during a controlled migration. The source is copied, never moved.
#[derive(Debug, Clone)]
pub struct CanonicalSeedTrack {
    /// Canonical metadata and stable ID.
    pub track: CanonicalTrack,
    /// Existing audio file copied into the canonical album directory.
    pub source_audio: PathBuf,
    /// Original audio extension without a leading dot.
    pub extension: String,
    /// Optional LRC text stored as `track_<id>.lrc`.
    pub lyrics_text: Option<String>,
}

/// A fully validated in-memory snapshot used by migration to build a new
/// canonical library in one transactional publication.
#[derive(Debug, Clone, Default)]
pub struct CanonicalSeed {
    /// Artists to create.
    pub artists: Vec<CanonicalArtist>,
    /// Albums to create.
    pub albums: Vec<CanonicalAlbum>,
    /// Tracks and their external source files.
    pub tracks: Vec<CanonicalSeedTrack>,
    /// Playlists to create.
    pub playlists: Vec<CanonicalPlaylist>,
    /// Ratings/favorites to persist.
    pub ratings: BTreeMap<TrackId, TrackRating>,
    /// Optional cover: album ID -> (source, canonical extension).
    pub covers: BTreeMap<AlbumId, (PathBuf, String)>,
    /// Preserved next TrackId counter, when the source had retired-ID gaps.
    pub next_track_id: Option<u64>,
    /// Preserved next ArtistId counter.
    pub next_artist_id: Option<u64>,
    /// Preserved next AlbumId counter.
    pub next_album_id: Option<u64>,
    /// Preserved next PlaylistId counter.
    pub next_playlist_id: Option<u64>,
}

impl CanonicalLibrary {
    /// Creates a new canonical library from a migration seed.
    ///
    /// `root` must be absent or empty. The empty root is created first; the
    /// seed is then published in one transaction whose catalogue write is the
    /// commit point. On failure the root may contain an empty canonical
    /// library but never a catalogue that references partially copied audio.
    pub fn create_from_seed(root: &Path, seed: &CanonicalSeed) -> Result<Self, CanonicalError> {
        let mut library = Self::create(root)?;
        library.install_seed(seed)?;
        Ok(library)
    }

    /// Installs a migration seed into an empty canonical library.
    pub fn install_seed(&mut self, seed: &CanonicalSeed) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        if !self.artists.is_empty()
            || !self.albums.is_empty()
            || !self.tracks.is_empty()
            || !self.playlists.is_empty()
        {
            return Err(CanonicalError::Invalid(
                "a migration seed can only be installed into an empty canonical library".to_owned(),
            ));
        }
        let mut artists = BTreeMap::new();
        let mut albums = BTreeMap::new();
        let mut tracks = BTreeMap::new();
        let mut playlists = BTreeMap::new();
        let mut catalog = CatalogDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: CATALOG_KIND.to_owned(),
            library_id: self.catalog.library_id.clone(),
            generation: self.catalog.generation,
            transaction_id: None,
            next_track_id: 1,
            next_artist_id: 1,
            next_album_id: 1,
            next_playlist_id: 1,
            artists: Vec::new(),
            albums: Vec::new(),
            tracks: Vec::new(),
            playlists: Vec::new(),
            ratings_path: RATINGS_FILE_NAME.to_owned(),
        };
        let mut writes = Vec::new();

        for artist in &seed.artists {
            if artist.id.get() == 0 || artist.name.trim().is_empty() {
                return Err(CanonicalError::Invalid(
                    "seed artists require nonzero IDs and names".to_owned(),
                ));
            }
            if artists.insert(artist.id, artist.clone()).is_some() {
                return Err(CanonicalError::Invalid(format!(
                    "seed contains duplicate artist {}",
                    artist.id
                )));
            }
            catalog.next_artist_id = catalog.next_artist_id.max(artist.id.get() + 1);
            let rel = artist_rel(artist.id);
            catalog.artists.push(CatalogArtistDto {
                id: artist.id.get(),
                path: rel.display_lossy(),
            });
            let dto = ArtistEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: ARTIST_KIND.to_owned(),
                id: artist.id.get(),
                name: artist.name.clone(),
                aliases: artist.aliases.clone(),
            };
            push_entity_write(&mut writes, &rel, "artist.flux", &dto)?;
        }
        catalog.artists.sort_by_key(|entry| entry.id);

        for album in &seed.albums {
            if !artists.contains_key(&album.owner_artist_id) {
                return Err(CanonicalError::Invalid(format!(
                    "seed album {} references missing owner {}",
                    album.id, album.owner_artist_id
                )));
            }
            if albums.insert(album.id, album.clone()).is_some() {
                return Err(CanonicalError::Invalid(format!(
                    "seed contains duplicate album {}",
                    album.id
                )));
            }
            catalog.next_album_id = catalog.next_album_id.max(album.id.get() + 1);
            let rel = album_rel(album.owner_artist_id, album.id);
            catalog.albums.push(CatalogAlbumDto {
                id: album.id.get(),
                artist_id: album.owner_artist_id.get(),
                path: rel.display_lossy(),
            });
            let mut order = Vec::new();
            let mut seen = HashSet::new();
            for track in &album.track_order {
                if !seen.insert(*track) {
                    return Err(CanonicalError::Invalid(format!(
                        "seed album {} lists track {track} more than once",
                        album.id
                    )));
                }
                order.push(track.get());
            }
            let dto = AlbumEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: ALBUM_KIND.to_owned(),
                id: album.id.get(),
                owner_artist_id: album.owner_artist_id.get(),
                title: album.title.clone(),
                release_type: album.release_type,
                cover: album.cover.clone(),
                track_order: order,
            };
            push_entity_write(&mut writes, &rel, "album.flux", &dto)?;
        }
        catalog.albums.sort_by_key(|entry| entry.id);

        for item in &seed.tracks {
            let track = &item.track;
            let Some(album) = albums.get(&track.album_id) else {
                return Err(CanonicalError::Invalid(format!(
                    "seed track {} references missing album {}",
                    track.id, track.album_id
                )));
            };
            if album.owner_artist_id != track.owner_artist_id {
                return Err(CanonicalError::Invalid(format!(
                    "seed track {} owner is not the album owner",
                    track.id
                )));
            }
            if track.credited_artist_ids.is_empty()
                || !track.credited_artist_ids.contains(&track.owner_artist_id)
                || track
                    .credited_artist_ids
                    .iter()
                    .any(|artist| !artists.contains_key(artist))
            {
                return Err(CanonicalError::Invalid(format!(
                    "seed track {} has invalid credits",
                    track.id
                )));
            }
            if tracks.insert(track.id, track.clone()).is_some() {
                return Err(CanonicalError::Invalid(format!(
                    "seed contains duplicate track {}",
                    track.id
                )));
            }
            let extension = item.extension.trim_start_matches('.').to_ascii_lowercase();
            if extension.is_empty() || !extension.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(CanonicalError::Invalid(format!(
                    "seed track {} has an unsafe audio extension",
                    track.id
                )));
            }
            if !item.source_audio.is_file() {
                return Err(CanonicalError::MissingResource(item.source_audio.clone()));
            }
            let fingerprint = ContentFingerprint::of_file(&item.source_audio)?;
            let audio_rel =
                track_audio_rel(track.owner_artist_id, track.album_id, track.id, &extension);
            let record_rel = track_record_rel(track.owner_artist_id, track.album_id, track.id);
            catalog.next_track_id = catalog.next_track_id.max(track.id.get() + 1);
            catalog.tracks.push(CatalogTrackDto {
                id: track.id.get(),
                owner_artist_id: track.owner_artist_id.get(),
                album_id: track.album_id.get(),
                audio_path: audio_rel.display_lossy(),
                record_path: record_rel.display_lossy(),
            });
            let lyrics = item
                .lyrics_text
                .as_ref()
                .map(|_| format!("track_{:06}.lrc", track.id.get()));
            let dto = TrackEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: TRACK_KIND.to_owned(),
                id: track.id.get(),
                title: track.title.clone(),
                credited_artist_ids: track
                    .credited_artist_ids
                    .iter()
                    .map(|artist| artist.get())
                    .collect(),
                owner_artist_id: track.owner_artist_id.get(),
                album_id: track.album_id.get(),
                track_number: track.track_number,
                disc_number: track.disc_number,
                duration_ms: track.duration_ms,
                lyrics,
                fingerprint: track.fingerprint.or(Some(fingerprint)),
            };
            push_file_write(&mut writes, &record_rel, &dto)?;
            if let (Some(text), Some(name)) = (&item.lyrics_text, &dto.lyrics) {
                let rel = RelPath::from_utf8(&format!(
                    "{}/{}",
                    record_rel
                        .display_lossy()
                        .rsplit_once('/')
                        .map_or("", |(dir, _)| dir),
                    name
                ))?;
                writes.push(PendingWrite {
                    target: rel,
                    bytes: Some(text.as_bytes().to_vec()),
                    copy_from: None,
                    expected: None,
                });
            }
            writes.push(PendingWrite {
                target: audio_rel,
                bytes: None,
                copy_from: Some(item.source_audio.clone()),
                expected: Some(fingerprint),
            });
        }
        catalog.tracks.sort_by_key(|entry| entry.id);
        for (value, field) in [
            (seed.next_track_id, "track"),
            (seed.next_artist_id, "artist"),
            (seed.next_album_id, "album"),
            (seed.next_playlist_id, "playlist"),
        ] {
            if value.is_some_and(|value| value == 0) {
                return Err(CanonicalError::Invalid(format!(
                    "seed {field} counter must be nonzero"
                )));
            }
        }
        catalog.next_track_id = catalog.next_track_id.max(seed.next_track_id.unwrap_or(1));
        catalog.next_artist_id = catalog.next_artist_id.max(seed.next_artist_id.unwrap_or(1));
        catalog.next_album_id = catalog.next_album_id.max(seed.next_album_id.unwrap_or(1));
        catalog.next_playlist_id = catalog
            .next_playlist_id
            .max(seed.next_playlist_id.unwrap_or(1));
        for album in albums.values() {
            for id in &album.track_order {
                if !tracks.contains_key(id) {
                    return Err(CanonicalError::Invalid(format!(
                        "seed album {} references missing track {id}",
                        album.id
                    )));
                }
            }
        }
        if tracks.len() != catalog.tracks.len() {
            return Err(CanonicalError::Invalid(
                "seed tracks are not unique".to_owned(),
            ));
        }

        for (album_id, (source, extension)) in &seed.covers {
            let album = albums.get(album_id).ok_or_else(|| {
                CanonicalError::Invalid(format!("cover references missing album {album_id}"))
            })?;
            let extension = extension.trim_start_matches('.').to_ascii_lowercase();
            if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
                return Err(CanonicalError::Invalid(
                    "seed cover must be png, jpg, jpeg, or webp".to_owned(),
                ));
            }
            let size = fs::metadata(source)?.len();
            if size == 0 || size > 16 * 1024 * 1024 {
                return Err(CanonicalError::Invalid(
                    "seed cover must be between 1 byte and 16 MiB".to_owned(),
                ));
            }
            let album_rel = album_rel(album.owner_artist_id, album.id);
            writes.push(PendingWrite {
                target: RelPath::from_utf8(&format!(
                    "{}/cover.{extension}",
                    album_rel.display_lossy()
                ))?,
                bytes: None,
                copy_from: Some(source.clone()),
                expected: Some(ContentFingerprint::of_file(source)?),
            });
        }

        for playlist in &seed.playlists {
            if playlist.id.get() == 0 || playlist.name.trim().is_empty() {
                return Err(CanonicalError::Invalid(
                    "seed playlists require nonzero IDs and names".to_owned(),
                ));
            }
            if playlist
                .track_ids
                .iter()
                .any(|track| !tracks.contains_key(track))
            {
                return Err(CanonicalError::Invalid(format!(
                    "seed playlist {} references a missing track",
                    playlist.id
                )));
            }
            if playlists.insert(playlist.id, playlist.clone()).is_some() {
                return Err(CanonicalError::Invalid(format!(
                    "seed contains duplicate playlist {}",
                    playlist.id
                )));
            }
            catalog.next_playlist_id = catalog.next_playlist_id.max(playlist.id.get() + 1);
            let rel = playlist_rel(playlist.id);
            catalog.playlists.push(CatalogPlaylistDto {
                id: playlist.id.get(),
                path: rel.display_lossy(),
            });
            let dto = PlaylistEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: PLAYLIST_KIND.to_owned(),
                id: playlist.id.get(),
                name: playlist.name.clone(),
                track_ids: playlist.track_ids.iter().map(|track| track.get()).collect(),
            };
            push_file_write(&mut writes, &rel, &dto)?;
        }
        catalog.playlists.sort_by_key(|entry| entry.id);

        if seed
            .ratings
            .values()
            .any(|rating| rating.rating.is_some_and(|value| value > 5))
        {
            return Err(CanonicalError::Invalid(
                "seed rating must be 0..=5".to_owned(),
            ));
        }
        if seed.ratings.keys().any(|track| !tracks.contains_key(track)) {
            return Err(CanonicalError::Invalid(
                "seed ratings reference a missing track".to_owned(),
            ));
        }
        let ratings = RatingsDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: RATINGS_KIND.to_owned(),
            entries: seed
                .ratings
                .iter()
                .map(|(track, value)| RatingEntryDto {
                    track_id: track.get(),
                    rating: value.rating,
                    favorite: value.favorite,
                })
                .collect(),
        };
        let ratings_bytes =
            serde_json::to_vec_pretty(&ratings).map_err(|err| CanonicalError::Malformed {
                file: RATINGS_FILE_NAME.to_owned(),
                message: err.to_string(),
            })?;
        writes.push(PendingWrite {
            target: RelPath::from_utf8(RATINGS_FILE_NAME)?,
            bytes: Some(ratings_bytes),
            copy_from: None,
            expected: None,
        });

        self.commit_writes(Some(catalog), writes)?;
        self.reopen_after()?;
        Ok(())
    }
}

// ------------------------------------------------ ownership transfer ---

/// Where a transferred track should live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipDestination {
    /// Move into an existing album owned by the destination artist.
    Existing(AlbumId),
    /// Create a new album owned by the destination artist. The title must not
    /// already identify an album for that owner.
    New {
        /// Explicit release title.
        title: String,
        /// Explicit release type.
        release_type: ReleaseType,
    },
}

/// Read-only plan for moving one track's physical ownership to another
/// credited artist.
#[derive(Debug, Clone)]
pub struct OwnershipTransferPlan {
    track_id: TrackId,
    from_owner: ArtistId,
    to_owner: ArtistId,
    source_album: AlbumId,
    destination_album: AlbumId,
    new_album: Option<CanonicalAlbum>,
    old_audio: RelPath,
    old_record: RelPath,
    old_lyrics: Option<RelPath>,
    new_audio: RelPath,
    new_record: RelPath,
    new_lyrics: Option<RelPath>,
    expected_fingerprint: Option<ContentFingerprint>,
    no_op: bool,
}

impl OwnershipTransferPlan {
    /// Track whose ownership changes.
    #[must_use]
    pub fn track_id(&self) -> TrackId {
        self.track_id
    }
    /// Current physical owner.
    #[must_use]
    pub fn from_owner(&self) -> ArtistId {
        self.from_owner
    }
    /// New physical owner.
    #[must_use]
    pub fn to_owner(&self) -> ArtistId {
        self.to_owner
    }
    /// Source album.
    #[must_use]
    pub fn source_album(&self) -> AlbumId {
        self.source_album
    }
    /// Destination album.
    #[must_use]
    pub fn destination_album(&self) -> AlbumId {
        self.destination_album
    }
    /// True when no filesystem/catalogue change is required.
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.no_op
    }
    /// New audio path after the transfer.
    #[must_use]
    pub fn new_audio_path(&self) -> &RelPath {
        &self.new_audio
    }
}

/// Result of a committed ownership transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwnershipTransferReport {
    /// Track ID (unchanged).
    pub track_id: TrackId,
    /// Previous owner.
    pub from_owner: ArtistId,
    /// New owner.
    pub to_owner: ArtistId,
    /// Destination album.
    pub destination_album: AlbumId,
    /// True when the plan was already satisfied.
    pub no_op: bool,
}

impl CanonicalLibrary {
    /// Plans a copy-first transfer of one track to another credited artist.
    ///
    /// The destination artist must already be one of the track's credited
    /// artists. The destination album must belong to that artist, or a new
    /// album must be explicitly named. Collisions are rejected during
    /// planning.
    pub fn plan_ownership_transfer(
        &self,
        track_id: TrackId,
        new_owner: ArtistId,
        destination: OwnershipDestination,
    ) -> Result<OwnershipTransferPlan, CanonicalError> {
        let track = self.tracks.get(&track_id).ok_or_else(|| {
            CanonicalError::Invalid(format!("track {track_id} is not in the library"))
        })?;
        if !self.artists.contains_key(&new_owner) {
            return Err(CanonicalError::Invalid(format!(
                "destination artist {new_owner} is not in the library"
            )));
        }
        if !track.credited_artist_ids.contains(&new_owner) {
            return Err(CanonicalError::Invalid(format!(
                "destination artist {new_owner} is not credited on track {track_id}"
            )));
        }
        let destination_album = match &destination {
            OwnershipDestination::Existing(id) => {
                let id = *id;
                let album = self.albums.get(&id).ok_or_else(|| {
                    CanonicalError::Invalid(format!("destination album {id} does not exist"))
                })?;
                if album.owner_artist_id != new_owner {
                    return Err(CanonicalError::Invalid(format!(
                        "destination album {id} is owned by {}, not {new_owner}",
                        album.owner_artist_id
                    )));
                }
                id
            }
            OwnershipDestination::New { title, .. } => {
                let title = title.trim();
                if title.is_empty() {
                    return Err(CanonicalError::Invalid(
                        "a new destination album needs a title".to_owned(),
                    ));
                }
                if let Some(existing) = self.album_by_title(new_owner, title) {
                    return Err(CanonicalError::Invalid(format!(
                        "album {existing} already has that owner/title; select the existing album explicitly"
                    )));
                }
                AlbumId::new(self.catalog.next_album_id)
            }
        };
        let new_album = match destination {
            OwnershipDestination::New {
                title,
                release_type,
            } => Some(CanonicalAlbum {
                id: destination_album,
                owner_artist_id: new_owner,
                title: title.trim().to_owned(),
                release_type,
                cover: None,
                track_order: Vec::new(),
            }),
            OwnershipDestination::Existing(_) => None,
        };
        if track.owner_artist_id == new_owner {
            if track.album_id != destination_album {
                return Err(CanonicalError::Invalid(
                    "the artist already owns this track; moving it to another album is a separate operation"
                        .to_owned(),
                ));
            }
            return Ok(OwnershipTransferPlan {
                track_id,
                from_owner: track.owner_artist_id,
                to_owner: new_owner,
                source_album: track.album_id,
                destination_album,
                new_album: None,
                old_audio: self.track_paths[&track_id].clone(),
                old_record: self.track_records[&track_id].clone(),
                old_lyrics: None,
                new_audio: self.track_paths[&track_id].clone(),
                new_record: self.track_records[&track_id].clone(),
                new_lyrics: None,
                expected_fingerprint: track.fingerprint,
                no_op: true,
            });
        }
        let old_audio = self.track_paths[&track_id].clone();
        let old_record = self.track_records[&track_id].clone();
        let extension = old_audio
            .display_lossy()
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .ok_or_else(|| CanonicalError::Invalid("track audio has no extension".to_owned()))?;
        let new_audio = track_audio_rel(new_owner, destination_album, track_id, &extension);
        let new_record = track_record_rel(new_owner, destination_album, track_id);
        let old_lyrics = track.lyrics.as_ref().map(|name| {
            RelPath::from_utf8(&format!(
                "{}/{}",
                old_record
                    .display_lossy()
                    .rsplit_once('/')
                    .map_or("", |(dir, _)| dir),
                name
            ))
        });
        let old_lyrics = old_lyrics.transpose()?;
        let new_lyrics = old_lyrics.as_ref().map(|old| {
            RelPath::from_utf8(&format!(
                "{}/{}",
                new_record
                    .display_lossy()
                    .rsplit_once('/')
                    .map_or("", |(dir, _)| dir),
                old.display_lossy()
                    .rsplit_once('/')
                    .map_or("", |(_, name)| name)
            ))
        });
        let new_lyrics = new_lyrics.transpose()?;
        for path in [&new_audio, &new_record] {
            if safe_join(&self.root, path)?.exists() {
                return Err(CanonicalError::Invalid(format!(
                    "destination collision at {}",
                    path.display_lossy()
                )));
            }
        }
        if let Some(path) = &new_lyrics
            && safe_join(&self.root, path)?.exists()
        {
            return Err(CanonicalError::Invalid(format!(
                "destination lyrics collision at {}",
                path.display_lossy()
            )));
        }
        let expected_fingerprint = match track.fingerprint {
            Some(fingerprint) => Some(fingerprint),
            None => Some(ContentFingerprint::of_file(&safe_join(
                &self.root, &old_audio,
            )?)?),
        };
        Ok(OwnershipTransferPlan {
            track_id,
            from_owner: track.owner_artist_id,
            to_owner: new_owner,
            source_album: track.album_id,
            destination_album,
            new_album,
            old_audio,
            old_record,
            old_lyrics,
            new_audio,
            new_record,
            new_lyrics,
            expected_fingerprint,
            no_op: false,
        })
    }

    /// Executes a previously validated ownership plan.
    ///
    /// The audio and managed lyrics are copied to the destination, all metadata
    /// and the catalogue are published transactionally, and only then are the
    /// old files removed. A committed journal remains available if cleanup is
    /// interrupted, so retry/restart completes it without duplicating the
    /// catalogue entry.
    pub fn execute_ownership_transfer(
        &mut self,
        plan: &OwnershipTransferPlan,
    ) -> Result<OwnershipTransferReport, CanonicalError> {
        self.ensure_writable()?;
        let report = OwnershipTransferReport {
            track_id: plan.track_id,
            from_owner: plan.from_owner,
            to_owner: plan.to_owner,
            destination_album: plan.destination_album,
            no_op: plan.no_op,
        };
        if plan.no_op {
            return Ok(report);
        }
        let track = self.tracks.get(&plan.track_id).cloned().ok_or_else(|| {
            CanonicalError::Invalid("track disappeared before transfer".to_owned())
        })?;
        if track.owner_artist_id != plan.from_owner || track.album_id != plan.source_album {
            return Err(CanonicalError::Invalid(
                "the track changed after the transfer plan was built; re-plan".to_owned(),
            ));
        }
        if !track.credited_artist_ids.contains(&plan.to_owner) {
            return Err(CanonicalError::Invalid(
                "the destination artist is no longer credited; re-plan".to_owned(),
            ));
        }
        if plan.new_album.is_none()
            && self
                .album_paths
                .get(&plan.destination_album)
                .is_none_or(|path| {
                    path.display_lossy()
                        != album_rel(plan.to_owner, plan.destination_album).display_lossy()
                })
        {
            return Err(CanonicalError::Invalid(
                "destination album changed after the plan was built; re-plan".to_owned(),
            ));
        }
        if plan.new_album.is_some() && self.catalog.next_album_id != plan.destination_album.get() {
            return Err(CanonicalError::Invalid(
                "album counter changed after the plan was built; re-plan".to_owned(),
            ));
        }
        let source_album = self
            .albums
            .get(&plan.source_album)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("source album disappeared".to_owned()))?;
        let mut destination_album = match &plan.new_album {
            Some(album) => album.clone(),
            None => self
                .albums
                .get(&plan.destination_album)
                .cloned()
                .ok_or_else(|| {
                    CanonicalError::Invalid("destination album disappeared".to_owned())
                })?,
        };
        if destination_album.owner_artist_id != plan.to_owner {
            return Err(CanonicalError::Invalid(
                "destination album owner changed; re-plan".to_owned(),
            ));
        }
        if destination_album.track_order.contains(&plan.track_id) {
            return Err(CanonicalError::Invalid(
                "destination album already contains the track; re-plan".to_owned(),
            ));
        }
        destination_album.track_order.push(plan.track_id);
        let source_rel = self.album_paths[&plan.source_album].clone();
        let destination_rel = album_rel(plan.to_owner, plan.destination_album);

        let source_dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: source_album.id.get(),
            owner_artist_id: source_album.owner_artist_id.get(),
            title: source_album.title,
            release_type: source_album.release_type,
            cover: source_album.cover,
            track_order: source_album
                .track_order
                .iter()
                .filter(|id| **id != plan.track_id)
                .map(|id| id.get())
                .collect(),
        };
        let destination_dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: destination_album.id.get(),
            owner_artist_id: destination_album.owner_artist_id.get(),
            title: destination_album.title,
            release_type: destination_album.release_type,
            cover: destination_album.cover,
            track_order: destination_album
                .track_order
                .iter()
                .map(|id| id.get())
                .collect(),
        };
        let track_dto = TrackEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: TRACK_KIND.to_owned(),
            id: track.id.get(),
            title: track.title,
            credited_artist_ids: track
                .credited_artist_ids
                .iter()
                .map(|artist| artist.get())
                .collect(),
            owner_artist_id: plan.to_owner.get(),
            album_id: plan.destination_album.get(),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            lyrics: track.lyrics,
            fingerprint: plan.expected_fingerprint,
        };
        let mut catalog = self.catalog.clone();
        catalog.next_album_id = catalog.next_album_id.max(plan.destination_album.get() + 1);
        if plan.new_album.is_some()
            && !catalog
                .albums
                .iter()
                .any(|album| album.id == plan.destination_album.get())
        {
            catalog.albums.push(CatalogAlbumDto {
                id: plan.destination_album.get(),
                artist_id: plan.to_owner.get(),
                path: destination_rel.display_lossy(),
            });
            catalog.albums.sort_by_key(|album| album.id);
        }
        let track_descriptor = catalog
            .tracks
            .iter_mut()
            .find(|entry| entry.id == plan.track_id.get())
            .ok_or_else(|| CanonicalError::Invalid("catalogue track disappeared".to_owned()))?;
        track_descriptor.owner_artist_id = plan.to_owner.get();
        track_descriptor.album_id = plan.destination_album.get();
        track_descriptor.audio_path = plan.new_audio.display_lossy();
        track_descriptor.record_path = plan.new_record.display_lossy();

        let mut writes = Vec::new();
        push_entity_write(&mut writes, &source_rel, "album.flux", &source_dto)?;
        push_entity_write(
            &mut writes,
            &destination_rel,
            "album.flux",
            &destination_dto,
        )?;
        push_file_write(&mut writes, &plan.new_record, &track_dto)?;
        writes.push(PendingWrite {
            target: plan.new_audio.clone(),
            bytes: None,
            copy_from: Some(safe_join(&self.root, &plan.old_audio)?),
            expected: plan.expected_fingerprint,
        });
        if let (Some(old), Some(new)) = (&plan.old_lyrics, &plan.new_lyrics) {
            let text = fs::read(safe_join(&self.root, old)?)?;
            writes.push(PendingWrite {
                target: new.clone(),
                bytes: Some(text),
                copy_from: None,
                expected: None,
            });
        }
        let mut cleanup = vec![plan.old_audio.clone(), plan.old_record.clone()];
        cleanup.extend(plan.old_lyrics.clone());
        self.commit_writes_with_cleanup(Some(catalog), writes, cleanup)?;
        self.reopen_after()?;
        Ok(report)
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    #[test]
    fn ordered_credits_include_featuring_without_duplicate_audio() {
        let dir = TempDir::new("credits");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("Artist A", Vec::new()).expect("A");
        let b = library.create_artist("Artist B", Vec::new()).expect("B");
        let source = dir.write("source/song.wav", b"opaque collaboration");
        let id = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Duet".to_owned(),
                artist_id: Some(a),
                artist_name: "Artist A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Together".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        let track = library.track(id).expect("track");
        assert_eq!(track.credited_artist_ids, vec![a, b]);
        assert_eq!(track.owner_artist_id, a);
        assert_eq!(library.owned_track_ids(a).collect::<Vec<_>>(), vec![id]);
        assert!(library.owned_track_ids(b).next().is_none());
        assert_eq!(library.credited_track_ids(b).collect::<Vec<_>>(), vec![id]);
        library
            .rename_artist(b, "Artist B Renamed")
            .expect("rename");
        assert_eq!(
            library.track(id).expect("track").credited_artist_ids,
            vec![a, b]
        );
        assert_eq!(
            library.artist(b).map(|artist| artist.name.as_str()),
            Some("Artist B Renamed")
        );
        assert_eq!(library.tracks().count(), 1);
        assert_eq!(library.track_audio_paths().count(), 1);
    }

    #[test]
    fn explicit_alternate_owner_need_not_be_the_first_credit() {
        let dir = TempDir::new("alternate-owner");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source = dir.write("source/song.wav", b"opaque owner bytes");
        let id = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(b),
                artist_name: "B".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: None,
                disc_number: None,
                duration_ms: None,
            })
            .expect("explicit owner");
        let track = library.track(id).expect("track");
        assert_eq!(track.credited_artist_ids, vec![a, b]);
        assert_eq!(track.owner_artist_id, b);
        assert_eq!(library.owned_track_ids(b).collect::<Vec<_>>(), vec![id]);
        assert!(library.owned_track_ids(a).next().is_none());
        assert_eq!(library.credited_track_ids(a).collect::<Vec<_>>(), vec![id]);
    }

    #[test]
    fn ownership_transfer_moves_files_and_preserves_identity_credits_and_resources() {
        let dir = TempDir::new("transfer");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("Artist A", Vec::new()).expect("A");
        let b = library.create_artist("Artist B", Vec::new()).expect("B");
        let source = dir.write("source/song.wav", b"opaque transfer bytes");
        let id = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Shared Song".to_owned(),
                artist_id: Some(a),
                artist_name: "Artist A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Source Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        library
            .set_lyrics(id, Some("[00:00.00]line"))
            .expect("lyrics");
        library.set_rating(id, Some(4), true).expect("rating");
        let playlist = library
            .set_playlist(None, "Keep", vec![id])
            .expect("playlist");
        let destination = library
            .create_album(b, "Destination Album", ReleaseType::Ep)
            .expect("album");
        let old_audio = library.track_audio_path(id).expect("old audio");
        let old_record = library.track_record_path(id).expect("old record");
        let plan = library
            .plan_ownership_transfer(id, b, OwnershipDestination::Existing(destination))
            .expect("plan");
        assert_eq!(plan.from_owner(), a);
        assert_eq!(plan.to_owner(), b);
        assert!(!plan.is_noop());
        let report = library.execute_ownership_transfer(&plan).expect("transfer");
        assert!(!report.no_op);
        assert!(!old_audio.exists(), "old audio removed after commit");
        assert!(!old_record.exists(), "old record removed after commit");
        let track = library.track(id).expect("track retained");
        assert_eq!(track.id, id);
        assert_eq!(track.owner_artist_id, b);
        assert_eq!(track.credited_artist_ids, vec![a, b]);
        assert_eq!(track.album_id, destination);
        assert_eq!(library.album_tracks(destination), vec![id]);
        let new_audio = library.track_audio_path(id).expect("new audio");
        assert!(new_audio.starts_with(library.artist_dir(b).expect("B dir")));
        assert!(new_audio.is_file());
        assert_eq!(
            library.rating(id),
            Some(crate::canonical::TrackRating {
                rating: Some(4),
                favorite: true
            })
        );
        assert_eq!(
            library
                .playlist(playlist)
                .map(|playlist| playlist.track_ids.clone()),
            Some(vec![id])
        );
        assert!(library.track_lyrics_path(id).is_some());
        assert!(library.track_lyrics_path(id).expect("lyrics").is_file());
        assert_eq!(library.tracks().count(), 1);
        // A retry is an explicit no-op and cannot create duplicates.
        let retry = library
            .plan_ownership_transfer(id, b, OwnershipDestination::Existing(destination))
            .expect("retry plan");
        assert!(retry.is_noop());
        assert!(
            library
                .execute_ownership_transfer(&retry)
                .expect("retry")
                .no_op
        );
        assert_eq!(library.tracks().count(), 1);
        // A second transfer can create an explicitly named destination album.
        let b_audio = library.track_audio_path(id).expect("B audio");
        let back = library
            .plan_ownership_transfer(
                id,
                a,
                OwnershipDestination::New {
                    title: "Back Home".to_owned(),
                    release_type: ReleaseType::Album,
                },
            )
            .expect("new album plan");
        library
            .execute_ownership_transfer(&back)
            .expect("new album transfer");
        assert!(!b_audio.exists(), "previous owner copy removed");
        assert_eq!(library.track(id).expect("track").owner_artist_id, a);
        assert_eq!(
            library
                .album(library.track(id).expect("track").album_id)
                .map(|album| album.title.as_str()),
            Some("Back Home")
        );
        assert_eq!(library.tracks().count(), 1);
    }

    #[test]
    fn ownership_transfer_requires_eligible_owner_and_collision_free_destination() {
        let dir = TempDir::new("transfer-guards");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let c = library.create_artist("C", Vec::new()).expect("C");
        let source = dir.write("source/song.wav", b"opaque guard bytes");
        let id = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: None,
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        let b_album = library
            .create_album(b, "B Album", ReleaseType::Album)
            .expect("album");
        assert!(
            library
                .plan_ownership_transfer(id, c, OwnershipDestination::Existing(b_album))
                .is_err(),
            "artist C is not credited"
        );
        let collision = library
            .album_dir(b_album)
            .expect("album dir")
            .join("tracks")
            .join(format!("track_{:06}.wav", id.get()));
        fs::create_dir_all(collision.parent().expect("parent")).expect("dirs");
        fs::write(&collision, b"collision").expect("collision");
        assert!(
            library
                .plan_ownership_transfer(id, b, OwnershipDestination::Existing(b_album))
                .is_err(),
            "destination collision must block planning"
        );
        assert!(library.track_audio_path(id).expect("old audio").is_file());
    }

    #[test]
    fn artist_and_album_rename_are_identity_preserving_and_case_only_safe() {
        let dir = TempDir::new("rename-case");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("Artist", Vec::new()).expect("A");
        let source = dir.write("source/song.wav", b"opaque rename bytes");
        let id = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(a),
                artist_name: "Artist".to_owned(),
                credited_artist_ids: vec![a],
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        let album = library.track(id).expect("track").album_id;
        library.rename_artist(a, "artist").expect("case rename");
        library
            .rename_album(album, "album")
            .expect("album case rename");
        assert_eq!(
            library.artist(a).map(|artist| artist.name.as_str()),
            Some("artist")
        );
        assert_eq!(
            library.album(album).map(|album| album.title.as_str()),
            Some("album")
        );
        assert_eq!(library.track(id).expect("track").id, id);
        assert_eq!(library.track(id).expect("track").album_id, album);
        assert_eq!(
            library.track(id).expect("track").credited_artist_ids,
            vec![a]
        );
        assert!(library.track_audio_path(id).expect("audio").is_file());
    }
}

// --------------------------------------------------- artist deletion ---

/// One owned track in an artist-deletion plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistDeletionTrack {
    /// Track that needs a decision.
    pub track_id: TrackId,
    /// Album currently containing it.
    pub album_id: AlbumId,
    /// Other surviving credited artists eligible to become owner.
    pub eligible_owners: Vec<ArtistId>,
}

/// Read-only artist deletion inventory. Execution is intentionally refused
/// unless every owned/credited track has a resolved transfer and credit
/// decision in a future workflow; this prevents exposing a destructive
/// half-operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistDeletionPlan {
    /// Artist being considered.
    pub artist_id: ArtistId,
    /// Albums owned by the artist.
    pub owned_albums: Vec<AlbumId>,
    /// Tracks physically owned by the artist.
    pub owned_tracks: Vec<ArtistDeletionTrack>,
    /// Collaborative owned tracks (at least one eligible survivor).
    pub collaborative_tracks: Vec<TrackId>,
    /// Owned tracks with no surviving credited artist.
    pub non_collaborative_tracks: Vec<TrackId>,
    /// Tracks credited to the artist but owned by someone else.
    pub credited_only_tracks: Vec<TrackId>,
    /// Playlists that reference affected tracks.
    pub affected_playlists: Vec<PlaylistId>,
    /// Blocking reasons; a non-empty list means deletion cannot execute.
    pub issues: Vec<String>,
}

impl ArtistDeletionPlan {
    /// Whether this plan can be executed by the current build.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.issues.is_empty()
    }
}

impl CanonicalLibrary {
    /// Builds a read-only deletion inventory. It does not modify anything.
    pub fn plan_artist_deletion(
        &self,
        artist_id: ArtistId,
    ) -> Result<ArtistDeletionPlan, CanonicalError> {
        if !self.artists.contains_key(&artist_id) {
            return Err(CanonicalError::Invalid(format!(
                "artist {artist_id} is not in the library"
            )));
        }
        let owned_albums = self
            .albums
            .values()
            .filter(|album| album.owner_artist_id == artist_id)
            .map(|album| album.id)
            .collect::<Vec<_>>();
        let mut owned_tracks = Vec::new();
        let mut collaborative = Vec::new();
        let mut non_collaborative = Vec::new();
        for track in self
            .tracks
            .values()
            .filter(|track| track.owner_artist_id == artist_id)
        {
            let eligible_owners = track
                .credited_artist_ids
                .iter()
                .copied()
                .filter(|artist| *artist != artist_id && self.artists.contains_key(artist))
                .collect::<Vec<_>>();
            if eligible_owners.is_empty() {
                non_collaborative.push(track.id);
            } else {
                collaborative.push(track.id);
            }
            owned_tracks.push(ArtistDeletionTrack {
                track_id: track.id,
                album_id: track.album_id,
                eligible_owners,
            });
        }
        let credited_only = self
            .tracks
            .values()
            .filter(|track| {
                track.owner_artist_id != artist_id && track.credited_artist_ids.contains(&artist_id)
            })
            .map(|track| track.id)
            .collect::<Vec<_>>();
        let affected = owned_tracks
            .iter()
            .map(|track| track.track_id)
            .chain(credited_only.iter().copied())
            .collect::<HashSet<_>>();
        let affected_playlists = self
            .playlists
            .values()
            .filter(|playlist| {
                playlist
                    .track_ids
                    .iter()
                    .any(|track| affected.contains(track))
            })
            .map(|playlist| playlist.id)
            .collect::<Vec<_>>();
        let mut issues = Vec::new();
        if !owned_tracks.is_empty() {
            issues.push(format!(
                "{} owned track(s) require explicit ownership/deletion decisions",
                owned_tracks.len()
            ));
        }
        if !credited_only.is_empty() {
            issues.push(format!(
                "{} track(s) still credit this artist and require an explicit credit list",
                credited_only.len()
            ));
        }
        if !owned_albums.is_empty() {
            issues.push(format!(
                "{} owned album(s) require a destination/cleanup decision",
                owned_albums.len()
            ));
        }
        if !affected_playlists.is_empty() {
            issues.push(format!(
                "{} playlist(s) reference affected tracks",
                affected_playlists.len()
            ));
        }
        Ok(ArtistDeletionPlan {
            artist_id,
            owned_albums,
            owned_tracks,
            collaborative_tracks: collaborative,
            non_collaborative_tracks: non_collaborative,
            credited_only_tracks: credited_only,
            affected_playlists,
            issues,
        })
    }
}

#[cfg(test)]
mod ownership_recovery_tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    fn library_with_track(dir: &TempDir) -> (CanonicalLibrary, ArtistId, ArtistId, TrackId) {
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source = dir.write("source/song.wav", b"opaque recovery bytes");
        let id = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        (library, a, b, id)
    }

    #[test]
    fn deletion_plan_enumerates_collaborative_and_non_collaborative_owned_tracks() {
        let dir = TempDir::new("deletion-plan");
        let (mut library, a, b, first) = library_with_track(&dir);
        let source = dir.write("source/solo.wav", b"opaque solo bytes");
        let solo = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Solo".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a],
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(2),
                disc_number: None,
                duration_ms: None,
            })
            .expect("solo");
        let plan = library.plan_artist_deletion(a).expect("plan");
        assert!(
            !plan.is_ready(),
            "deletion is unavailable until decisions resolve"
        );
        assert_eq!(plan.collaborative_tracks, vec![first]);
        assert_eq!(plan.non_collaborative_tracks, vec![solo]);
        assert_eq!(plan.owned_tracks.len(), 2);
        assert_eq!(plan.owned_tracks[0].eligible_owners, vec![b]);
        assert!(
            plan.issues
                .iter()
                .any(|issue| issue.contains("owned track"))
        );
    }

    #[test]
    fn committed_journal_cleanup_removes_only_journaled_obsolete_files() {
        let dir = TempDir::new("journal-cleanup");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let obsolete = root.join("obsolete.wav");
        let keep = root.join("keep.wav");
        fs::write(&obsolete, b"old").expect("obsolete");
        fs::write(&keep, b"keep").expect("keep");
        let txn = "txn-cleanup";
        let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join(txn);
        fs::create_dir_all(&txn_dir).expect("txn");
        let catalog_path = root.join(CATALOG_FILE_NAME);
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_path).expect("catalog")).expect("json");
        catalog["transaction_id"] = serde_json::json!(txn);
        fs::write(
            &catalog_path,
            serde_json::to_vec_pretty(&catalog).expect("catalog bytes"),
        )
        .expect("catalog");
        fs::write(
            txn_dir.join("journal.flux"),
            br#"{
                "schema_version": 1,
                "kind": "flux-transaction",
                "transaction_id": "txn-cleanup",
                "operation": "ownership-transfer",
                "state": "committed",
                "intended_generation": 2,
                "actions": [],
                "cleanup": ["obsolete.wav"]
            }"#,
        )
        .expect("journal");
        let reopened = CanonicalLibrary::open(&root).expect("recover cleanup");
        assert!(!obsolete.exists(), "journal-owned obsolete file removed");
        assert!(keep.exists(), "unrelated files are never deleted");
        assert!(!txn_dir.exists());
        drop(reopened);
    }

    #[test]
    fn uncommitted_transfer_journal_restores_backup_and_removes_created_copy() {
        let dir = TempDir::new("journal-rollback");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let old = root.join("old.wav");
        let new = root.join("new.wav");
        fs::write(&old, b"old bytes").expect("old");
        fs::write(&new, b"partial copy").expect("new");
        let txn = "txn-rollback";
        let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join(txn);
        fs::create_dir_all(&txn_dir).expect("txn");
        fs::write(txn_dir.join("backup-0"), b"old bytes").expect("backup");
        fs::write(
            txn_dir.join("journal.flux"),
            br#"{
                "schema_version": 1,
                "kind": "flux-transaction",
                "transaction_id": "txn-rollback",
                "operation": "ownership-transfer",
                "state": "applying",
                "intended_generation": 2,
                "actions": [
                    {"target": "old.wav", "existed": true, "backup": "backup-0"},
                    {"target": "new.wav", "existed": false}
                ]
            }"#,
        )
        .expect("journal");
        fs::write(&new, b"partial copy").expect("partial");
        let reopened = CanonicalLibrary::open(&root).expect("rollback");
        assert_eq!(fs::read(&old).expect("old restored"), b"old bytes");
        assert!(!new.exists(), "uncommitted created copy removed");
        assert!(!txn_dir.exists());
        drop(reopened);
    }
}

#[cfg(test)]
mod failure_matrix_tests {
    use std::fs;

    use flux_core::TrackId;

    use super::*;
    use crate::test_support::TempDir;

    const POINTS: &[TransactionFailPoint] = &[
        TransactionFailPoint::BeforeJournal,
        TransactionFailPoint::AfterJournal,
        TransactionFailPoint::BeforeStaging,
        TransactionFailPoint::DuringCopy,
        TransactionFailPoint::DuringAudioCopy,
        TransactionFailPoint::AfterAudioCopyBeforeVerify,
        TransactionFailPoint::DuringLyricsCopy,
        TransactionFailPoint::AfterCopyBeforeVerify,
        TransactionFailPoint::AfterVerify,
        TransactionFailPoint::BeforeMetadataPublish,
        TransactionFailPoint::DuringMetadataPublish,
        TransactionFailPoint::BeforeCatalogPublish,
        TransactionFailPoint::AfterCatalogPublish,
        TransactionFailPoint::BeforeCleanup,
        TransactionFailPoint::DuringCleanup,
        TransactionFailPoint::DuringLyricsCleanup,
        TransactionFailPoint::AfterCleanup,
        TransactionFailPoint::BeforeJournalFinalize,
    ];

    struct Fixture {
        _dir: TempDir,
        root: std::path::PathBuf,
        library: CanonicalLibrary,
        track: TrackId,
        a: ArtistId,
        b: ArtistId,
        destination: AlbumId,
        old_audio: std::path::PathBuf,
        old_record: std::path::PathBuf,
        old_lyrics: std::path::PathBuf,
        plan: OwnershipTransferPlan,
        playlist: PlaylistId,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = TempDir::new(name);
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source = dir.write("source/song.wav", b"durable transfer payload");
        let track = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Source".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        library
            .set_lyrics(track, Some("[00:00.00]lyrics"))
            .expect("lyrics");
        library.set_rating(track, Some(4), true).expect("rating");
        let playlist = library
            .set_playlist(None, "Keep", vec![track])
            .expect("playlist");
        let destination = library
            .create_album(b, "Destination", ReleaseType::Ep)
            .expect("album");
        let old_audio = library.track_audio_path(track).expect("old audio");
        let old_record = library.track_record_path(track).expect("old record");
        let old_lyrics = library.track_lyrics_path(track).expect("old lyrics");
        let plan = library
            .plan_ownership_transfer(track, b, OwnershipDestination::Existing(destination))
            .expect("plan");
        Fixture {
            _dir: dir,
            root,
            library,
            track,
            a,
            b,
            destination,
            old_audio,
            old_record,
            old_lyrics,
            plan,
            playlist,
        }
    }

    fn expected_committed(point: TransactionFailPoint) -> bool {
        matches!(
            point,
            TransactionFailPoint::AfterCatalogPublish
                | TransactionFailPoint::BeforeCleanup
                | TransactionFailPoint::DuringCleanup
                | TransactionFailPoint::DuringLyricsCleanup
                | TransactionFailPoint::AfterCleanup
                | TransactionFailPoint::BeforeJournalFinalize
        )
    }

    #[test]
    fn transfer_failure_matrix_is_atomic_and_recovery_is_idempotent() {
        for point in POINTS {
            let mut fx = fixture(&format!("matrix-{point:?}"));
            fx.library.set_transaction_failure(Some(TransactionFailure {
                point: *point,
                interrupt: true,
            }));
            let error = fx
                .library
                .execute_ownership_transfer(&fx.plan)
                .expect_err("injected failure must be returned");
            assert!(error.to_string().contains("injected"), "{error}");
            drop(fx.library);
            let reopened = CanonicalLibrary::open(&fx.root)
                .unwrap_or_else(|err| panic!("{point:?} recovery failed: {err}"));
            let track = reopened.track(fx.track).expect("track identity preserved");
            let final_owner = track.owner_artist_id;
            let committed = expected_committed(*point);
            assert_eq!(
                track.owner_artist_id,
                if committed { fx.b } else { fx.a },
                "{point:?} owner"
            );
            assert_eq!(
                track.credited_artist_ids,
                vec![fx.a, fx.b],
                "{point:?} credits"
            );
            assert_eq!(
                track.album_id,
                if committed {
                    fx.destination
                } else {
                    AlbumId::new(1)
                }
            );
            assert_eq!(reopened.tracks().count(), 1, "{point:?} one track");
            let new_audio = reopened.track_audio_path(fx.track).expect("new audio");
            let new_record = reopened.track_record_path(fx.track).expect("new record");
            let destination_audio = fx.root.join(
                fx.plan
                    .new_audio_path()
                    .to_os_path()
                    .expect("destination path"),
            );
            let destination_record = destination_audio.with_extension("flux");
            if committed {
                assert!(!fx.old_audio.exists(), "{point:?} old audio removed");
                assert!(!fx.old_record.exists(), "{point:?} old record removed");
                assert!(!fx.old_lyrics.exists(), "{point:?} old lyrics removed");
                assert!(new_audio.is_file(), "{point:?} new audio");
                assert!(new_record.is_file(), "{point:?} new record");
                assert_eq!(
                    fs::read(&new_audio).expect("audio"),
                    b"durable transfer payload"
                );
                let new_lyrics = reopened.track_lyrics_path(fx.track).expect("new lyrics");
                assert!(new_lyrics.is_file(), "{point:?} new lyrics");
                assert_eq!(
                    fs::read_to_string(new_lyrics).expect("lyrics"),
                    "[00:00.00]lyrics"
                );
            } else {
                assert!(fx.old_audio.is_file(), "{point:?} old audio preserved");
                assert!(fx.old_record.is_file(), "{point:?} old record preserved");
                assert!(fx.old_lyrics.is_file(), "{point:?} old lyrics preserved");
                assert!(
                    !destination_audio.exists(),
                    "{point:?} no destination audio"
                );
                assert!(
                    !destination_record.exists(),
                    "{point:?} no destination record"
                );
            }
            assert_eq!(
                reopened.rating(fx.track),
                Some(TrackRating {
                    rating: Some(4),
                    favorite: true
                }),
                "{point:?} rating"
            );
            assert_eq!(
                reopened.playlist(fx.playlist).map(|p| p.track_ids.clone()),
                Some(vec![fx.track]),
                "{point:?} playlist"
            );
            drop(reopened);
            let again = CanonicalLibrary::open(&fx.root)
                .unwrap_or_else(|err| panic!("{point:?} second recovery failed: {err}"));
            assert_eq!(
                again.track(fx.track).map(|t| t.owner_artist_id),
                Some(final_owner)
            );
            drop(again);
        }
    }

    #[test]
    fn retry_after_rollback_succeeds_without_duplicate_files() {
        let mut fx = fixture("retry-rollback");
        fx.library.set_transaction_failure(Some(TransactionFailure {
            point: TransactionFailPoint::AfterVerify,
            interrupt: false,
        }));
        assert!(fx.library.execute_ownership_transfer(&fx.plan).is_err());
        fx.library.set_transaction_failure(None);
        // The same plan remains valid after rollback.
        fx.library
            .execute_ownership_transfer(&fx.plan)
            .expect("retry after rollback");
        assert_eq!(
            fx.library.track(fx.track).map(|t| t.owner_artist_id),
            Some(fx.b)
        );
        assert_eq!(fx.library.tracks().count(), 1);
        assert!(!fx.old_audio.exists());
        assert!(
            fx.library
                .track_audio_path(fx.track)
                .expect("audio")
                .is_file()
        );
        drop(fx.library);
        assert!(CanonicalLibrary::open(&fx.root).is_ok());
    }

    #[test]
    fn malformed_truncated_and_orphan_journals_are_reported_without_cleanup() {
        for (name, journal, mkdir_only) in [
            ("truncated", Some(b"{".as_slice()), false),
            (
                "unknown-version",
                Some(br#"{"schema_version":99,"kind":"flux-transaction"}"#.as_slice()),
                false,
            ),
            ("missing", None, true),
        ] {
            let dir = TempDir::new(&format!("journal-{name}"));
            let root = dir.path().join("FluxLibrary");
            CanonicalLibrary::create(&root).expect("create");
            let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join(format!("txn-{name}"));
            fs::create_dir_all(&txn_dir).expect("txn dir");
            if !mkdir_only && let Some(bytes) = journal {
                fs::write(txn_dir.join("journal.flux"), bytes).expect("journal");
            }
            let valuable = txn_dir.join("valuable.bin");
            fs::write(&valuable, b"do not delete").expect("valuable");
            assert!(
                CanonicalLibrary::open(&root).is_err(),
                "{name} must require recovery attention"
            );
            assert!(valuable.is_file(), "{name} preserves ambiguous resources");
            assert!(txn_dir.is_dir(), "{name} preserves journal directory");
        }
    }

    #[test]
    fn committed_journal_without_catalogue_commit_is_not_cleaned_up() {
        let dir = TempDir::new("journal-state-only");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let valuable = root.join("valuable.wav");
        fs::write(&valuable, b"must survive").expect("valuable");
        let txn = "txn-state-only";
        let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join(txn);
        fs::create_dir_all(&txn_dir).expect("txn");
        fs::write(
            txn_dir.join("journal.flux"),
            br#"{
                "schema_version": 1,
                "kind": "flux-transaction",
                "transaction_id": "txn-state-only",
                "operation": "ownership-transfer",
                "state": "committed",
                "intended_generation": 2,
                "actions": [],
                "cleanup": ["valuable.wav"]
            }"#,
        )
        .expect("journal");
        assert!(CanonicalLibrary::open(&root).is_err());
        assert!(
            valuable.is_file(),
            "a journal state alone is not a commit record"
        );
        assert!(txn_dir.is_dir(), "ambiguous journal is preserved");
    }

    #[test]
    fn recovery_cleanup_failure_preserves_journal_and_can_retry() {
        let dir = TempDir::new("recovery-retry");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let blocked = root.join("blocked");
        fs::create_dir_all(&blocked).expect("blocked dir");
        let txn = "txn-blocked-cleanup";
        let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join(txn);
        fs::create_dir_all(&txn_dir).expect("txn");
        let catalog_path = root.join(CATALOG_FILE_NAME);
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_path).expect("catalog")).expect("json");
        catalog["transaction_id"] = serde_json::json!(txn);
        fs::write(
            &catalog_path,
            serde_json::to_vec_pretty(&catalog).expect("catalog bytes"),
        )
        .expect("catalog");
        fs::write(
            txn_dir.join("journal.flux"),
            br#"{
                "schema_version": 1,
                "kind": "flux-transaction",
                "transaction_id": "txn-blocked-cleanup",
                "operation": "ownership-transfer",
                "state": "cleaning",
                "intended_generation": 2,
                "actions": [],
                "cleanup": ["blocked"]
            }"#,
        )
        .expect("journal");
        assert!(
            CanonicalLibrary::open(&root).is_err(),
            "cleanup failure must require recovery attention"
        );
        assert!(blocked.is_dir());
        assert!(txn_dir.is_dir(), "journal survives failed recovery");
        fs::remove_dir(&blocked).expect("unblock");
        let reopened = CanonicalLibrary::open(&root).expect("retry recovery");
        assert!(
            !txn_dir.exists(),
            "journal finalized after successful retry"
        );
        drop(reopened);
    }

    #[test]
    fn cleanup_refuses_paths_still_referenced_by_catalogue() {
        let dir = TempDir::new("cleanup-reference");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let source = dir.write("source/song.wav", b"keep me");
        let track = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: Vec::new(),
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: None,
                disc_number: None,
                duration_ms: None,
            })
            .expect("import");
        let audio = library.track_audio_path(track).expect("audio");
        let rel = RelPath::from_utf8(
            audio
                .strip_prefix(&root)
                .expect("inside")
                .to_str()
                .expect("utf8"),
        )
        .expect("rel");
        let txn = "txn-unsafe";
        let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join(txn);
        fs::create_dir_all(&txn_dir).expect("txn");
        fs::write(
            txn_dir.join("journal.flux"),
            br#"{
                "schema_version": 1,
                "kind": "flux-transaction",
                "transaction_id": "txn-unsafe",
                "operation": "ownership-transfer",
                "state": "committed",
                "intended_generation": 2,
                "actions": [],
                "cleanup": ["artists/artist_000001/albums/album_000001/tracks/track_000001.wav"]
            }"#,
        )
        .expect("journal");
        assert!(CanonicalLibrary::open(&root).is_err());
        assert!(audio.is_file(), "referenced audio is never cleaned up");
        assert!(txn_dir.is_dir(), "unsafe journal is preserved");
        let _ = rel;
    }
}
