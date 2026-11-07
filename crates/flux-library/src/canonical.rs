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
//! committed, and only then deletes its backups. Metadata-only changes publish
//! the catalogue too, so generation and transaction identity are durable for
//! every catalogue-affecting write. Startup recovery either completes cleanup
//! for a committed catalogue or restores the pre-batch bytes and removes files
//! created by an uncommitted batch. Setup errors before journal publication
//! remove only the current transaction directory.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use flux_core::{ArtistId, TrackId};
use serde::{Deserialize, Serialize};

use crate::fs_ops::{is_link_or_reparse, write_atomic as write_atomic_shared};
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
/// Track-specific artwork overrides, keyed by managed file names.
pub const TRACK_ARTWORK_DIR_NAME: &str = "track-artwork";

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    /// Optional user-authored description.
    pub description: Option<String>,
    /// Optional managed artist image file name inside the artist directory.
    pub image: Option<String>,
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
    /// Owning album, or `None` for a standalone Track.
    pub album_id: Option<AlbumId>,
    /// Track number within its disc.
    pub track_number: Option<u32>,
    /// Disc number.
    pub disc_number: Option<u32>,
    /// Stored technical duration, when known independently of tags.
    pub duration_ms: Option<u64>,
    /// Optional lyrics file name inside the track directory.
    pub lyrics: Option<String>,
    /// Optional Track-specific artwork file name under `track-artwork/`.
    /// Album artwork is the fallback when absent.
    pub artwork: Option<String>,
    /// Optional content fingerprint.
    pub fingerprint: Option<ContentFingerprint>,
    /// Canonical creation/import time; zero means unknown/legacy.
    pub created_unix_ms: u64,
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
    /// Immediately after the current transaction directory is created.
    AfterTransactionDirectory,
    /// Immediately after all prior target files are backed up.
    AfterBackups,
    /// Immediately before the prepared journal is written.
    BeforeJournalWrite,
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
    /// When true, terminate the process at the boundary (test children only).
    pub abort: bool,
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
    /// Absolute path to an album's managed cover, when recorded.
    #[must_use]
    pub fn album_cover_path(&self, id: AlbumId) -> Option<PathBuf> {
        let album = self.albums.get(&id)?;
        let cover = album.cover.as_ref()?;
        Some(self.album_dir(id)?.join(cover))
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    album_id: Option<u64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    image: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    album_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    track_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disc_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lyrics: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    artwork: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fingerprint: Option<ContentFingerprint>,
    /// Canonical creation/import time; zero means this predates the field.
    #[serde(default)]
    created_unix_ms: u64,
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
fn track_container_rel(owner: ArtistId, album: Option<AlbumId>) -> String {
    album.map_or_else(
        || format!("artists/artist_{:06}/standalone", owner.get()),
        |album| {
            format!(
                "artists/artist_{:06}/albums/album_{:06}",
                owner.get(),
                album.get()
            )
        },
    )
}
fn track_audio_rel(
    owner: ArtistId,
    album: Option<AlbumId>,
    id: TrackId,
    extension: &str,
) -> RelPath {
    RelPath::from_utf8(&format!(
        "{}/tracks/track_{:06}.{extension}",
        track_container_rel(owner, album),
        id.get()
    ))
    .expect("generated track path is valid")
}
fn track_record_rel(owner: ArtistId, album: Option<AlbumId>, id: TrackId) -> RelPath {
    RelPath::from_utf8(&format!(
        "{}/tracks/track_{:06}.flux",
        track_container_rel(owner, album),
        id.get()
    ))
    .expect("generated record path is valid")
}
fn track_replacement_audio_rel(
    owner: ArtistId,
    album: Option<AlbumId>,
    id: TrackId,
    extension: &str,
    version: u64,
) -> RelPath {
    RelPath::from_utf8(&format!(
        "{}/tracks/track_{:06}.replacement-{version}.{extension}",
        track_container_rel(owner, album),
        id.get()
    ))
    .expect("generated replacement path is valid")
}

fn track_audio_path_is_canonical(
    path: &RelPath,
    owner: ArtistId,
    album: Option<AlbumId>,
    id: TrackId,
) -> bool {
    let text = path.display_lossy();
    let Some((_, file)) = text.rsplit_once('/') else {
        return false;
    };
    let Some(extension) = Path::new(file).extension().and_then(|ext| ext.to_str()) else {
        return false;
    };
    if path == &track_audio_rel(owner, album, id, extension) {
        return true;
    }
    let prefix = format!("track_{:06}.replacement-", id.get());
    let Some(versioned) = file.strip_prefix(&prefix) else {
        return false;
    };
    let Some((version, candidate_extension)) = versioned.split_once('.') else {
        return false;
    };
    !version.is_empty()
        && version.bytes().all(|byte| byte.is_ascii_digit())
        && candidate_extension == extension
        && path
            == &track_replacement_audio_rel(
                owner,
                album,
                id,
                extension,
                version.parse().unwrap_or(0),
            )
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

fn validate_image_extension_size(
    source: &Path,
    extension: &str,
    label: &str,
) -> Result<String, CanonicalError> {
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();
    if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
        return Err(CanonicalError::Invalid(format!(
            "{label} must be png, jpg, jpeg, or webp"
        )));
    }
    let size = fs::metadata(source)?.len();
    if size == 0 || size > 16 * 1024 * 1024 {
        return Err(CanonicalError::Invalid(format!(
            "{label} must be between 1 byte and 16 MiB"
        )));
    }
    Ok(extension)
}

fn validate_import_asset(
    asset: &CanonicalImportAsset,
    label: &str,
) -> Result<String, CanonicalError> {
    if !asset.source.is_file() {
        return Err(CanonicalError::Invalid(format!(
            "{label} source is not a readable file"
        )));
    }
    let extension = asset
        .source
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| CanonicalError::Invalid(format!("{label} has no extension")))?;
    let extension = validate_image_extension_size(&asset.source, extension, label)?;
    if ContentFingerprint::of_file(&asset.source)? != asset.fingerprint {
        return Err(CanonicalError::Invalid(format!(
            "{label} source changed after review"
        )));
    }
    Ok(extension)
}

fn validate_asset_name(name: &str, file: &str, label: &str) -> Result<(), CanonicalError> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(CanonicalError::Invalid(format!(
            "{file} has an invalid {label} file name"
        )));
    }
    Ok(())
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
            Ok(metadata) if is_link_or_reparse(&metadata) => {
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
    /// Album title, or `None` for a standalone Track.
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

/// Snapshot used to import reviewed tracks into an existing canonical album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalAlbumImportTarget {
    /// Target album identity.
    pub album_id: AlbumId,
    /// Catalogue generation captured by the review.
    pub generation: u64,
    /// Required album owner and track owner.
    pub owner_artist_id: ArtistId,
    /// Album title.
    pub title: String,
    /// Release type.
    pub release_type: ReleaseType,
    /// Cover file name.
    pub cover: Option<String>,
    /// Existing track order that new tracks append after.
    pub track_order: Vec<TrackId>,
}

impl CanonicalLibrary {
    /// Creates an empty canonical library at `root`.
    ///
    /// The root must be either absent or empty. A partial canonical layout is
    /// never overwritten; use an explicit migration/repair workflow instead.
    pub fn create(root: &Path) -> Result<Self, CanonicalError> {
        match fs::symlink_metadata(root) {
            Ok(metadata) if is_link_or_reparse(&metadata) => {
                return Err(CanonicalError::Invalid(
                    "the library root must not be a symbolic link or reparse point".to_owned(),
                ));
            }
            Ok(metadata) if metadata.is_dir() => {
                // The creation workflow holds `<root>/.flux.lock` before it
                // validates emptiness. The lease file is not library content
                // and must not make first-run creation fail.
                for entry in fs::read_dir(root)? {
                    let entry = entry?;
                    if entry.file_name() != ".flux.lock" {
                        return Err(CanonicalError::AlreadyExists(root.to_path_buf()));
                    }
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
            let album = entry.album_id.map(AlbumId::new);
            if id.get() == 0 || owner.get() == 0 || album.is_some_and(|album| album.get() == 0) {
                return Err(CanonicalError::Invalid("track/owner/album ID 0".to_owned()));
            }
            if !artist_paths.contains_key(&owner) {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} references missing artist {owner}"
                )));
            }
            if let Some(album) = album
                && !album_paths.contains_key(&album)
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} references missing album {album}"
                )));
            }
            let audio = canonical_rel_path(&entry.audio_path, "track audio")?;
            if Path::new(&entry.audio_path).extension().is_none() {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} has no audio extension"
                )));
            }
            if !track_audio_path_is_canonical(&audio, owner, album, id) {
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
        validate_next_counter(
            "next_artist_id",
            catalog.next_artist_id,
            artist_paths.keys().map(|id| id.get()).max(),
            "artist",
        )?;
        validate_next_counter(
            "next_album_id",
            catalog.next_album_id,
            album_paths.keys().map(|id| id.get()).max(),
            "album",
        )?;
        validate_next_counter(
            "next_track_id",
            catalog.next_track_id,
            track_paths.keys().map(|id| id.get()).max(),
            "track",
        )?;
        validate_next_counter(
            "next_playlist_id",
            catalog.next_playlist_id,
            playlist_paths.keys().map(|id| id.get()).max(),
            "playlist",
        )?;

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
            if let Some(image) = &dto.image {
                validate_asset_name(image, &file, "artist image")?;
                reject_symlink_escape(
                    &root,
                    &RelPath::from_utf8(&format!("{}/{}", rel.display_lossy(), image))?,
                )?;
            }
            artists.insert(
                *id,
                CanonicalArtist {
                    id: *id,
                    name: dto.name,
                    aliases: dto.aliases,
                    description: dto.description,
                    image: dto.image,
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
            let album_id = descriptor.album_id.map(AlbumId::new);
            if dto.owner_artist_id != owner.get() || dto.album_id != descriptor.album_id {
                return Err(CanonicalError::Invalid(format!(
                    "{file} owner/album does not match the catalogue"
                )));
            }
            if let Some(album_id) = album_id {
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
            } else if album_track_membership.contains_key(&id) {
                return Err(CanonicalError::Invalid(format!(
                    "standalone track {id} appears in an album order"
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
            if let Some(artwork) = &dto.artwork {
                validate_asset_name(artwork, &file, "track artwork")?;
                reject_symlink_escape(
                    &root,
                    &RelPath::from_utf8(&format!("{TRACK_ARTWORK_DIR_NAME}/{artwork}"))?,
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
                    artwork: dto.artwork,
                    fingerprint: dto.fingerprint,
                    created_unix_ms: dto.created_unix_ms,
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

fn validate_next_counter(
    counter_name: &str,
    next: u64,
    max_existing: Option<u64>,
    entity_name: &str,
) -> Result<(), CanonicalError> {
    if next == u64::MAX {
        return Err(CanonicalError::Invalid(format!(
            "{counter_name} is u64::MAX and leaves no room for a safe allocation"
        )));
    }
    if let Some(max_existing) = max_existing
        && next <= max_existing
    {
        return Err(CanonicalError::Invalid(format!(
            "{counter_name} {next} is not greater than existing maximum {entity_name} ID {max_existing}; \
             refusing to reuse an existing ID or managed path"
        )));
    }
    Ok(())
}

fn advance_counter(counter: u64, entity_name: &str) -> Result<u64, CanonicalError> {
    let next = counter
        .checked_add(1)
        .ok_or_else(|| CanonicalError::Invalid(format!("{entity_name} counter overflow")))?;
    if next == u64::MAX {
        return Err(CanonicalError::Invalid(format!(
            "{entity_name} ID counter is exhausted before u64::MAX; refusing to create a counter state that cannot be reopened safely"
        )));
    }
    Ok(next)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CanonicalError> {
    let parent = path
        .parent()
        .ok_or_else(|| CanonicalError::Invalid("file has no parent directory".to_owned()))?;
    fs::create_dir_all(parent)?;
    write_atomic_shared(path, bytes)?;
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
    #[cfg(test)]
    if std::env::var_os("FLUX_TEST_ABORT_RECOVERY").is_some() {
        std::process::abort();
    }
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
            // Older builds did not update catalog.flux for metadata-only
            // commits. Such a journal used the current catalogue generation
            // as its intended generation and never wrote a transaction ID.
            // Its committed/cleaning/completed state means all metadata files
            // were already published, so finish only its journal-owned cleanup.
            if journal.operation == "metadata-write" && journal.intended_generation == generation {
                cleanup_committed_paths(root, &journal)?;
                let _ = fs::remove_dir_all(entry.path());
                let _ = sync_directory(&dir);
                continue;
            }
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

/// Artist reference in an atomic import batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalImportArtistRef {
    /// Existing stable artist identity.
    Existing(ArtistId),
    /// Proposed new artist, created only if the whole batch commits.
    Proposed(String),
}

/// Album reference in an atomic import batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanonicalImportAlbumRef {
    /// Standalone Track with no Album membership.
    Standalone,
    /// Existing album whose owner must match the track owner.
    Existing(AlbumId),
    /// Reuse an exact owner/title album or create it inside the batch.
    ByName {
        /// Album title.
        title: String,
        /// Release type for a newly created album.
        release_type: ReleaseType,
    },
}

/// Reviewed managed artwork attached during import.
#[derive(Debug, Clone)]
pub struct CanonicalImportAsset {
    /// Source image file.
    pub source: PathBuf,
    /// Fingerprint captured at review time.
    pub fingerprint: ContentFingerprint,
}

/// One reviewed track in an atomic import batch.
#[derive(Debug, Clone)]
pub struct CanonicalImportTrackRequest {
    /// Source audio file.
    pub source: PathBuf,
    /// Track title.
    pub title: String,
    /// Ordered credited artists.
    pub credited_artists: Vec<CanonicalImportArtistRef>,
    /// Index of the physical owner in the ordered credits.
    pub owner_index: usize,
    /// Album destination.
    pub album: CanonicalImportAlbumRef,
    /// Optional track number.
    pub track_number: Option<u32>,
    /// Optional disc number.
    pub disc_number: Option<u32>,
    /// Optional technical duration.
    pub duration_ms: Option<u64>,
    /// Optional Track-specific artwork override.
    pub artwork: Option<CanonicalImportAsset>,
    /// Optional album artwork applied to the destination album.
    pub album_artwork: Option<CanonicalImportAsset>,
    /// Optional managed lyrics text (LRC or plain text).
    pub lyrics: Option<String>,
    /// Explicit duplicate review decision permits identical content in this batch.
    pub allow_duplicate_content: bool,
    /// Source fingerprint captured at review time; changed sources are stale.
    pub expected_source_fingerprint: Option<ContentFingerprint>,
}

/// An explicitly approved metadata update for an existing track. Audio and
/// TrackId are preserved; the owner and album never change here.
#[derive(Debug, Clone)]
pub struct CanonicalTrackMetadataUpdate {
    /// Stable existing track.
    pub track_id: TrackId,
    /// File fingerprint expected at review time.
    pub expected_fingerprint: Option<ContentFingerprint>,
    /// Album expected at review time; metadata updates cannot move tracks.
    pub expected_album_id: Option<AlbumId>,
    /// Title expected at review time.
    pub expected_title: String,
    /// Ordered credits expected at review time.
    pub expected_credited_artist_ids: Vec<ArtistId>,
    /// Track number expected at review time.
    pub expected_track_number: Option<u32>,
    /// Disc number expected at review time.
    pub expected_disc_number: Option<u32>,
    /// Duration expected at review time.
    pub expected_duration_ms: Option<u64>,
    /// Proposed new title.
    pub title: String,
    /// Proposed ordered credits; must include the existing physical owner.
    pub credited_artists: Vec<CanonicalImportArtistRef>,
    /// Proposed track number.
    pub track_number: Option<u32>,
    /// Proposed disc number.
    pub disc_number: Option<u32>,
    /// Proposed duration.
    pub duration_ms: Option<u64>,
    /// Optional Track artwork override to attach or replace.
    pub artwork: Option<CanonicalImportAsset>,
    /// Optional Album artwork to attach or replace.
    pub album_artwork: Option<CanonicalImportAsset>,
    /// Optional lyrics text to attach or replace.
    pub lyrics: Option<String>,
}

fn resolve_import_artists(
    catalog: &mut CatalogDto,
    artists: &mut BTreeMap<ArtistId, CanonicalArtist>,
    new_artist_ids: &mut BTreeMap<String, ArtistId>,
    writes: &mut Vec<PendingWrite>,
    references: Vec<CanonicalImportArtistRef>,
) -> Result<Vec<ArtistId>, CanonicalError> {
    let mut credits = Vec::with_capacity(references.len());
    for reference in references {
        let id = match reference {
            CanonicalImportArtistRef::Existing(id) => {
                if !artists.contains_key(&id) {
                    return Err(CanonicalError::Invalid(format!(
                        "existing artist {id} is not in the library"
                    )));
                }
                id
            }
            CanonicalImportArtistRef::Proposed(name) => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return Err(CanonicalError::Invalid(
                        "proposed artist name must not be empty".to_owned(),
                    ));
                }
                if let Some(id) = new_artist_ids.get(&name) {
                    *id
                } else {
                    let id = ArtistId::new(catalog.next_artist_id);
                    catalog.next_artist_id = advance_counter(catalog.next_artist_id, "artist")?;
                    let rel = artist_rel(id);
                    catalog.artists.push(CatalogArtistDto {
                        id: id.get(),
                        path: rel.display_lossy(),
                    });
                    let dto = ArtistEntityDto {
                        schema_version: CANONICAL_SCHEMA_VERSION,
                        kind: ARTIST_KIND.to_owned(),
                        id: id.get(),
                        name: name.clone(),
                        aliases: Vec::new(),
                        description: None,
                        image: None,
                    };
                    push_entity_write(writes, &rel, "artist.flux", &dto)?;
                    artists.insert(
                        id,
                        CanonicalArtist {
                            id,
                            name: name.clone(),
                            aliases: Vec::new(),
                            description: None,
                            image: None,
                        },
                    );
                    new_artist_ids.insert(name, id);
                    id
                }
            }
        };
        if credits.contains(&id) {
            return Err(CanonicalError::Invalid(
                "an artist cannot appear twice in ordered credits".to_owned(),
            ));
        }
        credits.push(id);
    }
    Ok(credits)
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
        let release_type = self
            .albums
            .get(&id)
            .map(|album| album.release_type)
            .ok_or_else(|| CanonicalError::Invalid("album is not in the library".to_owned()))?;
        self.update_album_metadata(id, title, release_type)
    }

    /// Updates supported album metadata without changing AlbumId or paths.
    pub fn update_album_metadata(
        &mut self,
        id: AlbumId,
        title: impl Into<String>,
        release_type: ReleaseType,
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
        if self.albums.values().any(|candidate| {
            candidate.id != id
                && candidate.owner_artist_id == album.owner_artist_id
                && candidate.title == title
        }) {
            return Err(CanonicalError::Invalid(format!(
                "owner already has an album titled {title:?}"
            )));
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
            release_type,
            cover: album.cover,
            track_order: album.track_order.iter().map(|track| track.get()).collect(),
        };
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &rel, "album.flux", &dto)?;
        self.commit_writes(None, writes)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Deletes an empty album. Populated albums are refused: tracks must first
    /// be transferred or explicitly deleted through their own reviewed flows.
    pub fn delete_empty_album(&mut self, id: AlbumId) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let album = self
            .albums
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("album is not in the library".to_owned()))?;
        if !album.track_order.is_empty() {
            return Err(CanonicalError::Invalid(
                "album still contains tracks; transfer or delete tracks first".to_owned(),
            ));
        }
        let rel = self
            .album_paths
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing album path".to_owned()))?;
        let mut catalog = self.catalog.clone();
        catalog.albums.retain(|entry| entry.id != id.get());
        let mut cleanup = vec![RelPath::from_utf8(&format!(
            "{}/album.flux",
            rel.display_lossy()
        ))?];
        if let Some(cover) = &album.cover {
            cleanup.push(RelPath::from_utf8(&format!(
                "{}/{}",
                rel.display_lossy(),
                cover
            ))?);
        }
        self.commit_writes_with_cleanup(Some(catalog), Vec::new(), cleanup)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Removes an album's managed cover while preserving all other metadata.
    pub fn remove_cover(&mut self, album_id: AlbumId) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let album = self
            .albums
            .get(&album_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("album is not in the library".to_owned()))?;
        let Some(cover) = album.cover.clone() else {
            return Err(CanonicalError::Invalid(
                "album has no managed cover".to_owned(),
            ));
        };
        let rel = self
            .album_paths
            .get(&album_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing album path".to_owned()))?;
        let dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: album_id.get(),
            owner_artist_id: album.owner_artist_id.get(),
            title: album.title,
            release_type: album.release_type,
            cover: None,
            track_order: album.track_order.iter().map(|id| id.get()).collect(),
        };
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &rel, "album.flux", &dto)?;
        let cleanup = vec![RelPath::from_utf8(&format!(
            "{}/{}",
            rel.display_lossy(),
            cover
        ))?];
        self.commit_writes_with_cleanup(None, writes, cleanup)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Sets a complete album track order, preserving every stable TrackId.
    pub fn set_album_track_order(
        &mut self,
        album_id: AlbumId,
        order: Vec<TrackId>,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let album = self
            .albums
            .get(&album_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("album is not in the library".to_owned()))?;
        let current = album.track_order.iter().copied().collect::<HashSet<_>>();
        let requested = order.iter().copied().collect::<HashSet<_>>();
        if current != requested || current.len() != order.len() {
            return Err(CanonicalError::Invalid(
                "reordered album must contain exactly the existing tracks".to_owned(),
            ));
        }
        for track in &order {
            if self
                .tracks
                .get(track)
                .is_none_or(|value| value.album_id != Some(album_id))
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {track} does not belong to album {album_id}"
                )));
            }
        }
        let rel = self
            .album_paths
            .get(&album_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing album path".to_owned()))?;
        let dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: album_id.get(),
            owner_artist_id: album.owner_artist_id.get(),
            title: album.title,
            release_type: album.release_type,
            cover: album.cover,
            track_order: order.iter().map(|track| track.get()).collect(),
        };
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &rel, "album.flux", &dto)?;
        self.commit_writes(None, writes)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Moves one album track by `delta` positions.
    pub fn reorder_album_track(
        &mut self,
        album_id: AlbumId,
        track_id: TrackId,
        delta: isize,
    ) -> Result<(), CanonicalError> {
        let album = self
            .albums
            .get(&album_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("album is not in the library".to_owned()))?;
        let mut order = album.track_order.clone();
        let Some(index) = order.iter().position(|track| *track == track_id) else {
            return Err(CanonicalError::Invalid(
                "track is not in this album".to_owned(),
            ));
        };
        let target = index as isize + delta;
        if target < 0 || target as usize >= order.len() {
            return Ok(());
        }
        let track = order.remove(index);
        order.insert(target as usize, track);
        self.set_album_track_order(album_id, order)
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
            if let Some(album) = track
                .album_id
                .and_then(|album_id| self.albums.get(&album_id))
            {
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
        TransactionFailPoint::AfterTransactionDirectory => "after-transaction-directory",
        TransactionFailPoint::AfterBackups => "after-backups",
        TransactionFailPoint::BeforeJournalWrite => "before-journal-write",
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

/// Snapshot used to reject a stale metadata edit before publication.
#[derive(Debug, Clone)]
pub struct TrackMetadataUpdatePlan {
    /// Target track.
    pub track_id: TrackId,
    /// Existing album identity, which must not change. `None` means standalone.
    pub album_id: Option<AlbumId>,
    /// Existing physical owner, which must not change.
    pub owner_artist_id: ArtistId,
    /// Catalogue generation at review.
    pub generation: u64,
    /// Expected title.
    pub expected_title: String,
    /// Expected ordered credits.
    pub expected_credited_artist_ids: Vec<ArtistId>,
    /// Expected track number.
    pub expected_track_number: Option<u32>,
    /// Expected disc number.
    pub expected_disc_number: Option<u32>,
    /// Expected duration.
    pub expected_duration_ms: Option<u64>,
    /// Expected audio fingerprint.
    pub expected_fingerprint: Option<ContentFingerprint>,
    /// Expected album title, or `None` for a standalone Track.
    pub expected_album_title: Option<String>,
}

/// Immutable plan for moving one track between albums under the same owner.
#[derive(Debug, Clone)]
pub struct TrackMovePlan {
    /// Target track.
    pub track_id: TrackId,
    /// Track title captured for review.
    pub title: String,
    /// Physical owner, which must not change.
    pub owner_artist_id: ArtistId,
    /// Source album, or `None` when the Track is standalone.
    pub from_album_id: Option<AlbumId>,
    /// Source album title, or `None` when standalone.
    pub from_album_title: Option<String>,
    /// Destination album, or `None` to become standalone.
    pub to_album_id: Option<AlbumId>,
    /// Destination album title, or `None` when becoming standalone.
    pub to_album_title: Option<String>,
    /// Explicit insertion index in the destination order.
    pub insertion_index: usize,
    /// Expected source order at review.
    pub expected_from_order: Vec<TrackId>,
    /// Destination order before insertion, used for stale validation.
    pub expected_to_order: Vec<TrackId>,
    /// Reviewed destination order after insertion.
    pub destination_order: Vec<TrackId>,
    /// Expected ordered credits.
    pub expected_credited_artist_ids: Vec<ArtistId>,
    /// Expected track number.
    pub expected_track_number: Option<u32>,
    /// Expected disc number.
    pub expected_disc_number: Option<u32>,
    /// Expected duration.
    pub expected_duration_ms: Option<u64>,
    /// Expected audio path.
    pub audio_path: RelPath,
    /// Expected record path.
    pub record_path: RelPath,
    /// Expected lyrics path.
    pub lyrics_path: Option<RelPath>,
    /// Expected audio fingerprint.
    pub fingerprint: ContentFingerprint,
    /// Expected lyrics fingerprint.
    pub lyrics_fingerprint: Option<ContentFingerprint>,
    /// Destination audio path.
    pub destination_audio_path: RelPath,
    /// Destination record path.
    pub destination_record_path: RelPath,
    /// Catalogue generation at review.
    pub generation: u64,
}

/// Result of one committed track move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackMoveReport {
    /// Moved TrackId.
    pub track_id: TrackId,
    /// Source album, or `None` when it was standalone.
    pub from_album_id: Option<AlbumId>,
    /// Destination album, or `None` when it became standalone.
    pub to_album_id: Option<AlbumId>,
    /// Insertion index in the new destination order.
    pub insertion_index: usize,
    /// New managed audio path.
    pub audio_path: RelPath,
}

/// Immutable plan for replacing one track's managed audio.
#[derive(Debug, Clone)]
pub struct TrackAudioReplacementPlan {
    /// Target track.
    pub track_id: TrackId,
    /// Track title captured for review.
    pub title: String,
    /// Physical owner, which must not change.
    pub owner_artist_id: ArtistId,
    /// Album identity, which must not change. `None` means standalone.
    pub album_id: Option<AlbumId>,
    /// Expected ordered credits.
    pub expected_credited_artist_ids: Vec<ArtistId>,
    /// Expected track number.
    pub expected_track_number: Option<u32>,
    /// Expected disc number.
    pub expected_disc_number: Option<u32>,
    /// Expected duration.
    pub expected_duration_ms: Option<u64>,
    /// Existing managed audio path.
    pub current_audio_path: RelPath,
    /// Existing managed audio fingerprint; `None` means the file is missing
    /// and this plan repairs that missing resource.
    pub current_fingerprint: Option<ContentFingerprint>,
    /// Replacement source file, preserved by the operation.
    pub source: PathBuf,
    /// Replacement source fingerprint.
    pub source_fingerprint: ContentFingerprint,
    /// Staged destination path that will hold the new managed audio.
    pub destination_audio_path: RelPath,
    /// Existing record path preserved by the operation.
    pub record_path: RelPath,
    /// Catalogue generation at review.
    pub generation: u64,
}

/// Result of one committed audio replacement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackAudioReplacementReport {
    /// Stable track identity.
    pub track_id: TrackId,
    /// New managed audio path.
    pub audio_path: RelPath,
    /// New managed audio fingerprint.
    pub fingerprint: ContentFingerprint,
    /// Whether this operation repaired a previously missing managed audio file.
    pub restored_missing_audio: bool,
}

/// Immutable review plan for deleting one catalogue track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackDeletionPlan {
    /// Target track.
    pub track_id: TrackId,
    /// Captured title for review/confirmation text.
    pub title: String,
    /// Album identity retained even if it becomes empty, or `None` for standalone.
    pub album_id: Option<AlbumId>,
    /// Physical owner, retained even if it has no tracks left.
    pub owner_artist_id: ArtistId,
    /// Managed audio path at review.
    pub audio_path: RelPath,
    /// Whether managed audio existed at review.
    pub audio_present: bool,
    /// Managed audio fingerprint when the file was present and readable.
    pub audio_fingerprint: Option<ContentFingerprint>,
    /// Managed track record path.
    pub record_path: RelPath,
    /// Managed lyrics path, when recorded.
    pub lyrics_path: Option<RelPath>,
    /// Managed Track artwork override path, when recorded.
    pub artwork_path: Option<RelPath>,
    /// Playlist names referencing this track at review.
    pub playlists: Vec<String>,
    /// Catalogue generation at review.
    pub generation: u64,
}

/// Result of one committed single-track deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackDeletionReport {
    /// Deleted TrackId.
    pub track_id: TrackId,
    /// Album retained (possibly empty), or `None` for a standalone deletion.
    pub album_id: Option<AlbumId>,
    /// Whether catalogue audio existed and was requested for physical cleanup.
    pub audio_was_present: bool,
    /// Post-commit managed-resource cleanup failures. The catalogue deletion is
    /// still authoritative when this is non-empty.
    pub cleanup_errors: Vec<String>,
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
        if let Some(failure) = self.fail_scenario
            && failure.point == point
        {
            if failure.abort {
                std::process::abort();
            }
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

    fn create_transaction_directory(&self) -> Result<(String, PathBuf), CanonicalError> {
        let transactions = self.root.join(TRANSACTIONS_DIR_NAME);
        fs::create_dir_all(&transactions)?;
        for _ in 0..128 {
            let transaction_id = Self::next_transaction_id();
            if self.catalog.transaction_id.as_deref() == Some(transaction_id.as_str()) {
                continue;
            }
            let rel = transaction_rel(&transaction_id);
            let dir = safe_join(&self.root, &rel)?;
            match fs::create_dir(&dir) {
                Ok(()) => return Ok((transaction_id, dir)),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(CanonicalError::Invalid(
            "could not allocate a unique transaction ID".to_owned(),
        ))
    }

    fn cleanup_incomplete_transaction_setup(
        &self,
        txn_dir: &Path,
        transaction_id: &str,
        original: CanonicalError,
    ) -> CanonicalError {
        // A journal, even one whose durability sync reported an error, is a
        // recovery authority. Never delete it based on an ambiguous setup
        // failure.
        if txn_dir.join("journal.flux").exists() {
            return original;
        }
        let cleanup = (|| -> Result<(), CanonicalError> {
            match fs::remove_dir_all(txn_dir) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            sync_directory(&self.root.join(TRANSACTIONS_DIR_NAME))?;
            Ok(())
        })();
        match cleanup {
            Ok(()) => original,
            Err(cleanup_error) => CanonicalError::Transaction {
                file: transaction_id.to_owned(),
                message: format!(
                    "transaction setup failed before journal publication: {original}; \
                     cleanup of the incomplete transaction directory {} failed: {cleanup_error}",
                    txn_dir.display()
                ),
            },
        }
    }

    fn commit_writes(
        &mut self,
        new_catalog: Option<CatalogDto>,
        writes: Vec<PendingWrite>,
    ) -> Result<(), CanonicalError> {
        self.commit_writes_with_cleanup(new_catalog, writes, Vec::new())
    }

    fn ensure_no_pending_transactions(&self) -> Result<(), CanonicalError> {
        let dir = self.root.join(TRANSACTIONS_DIR_NAME);
        if !dir.exists() {
            return Ok(());
        }
        let mut entries = fs::read_dir(&dir)?.flatten();
        if entries.any(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir())) {
            return Err(CanonicalError::Transaction {
                file: TRANSACTIONS_DIR_NAME.to_owned(),
                message: "a previous transaction is unresolved; reopen the library and complete recovery before starting another write"
                    .to_owned(),
            });
        }
        Ok(())
    }

    fn commit_writes_with_cleanup(
        &mut self,
        new_catalog: Option<CatalogDto>,
        writes: Vec<PendingWrite>,
        cleanup: Vec<RelPath>,
    ) -> Result<(), CanonicalError> {
        self.maybe_fail(TransactionFailPoint::BeforeJournal)?;
        self.ensure_no_pending_transactions()?;
        let next_generation =
            self.catalog.generation.checked_add(1).ok_or_else(|| {
                CanonicalError::Invalid("catalogue generation overflow".to_owned())
            })?;
        let operation = if new_catalog.is_some() {
            "catalogue-write"
        } else {
            "metadata-write"
        };
        let (transaction_id, txn_dir) = self.create_transaction_directory()?;
        let setup_result = (|| -> Result<JournalDto, CanonicalError> {
            sync_directory(&self.root.join(TRANSACTIONS_DIR_NAME))?;
            self.maybe_fail(TransactionFailPoint::AfterTransactionDirectory)?;
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
            self.maybe_fail(TransactionFailPoint::AfterBackups)?;
            let journal = JournalDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: JOURNAL_KIND.to_owned(),
                transaction_id: transaction_id.clone(),
                operation: operation.to_owned(),
                state: JournalState::Prepared,
                intended_generation: next_generation,
                actions,
                cleanup: cleanup.iter().map(RelPath::display_lossy).collect(),
            };
            self.maybe_fail(TransactionFailPoint::BeforeJournalWrite)?;
            write_journal(&self.root, &journal)?;
            Ok(journal)
        })();
        let mut journal = match setup_result {
            Ok(journal) => journal,
            Err(err) => {
                return Err(self.cleanup_incomplete_transaction_setup(
                    &txn_dir,
                    &transaction_id,
                    err,
                ));
            }
        };
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
            let mut catalog = new_catalog.unwrap_or_else(|| self.catalog.clone());
            catalog.generation = next_generation;
            catalog.transaction_id = Some(transaction_id.clone());
            let catalog_path = self.root.join(CATALOG_FILE_NAME);
            if catalog_path.exists() {
                let prior = fs::read(&catalog_path)?;
                let backup_name = format!("catalog-gen-{}.flux", self.catalog.generation);
                write_atomic(&self.root.join(BACKUPS_DIR_NAME).join(backup_name), &prior)?;
                write_atomic(&self.root.join(CATALOG_BACKUP_FILE_NAME), &prior)?;
            }
            let bytes =
                serde_json::to_vec_pretty(&catalog).map_err(|err| CanonicalError::Malformed {
                    file: CATALOG_FILE_NAME.to_owned(),
                    message: err.to_string(),
                })?;
            write_atomic(&catalog_path, &bytes)?;
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
            description: None,
            image: None,
        };
        let mut catalog = self.catalog.clone();
        catalog.next_artist_id = advance_counter(catalog.next_artist_id, "artist")?;
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
            description: artist.description,
            image: artist.image,
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

    /// Updates an artist's display name and optional description while
    /// preserving aliases, image and every relationship.
    pub fn update_artist_metadata(
        &mut self,
        id: ArtistId,
        name: impl Into<String>,
        description: Option<String>,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let artist =
            self.artists.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("artist {id} is not in the library"))
            })?;
        let name = name.into().trim().to_owned();
        if name.is_empty() {
            return Err(CanonicalError::Invalid(
                "artist name must not be empty".to_owned(),
            ));
        }
        if self
            .artists
            .values()
            .any(|other| other.id != id && other.name == name)
        {
            return Err(CanonicalError::Invalid(format!(
                "artist name {name:?} is already in use"
            )));
        }
        let description = description
            .map(|description| description.trim().to_owned())
            .filter(|description| !description.is_empty());
        let dto = ArtistEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ARTIST_KIND.to_owned(),
            id: id.get(),
            name,
            aliases: artist.aliases,
            description,
            image: artist.image,
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

    /// Absolute path for an artist's managed image.
    #[must_use]
    pub fn artist_image_path(&self, id: ArtistId) -> Option<PathBuf> {
        let rel = self.artist_paths.get(&id)?;
        let image = self.artists.get(&id)?.image.as_ref()?;
        let os = rel.to_os_path()?;
        Some(self.root.join(os).join(image))
    }

    /// Writes or replaces an artist image. The caller has already validated
    /// decodability; this validates the managed extension/size and commits the
    /// file and entity update atomically.
    pub fn set_artist_image(
        &mut self,
        id: ArtistId,
        source: &Path,
        extension: &str,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let extension = validate_image_extension_size(source, extension, "artist image")?;
        let artist =
            self.artists.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("artist {id} is not in the library"))
            })?;
        let artist_rel = self
            .artist_paths
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing artist path".to_owned()))?;
        let image_name = format!("image.{extension}");
        let dto = ArtistEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ARTIST_KIND.to_owned(),
            id: id.get(),
            name: artist.name,
            aliases: artist.aliases,
            description: artist.description,
            image: Some(image_name.clone()),
        };
        let image_rel =
            RelPath::from_utf8(&format!("{}/{}", artist_rel.display_lossy(), image_name))?;
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &artist_rel, "artist.flux", &dto)?;
        writes.push(PendingWrite {
            target: image_rel,
            bytes: None,
            copy_from: Some(source.to_path_buf()),
            expected: Some(ContentFingerprint::of_file(source)?),
        });
        let mut cleanup = Vec::new();
        if let Some(old) = artist.image
            && old != image_name
        {
            cleanup.push(RelPath::from_utf8(&format!(
                "{}/{}",
                artist_rel.display_lossy(),
                old
            ))?);
        }
        self.commit_writes_with_cleanup(None, writes, cleanup)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Removes an artist image reference and its managed file after commit.
    pub fn remove_artist_image(&mut self, id: ArtistId) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let artist =
            self.artists.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("artist {id} is not in the library"))
            })?;
        let Some(old) = artist.image.clone() else {
            return Ok(());
        };
        let artist_rel = self
            .artist_paths
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing artist path".to_owned()))?;
        let dto = ArtistEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ARTIST_KIND.to_owned(),
            id: id.get(),
            name: artist.name,
            aliases: artist.aliases,
            description: artist.description,
            image: None,
        };
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &artist_rel, "artist.flux", &dto)?;
        let cleanup = vec![RelPath::from_utf8(&format!(
            "{}/{}",
            artist_rel.display_lossy(),
            old
        ))?];
        self.commit_writes_with_cleanup(None, writes, cleanup)?;
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
        if self
            .albums
            .values()
            .any(|album| album.owner_artist_id == owner && album.title == title)
        {
            return Err(CanonicalError::Invalid(format!(
                "artist {owner} already has an album titled {title:?}"
            )));
        }
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
        catalog.next_album_id = advance_counter(catalog.next_album_id, "album")?;
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

    /// Atomically imports reviewed tracks and every proposed artist/album in
    /// one canonical transaction. No artist or album is persisted unless the
    /// complete batch commits.
    pub fn import_tracks_batch(
        &mut self,
        expected_generation: u64,
        requests: Vec<CanonicalImportTrackRequest>,
    ) -> Result<Vec<TrackId>, CanonicalError> {
        self.import_review_batch(expected_generation, requests, Vec::new())
    }

    /// Atomically imports new tracks and applies approved metadata updates in
    /// one canonical transaction. Proposed artists and albums are staged only
    /// when referenced by this batch.
    pub fn import_review_batch(
        &mut self,
        expected_generation: u64,
        requests: Vec<CanonicalImportTrackRequest>,
        updates: Vec<CanonicalTrackMetadataUpdate>,
    ) -> Result<Vec<TrackId>, CanonicalError> {
        self.ensure_writable()?;
        if expected_generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "import plan is stale: catalogue generation changed; review again".to_owned(),
            ));
        }
        let mut catalog = self.catalog.clone();
        let mut artists = self.artists.clone();
        let mut albums = self.albums.clone();
        let mut writes = Vec::new();
        let mut new_artist_ids: BTreeMap<String, ArtistId> = BTreeMap::new();
        let mut new_album_ids: BTreeMap<(ArtistId, String), AlbumId> = BTreeMap::new();
        let mut album_orders: BTreeMap<AlbumId, Vec<TrackId>> = BTreeMap::new();
        let mut touched_albums: BTreeSet<AlbumId> = BTreeSet::new();
        let mut album_artwork_updates: BTreeMap<AlbumId, (String, CanonicalImportAsset)> =
            BTreeMap::new();
        let mut cleanup = Vec::new();
        let mut ids = Vec::with_capacity(requests.len());
        let mut fingerprints = self
            .tracks
            .values()
            .filter_map(|track| track.fingerprint)
            .collect::<HashSet<_>>();

        for request in requests {
            let source = request.source;
            let fingerprint = ContentFingerprint::of_file(&source)?;
            if request
                .expected_source_fingerprint
                .is_some_and(|expected| expected != fingerprint)
            {
                return Err(CanonicalError::Invalid(format!(
                    "source changed after review: {}",
                    source.display()
                )));
            }
            if !fingerprints.insert(fingerprint) && !request.allow_duplicate_content {
                return Err(CanonicalError::Invalid(format!(
                    "duplicate audio content in import batch: {}",
                    source.display()
                )));
            }
            let extension = source
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .ok_or_else(|| {
                    CanonicalError::Invalid("source has no usable extension".to_owned())
                })?;
            if extension.is_empty() || !extension.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(CanonicalError::Invalid(
                    "source extension is not a safe canonical suffix".to_owned(),
                ));
            }
            let title = request.title.trim().to_owned();
            if title.is_empty() {
                return Err(CanonicalError::Invalid(
                    "track title must not be empty".to_owned(),
                ));
            }
            if request.credited_artists.is_empty()
                || request.owner_index >= request.credited_artists.len()
            {
                return Err(CanonicalError::Invalid(
                    "import requires credits and a valid physical owner".to_owned(),
                ));
            }
            let credits = resolve_import_artists(
                &mut catalog,
                &mut artists,
                &mut new_artist_ids,
                &mut writes,
                request.credited_artists,
            )?;
            let owner = credits[request.owner_index];
            let album_id = match request.album {
                CanonicalImportAlbumRef::Standalone => None,
                CanonicalImportAlbumRef::Existing(id) => {
                    let album = albums.get(&id).ok_or_else(|| {
                        CanonicalError::Invalid(format!("album {id} is not in the library"))
                    })?;
                    if album.owner_artist_id != owner {
                        return Err(CanonicalError::Invalid(
                            "track owner must equal the existing album owner".to_owned(),
                        ));
                    }
                    Some(id)
                }
                CanonicalImportAlbumRef::ByName {
                    title,
                    release_type,
                } => {
                    let title = title.trim().to_owned();
                    if title.is_empty() {
                        return Err(CanonicalError::Invalid(
                            "album title must not be empty".to_owned(),
                        ));
                    }
                    let key = (owner, title.clone());
                    if let Some(id) = new_album_ids.get(&key) {
                        Some(*id)
                    } else if let Some(existing) = albums
                        .values()
                        .find(|album| album.owner_artist_id == owner && album.title == title)
                    {
                        Some(existing.id)
                    } else {
                        let id = AlbumId::new(catalog.next_album_id);
                        catalog.next_album_id = advance_counter(catalog.next_album_id, "album")?;
                        let rel = album_rel(owner, id);
                        catalog.albums.push(CatalogAlbumDto {
                            id: id.get(),
                            artist_id: owner.get(),
                            path: rel.display_lossy(),
                        });
                        albums.insert(
                            id,
                            CanonicalAlbum {
                                id,
                                owner_artist_id: owner,
                                title: title.clone(),
                                release_type,
                                cover: None,
                                track_order: Vec::new(),
                            },
                        );
                        new_album_ids.insert(key, id);
                        Some(id)
                    }
                }
            };
            if request.album_artwork.is_some() && album_id.is_none() {
                return Err(CanonicalError::Invalid(
                    "Album artwork requires Album membership".to_owned(),
                ));
            }
            if let (Some(album_id), Some(asset)) = (album_id, request.album_artwork.as_ref()) {
                let extension = validate_import_asset(asset, "album artwork")?;
                let cover_name = format!("cover.{extension}");
                if let Some((existing_name, existing_asset)) = album_artwork_updates.get(&album_id)
                {
                    if existing_name != &cover_name
                        || existing_asset.fingerprint != asset.fingerprint
                    {
                        return Err(CanonicalError::Invalid(
                            "one import batch requested conflicting artwork for the same album"
                                .to_owned(),
                        ));
                    }
                } else {
                    album_artwork_updates.insert(album_id, (cover_name, asset.clone()));
                }
            }
            if let Some(album_id) = album_id
                && !albums.contains_key(&album_id)
            {
                return Err(CanonicalError::Invalid(
                    "import destination album disappeared".to_owned(),
                ));
            }
            let track_id = TrackId::new(catalog.next_track_id);
            catalog.next_track_id = advance_counter(catalog.next_track_id, "track")?;
            let audio = track_audio_rel(owner, album_id, track_id, &extension);
            let record = track_record_rel(owner, album_id, track_id);
            if safe_join(&self.root, &audio)?.exists() || safe_join(&self.root, &record)?.exists() {
                return Err(CanonicalError::Invalid(format!(
                    "destination collision for track {track_id}"
                )));
            }
            let artwork_name = if let Some(asset) = request.artwork.as_ref() {
                let extension = validate_import_asset(asset, "track artwork")?;
                let name = format!("track_{}.{}", track_id.get(), extension);
                writes.push(PendingWrite {
                    target: RelPath::from_utf8(&format!("{TRACK_ARTWORK_DIR_NAME}/{name}"))?,
                    bytes: None,
                    copy_from: Some(asset.source.clone()),
                    expected: Some(asset.fingerprint),
                });
                Some(name)
            } else {
                None
            };
            let lyrics_name = request
                .lyrics
                .as_ref()
                .map(|_| format!("track_{:06}.lrc", track_id.get()));
            if let Some(text) = request.lyrics.as_ref() {
                let target = RelPath::from_utf8(&format!(
                    "{}/{}",
                    record
                        .display_lossy()
                        .rsplit_once('/')
                        .map_or("", |(dir, _)| dir),
                    lyrics_name.as_deref().expect("lyrics name")
                ))?;
                writes.push(PendingWrite {
                    target,
                    bytes: Some(text.as_bytes().to_vec()),
                    copy_from: None,
                    expected: None,
                });
            }
            catalog.tracks.push(CatalogTrackDto {
                id: track_id.get(),
                owner_artist_id: owner.get(),
                album_id: album_id.map(|album| album.get()),
                audio_path: audio.display_lossy(),
                record_path: record.display_lossy(),
            });
            if let Some(album_id) = album_id {
                let album = albums
                    .get_mut(&album_id)
                    .expect("existing album was validated");
                album.track_order.push(track_id);
                album_orders.insert(album_id, album.track_order.clone());
                touched_albums.insert(album_id);
            }
            let dto = TrackEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: TRACK_KIND.to_owned(),
                id: track_id.get(),
                title,
                credited_artist_ids: credits.iter().map(|artist| artist.get()).collect(),
                owner_artist_id: owner.get(),
                album_id: album_id.map(|album| album.get()),
                track_number: request.track_number,
                disc_number: request.disc_number,
                duration_ms: request.duration_ms,
                lyrics: lyrics_name,
                artwork: artwork_name,
                fingerprint: Some(fingerprint),
                created_unix_ms: crate::portable::now_unix_ms(),
            };
            push_file_write(&mut writes, &record, &dto)?;
            writes.push(PendingWrite {
                target: audio,
                bytes: None,
                copy_from: Some(source),
                expected: Some(fingerprint),
            });
            ids.push(track_id);
        }
        for update in updates {
            let track = self.tracks.get(&update.track_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("track {} is not in the library", update.track_id))
            })?;
            if track.album_id != update.expected_album_id {
                return Err(CanonicalError::Invalid(
                    "track moved albums after review; review the conflict again".to_owned(),
                ));
            }
            if track.fingerprint != update.expected_fingerprint
                || track.title != update.expected_title
                || track.credited_artist_ids != update.expected_credited_artist_ids
                || track.track_number != update.expected_track_number
                || track.disc_number != update.expected_disc_number
                || track.duration_ms != update.expected_duration_ms
            {
                return Err(CanonicalError::Invalid(
                    "existing track metadata changed after review; review the conflict again"
                        .to_owned(),
                ));
            }
            let title = update.title.trim().to_owned();
            if title.is_empty() {
                return Err(CanonicalError::Invalid(
                    "track title must not be empty".to_owned(),
                ));
            }
            let credits = resolve_import_artists(
                &mut catalog,
                &mut artists,
                &mut new_artist_ids,
                &mut writes,
                update.credited_artists,
            )?;
            if credits.is_empty() || !credits.contains(&track.owner_artist_id) {
                return Err(CanonicalError::Invalid(
                    "metadata updates must keep the physical owner in ordered credits".to_owned(),
                ));
            }
            let record = self
                .track_records
                .get(&update.track_id)
                .cloned()
                .ok_or_else(|| CanonicalError::Invalid("missing track record path".to_owned()))?;
            if let Some(asset) = update.album_artwork.as_ref() {
                let Some(album_id) = track.album_id else {
                    return Err(CanonicalError::Invalid(
                        "Album artwork requires Album membership".to_owned(),
                    ));
                };
                let extension = validate_import_asset(asset, "album artwork")?;
                let cover_name = format!("cover.{extension}");
                if let Some((existing_name, existing_asset)) = album_artwork_updates.get(&album_id)
                {
                    if existing_name != &cover_name
                        || existing_asset.fingerprint != asset.fingerprint
                    {
                        return Err(CanonicalError::Invalid(
                            "one import batch requested conflicting artwork for the same album"
                                .to_owned(),
                        ));
                    }
                } else {
                    album_artwork_updates.insert(album_id, (cover_name, asset.clone()));
                }
                touched_albums.insert(album_id);
            }
            let artwork_name = if let Some(asset) = update.artwork.as_ref() {
                let extension = validate_import_asset(asset, "track artwork")?;
                let name = format!("track_{}.{}", track.id.get(), extension);
                writes.push(PendingWrite {
                    target: RelPath::from_utf8(&format!("{TRACK_ARTWORK_DIR_NAME}/{name}"))?,
                    bytes: None,
                    copy_from: Some(asset.source.clone()),
                    expected: Some(asset.fingerprint),
                });
                if let Some(old) = &track.artwork
                    && old != &name
                {
                    cleanup.push(RelPath::from_utf8(&format!(
                        "{TRACK_ARTWORK_DIR_NAME}/{old}"
                    ))?);
                }
                Some(name)
            } else {
                track.artwork.clone()
            };
            let lyrics_name = update
                .lyrics
                .as_ref()
                .map(|_| format!("track_{:06}.lrc", track.id.get()))
                .or_else(|| track.lyrics.clone());
            if let Some(text) = update.lyrics.as_ref() {
                let target = RelPath::from_utf8(&format!(
                    "{}/{}",
                    record
                        .display_lossy()
                        .rsplit_once('/')
                        .map_or("", |(dir, _)| dir),
                    lyrics_name.as_deref().expect("lyrics name")
                ))?;
                writes.push(PendingWrite {
                    target,
                    bytes: Some(text.as_bytes().to_vec()),
                    copy_from: None,
                    expected: None,
                });
            }
            let dto = TrackEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: TRACK_KIND.to_owned(),
                id: update.track_id.get(),
                title,
                credited_artist_ids: credits.iter().map(|artist| artist.get()).collect(),
                owner_artist_id: track.owner_artist_id.get(),
                album_id: track.album_id.map(|album| album.get()),
                track_number: update.track_number,
                disc_number: update.disc_number,
                duration_ms: update.duration_ms,
                lyrics: lyrics_name,
                artwork: artwork_name,
                fingerprint: track.fingerprint,
                created_unix_ms: track.created_unix_ms,
            };
            push_file_write(&mut writes, &record, &dto)?;
        }
        for album_id in touched_albums {
            let Some(album) = albums.get(&album_id) else {
                continue;
            };
            let mut dto = AlbumEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: ALBUM_KIND.to_owned(),
                id: album.id.get(),
                owner_artist_id: album.owner_artist_id.get(),
                title: album.title.clone(),
                release_type: album.release_type,
                cover: album.cover.clone(),
                track_order: album.track_order.iter().map(|track| track.get()).collect(),
            };
            if let Some(order) = album_orders.get(&album_id) {
                dto.track_order = order.iter().map(|track| track.get()).collect();
            }
            if let Some((cover_name, asset)) = album_artwork_updates.get(&album_id) {
                dto.cover = Some(cover_name.clone());
                let album_path = album_rel(album.owner_artist_id, album.id);
                writes.push(PendingWrite {
                    target: RelPath::from_utf8(&format!(
                        "{}/{}",
                        album_path.display_lossy(),
                        cover_name
                    ))?,
                    bytes: None,
                    copy_from: Some(asset.source.clone()),
                    expected: Some(asset.fingerprint),
                });
                if let Some(old) = &album.cover
                    && old != cover_name
                {
                    cleanup.push(RelPath::from_utf8(&format!(
                        "{}/{}",
                        album_path.display_lossy(),
                        old
                    ))?);
                }
            }
            push_entity_write(
                &mut writes,
                &album_rel(album.owner_artist_id, album.id),
                "album.flux",
                &dto,
            )?;
        }
        catalog.artists.sort_by_key(|entry| entry.id);
        catalog.albums.sort_by_key(|entry| entry.id);
        catalog.tracks.sort_by_key(|entry| entry.id);
        self.commit_writes_with_cleanup(Some(catalog), writes, cleanup)?;
        self.reopen_after()?;
        Ok(ids)
    }

    /// Imports multiple reviewed tracks into an existing album as one canonical
    /// transaction, preserving album metadata/cover and append order.
    pub fn import_tracks_into_album(
        &mut self,
        target: &CanonicalAlbumImportTarget,
        imported: Vec<CanonicalImportedTrack>,
    ) -> Result<Vec<TrackId>, CanonicalError> {
        self.ensure_writable()?;
        if target.generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "album import plan is stale: catalogue generation changed; review again".to_owned(),
            ));
        }
        let album = self.albums.get(&target.album_id).cloned().ok_or_else(|| {
            CanonicalError::Invalid("target album is no longer in the library".to_owned())
        })?;
        if album.owner_artist_id != target.owner_artist_id
            || album.title != target.title
            || album.release_type != target.release_type
            || album.cover.as_deref() != target.cover.as_deref()
            || album.track_order != target.track_order
        {
            return Err(CanonicalError::Invalid(
                "target album changed since review; review again".to_owned(),
            ));
        }
        let mut catalog = self.catalog.clone();
        let mut album_order = album.track_order.clone();
        let mut writes = Vec::new();
        let mut ids = Vec::with_capacity(imported.len());
        let mut fingerprints = self
            .tracks
            .values()
            .filter_map(|track| track.fingerprint)
            .collect::<HashSet<_>>();
        for imported in imported {
            let source = imported.source;
            let fingerprint = ContentFingerprint::of_file(&source)?;
            if !fingerprints.insert(fingerprint) {
                return Err(CanonicalError::Invalid(format!(
                    "duplicate audio content in album import: {}",
                    source.display()
                )));
            }
            let extension = source
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ext.to_ascii_lowercase())
                .ok_or_else(|| {
                    CanonicalError::Invalid("source has no usable extension".to_owned())
                })?;
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
            if imported.artist_id != Some(album.owner_artist_id) {
                return Err(CanonicalError::Invalid(
                    "target album imports must use the album owner as physical owner".to_owned(),
                ));
            }
            let credits = if imported.credited_artist_ids.is_empty() {
                vec![album.owner_artist_id]
            } else {
                imported.credited_artist_ids
            };
            let unique = credits.iter().collect::<HashSet<_>>();
            if unique.len() != credits.len()
                || !credits.contains(&album.owner_artist_id)
                || credits
                    .iter()
                    .any(|artist| !self.artists.contains_key(artist))
            {
                return Err(CanonicalError::Invalid(
                    "import credits must be unique, known and include the album owner".to_owned(),
                ));
            }
            let track_id = TrackId::new(catalog.next_track_id);
            catalog.next_track_id = advance_counter(catalog.next_track_id, "track")?;
            let audio = track_audio_rel(
                album.owner_artist_id,
                Some(target.album_id),
                track_id,
                &extension,
            );
            let record = track_record_rel(album.owner_artist_id, Some(target.album_id), track_id);
            catalog.tracks.push(CatalogTrackDto {
                id: track_id.get(),
                owner_artist_id: album.owner_artist_id.get(),
                album_id: Some(target.album_id.get()),
                audio_path: audio.display_lossy(),
                record_path: record.display_lossy(),
            });
            let dto = TrackEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: TRACK_KIND.to_owned(),
                id: track_id.get(),
                title,
                credited_artist_ids: credits.iter().map(|artist| artist.get()).collect(),
                owner_artist_id: album.owner_artist_id.get(),
                album_id: Some(target.album_id.get()),
                track_number: imported.track_number,
                disc_number: imported.disc_number,
                duration_ms: imported.duration_ms,
                lyrics: None,
                artwork: None,
                fingerprint: Some(fingerprint),
                created_unix_ms: crate::portable::now_unix_ms(),
            };
            push_file_write(&mut writes, &record, &dto)?;
            writes.push(PendingWrite {
                target: audio,
                bytes: None,
                copy_from: Some(source),
                expected: Some(fingerprint),
            });
            album_order.push(track_id);
            ids.push(track_id);
        }
        let album_rel_path = self.album_paths[&target.album_id].clone();
        let album_dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: album.id.get(),
            owner_artist_id: album.owner_artist_id.get(),
            title: album.title,
            release_type: album.release_type,
            cover: album.cover,
            track_order: album_order.iter().map(|track| track.get()).collect(),
        };
        push_entity_write(&mut writes, &album_rel_path, "album.flux", &album_dto)?;
        self.commit_writes(Some(catalog), writes)?;
        self.reopen_after()?;
        Ok(ids)
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
            .filter(|title| !title.is_empty());

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
                catalog.next_artist_id = advance_counter(catalog.next_artist_id, "artist")?;
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
                    description: None,
                    image: None,
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

        let mut album = if let Some(album_title) = album_title {
            let existing = catalog.albums.iter().find_map(|entry| {
                let id = AlbumId::new(entry.id);
                self.albums
                    .get(&id)
                    .filter(|album| {
                        album.owner_artist_id == artist_id && album.title == album_title
                    })
                    .cloned()
                    .map(|album| (id, album))
            });
            Some(if let Some(existing) = existing {
                existing
            } else {
                let id = AlbumId::new(catalog.next_album_id);
                catalog.next_album_id = advance_counter(catalog.next_album_id, "album")?;
                let rel = album_rel(artist_id, id);
                catalog.albums.push(CatalogAlbumDto {
                    id: id.get(),
                    artist_id: artist_id.get(),
                    path: rel.display_lossy(),
                });
                (
                    id,
                    CanonicalAlbum {
                        id,
                        owner_artist_id: artist_id,
                        title: album_title,
                        release_type: imported.release_type,
                        cover: None,
                        track_order: Vec::new(),
                    },
                )
            })
        } else {
            None
        };
        let album_id = album.as_ref().map(|(id, _)| *id);

        let track_id = TrackId::new(catalog.next_track_id);
        catalog.next_track_id = advance_counter(catalog.next_track_id, "track")?;
        let audio_rel = track_audio_rel(artist_id, album_id, track_id, &extension);
        let record_rel = track_record_rel(artist_id, album_id, track_id);
        catalog.tracks.push(CatalogTrackDto {
            id: track_id.get(),
            owner_artist_id: artist_id.get(),
            album_id: album_id.map(|album| album.get()),
            audio_path: audio_rel.display_lossy(),
            record_path: record_rel.display_lossy(),
        });

        if let Some((album_id, album)) = album.as_mut() {
            album.track_order.push(track_id);
            let album_dto = AlbumEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: ALBUM_KIND.to_owned(),
                id: album_id.get(),
                owner_artist_id: artist_id.get(),
                title: album.title.clone(),
                release_type: album.release_type,
                cover: album.cover.clone(),
                track_order: album.track_order.iter().map(|id| id.get()).collect(),
            };
            push_entity_write(
                &mut writes,
                &album_rel(artist_id, *album_id),
                "album.flux",
                &album_dto,
            )?;
        }
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
            album_id: album_id.map(|album| album.get()),
            track_number: imported.track_number,
            disc_number: imported.disc_number,
            duration_ms: imported.duration_ms,
            lyrics: None,
            artwork: None,
            fingerprint: Some(fingerprint),
            created_unix_ms: crate::portable::now_unix_ms(),
        };
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

    /// Captures a metadata-edit snapshot for stale-plan validation.
    pub fn plan_track_metadata_update(
        &self,
        id: TrackId,
    ) -> Result<TrackMetadataUpdatePlan, CanonicalError> {
        let track = self
            .tracks
            .get(&id)
            .ok_or_else(|| CanonicalError::Invalid(format!("track {id} is not in the library")))?;
        let expected_album_title = match track.album_id {
            Some(album_id) => Some(
                self.albums
                    .get(&album_id)
                    .ok_or_else(|| {
                        CanonicalError::Invalid(
                            "track album is missing from the library".to_owned(),
                        )
                    })?
                    .title
                    .clone(),
            ),
            None => None,
        };
        Ok(TrackMetadataUpdatePlan {
            track_id: id,
            album_id: track.album_id,
            owner_artist_id: track.owner_artist_id,
            generation: self.catalog.generation,
            expected_title: track.title.clone(),
            expected_credited_artist_ids: track.credited_artist_ids.clone(),
            expected_track_number: track.track_number,
            expected_disc_number: track.disc_number,
            expected_duration_ms: track.duration_ms,
            expected_fingerprint: track.fingerprint,
            expected_album_title,
        })
    }

    /// Applies an approved metadata edit, including an album-title rename, as
    /// one transaction. Ownership and album identity never change.
    pub fn execute_track_metadata_update(
        &mut self,
        plan: &TrackMetadataUpdatePlan,
        title: impl Into<String>,
        credited_artist_ids: Vec<ArtistId>,
        track_number: Option<u32>,
        disc_number: Option<u32>,
        album_title: Option<String>,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        if plan.generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "metadata edit plan is stale: catalogue generation changed; reopen the editor"
                    .to_owned(),
            ));
        }
        let track = self.tracks.get(&plan.track_id).cloned().ok_or_else(|| {
            CanonicalError::Invalid(format!("track {} is not in the library", plan.track_id))
        })?;
        let album = match plan.album_id {
            Some(album_id) => Some(self.albums.get(&album_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid("track album disappeared since review".to_owned())
            })?),
            None => None,
        };
        if track.album_id != plan.album_id
            || track.owner_artist_id != plan.owner_artist_id
            || track.title != plan.expected_title
            || track.credited_artist_ids != plan.expected_credited_artist_ids
            || track.track_number != plan.expected_track_number
            || track.disc_number != plan.expected_disc_number
            || track.duration_ms != plan.expected_duration_ms
            || track.fingerprint != plan.expected_fingerprint
            || album.as_ref().map(|album| &album.title) != plan.expected_album_title.as_ref()
            || album
                .as_ref()
                .is_some_and(|album| album.owner_artist_id != track.owner_artist_id)
        {
            return Err(CanonicalError::Invalid(
                "track or album changed since review; reopen the editor".to_owned(),
            ));
        }
        let title = title.into().trim().to_owned();
        if title.is_empty() {
            return Err(CanonicalError::Invalid(
                "track title must not be empty".to_owned(),
            ));
        }
        let album_title = match (album.as_ref(), album_title) {
            (None, _) => None,
            (Some(_), Some(title)) => {
                let title = title.trim().to_owned();
                if title.is_empty() {
                    return Err(CanonicalError::Invalid(
                        "album title must not be empty".to_owned(),
                    ));
                }
                Some(title)
            }
            (Some(_), None) => plan.expected_album_title.clone(),
        };
        if let (Some(album), Some(album_title)) = (album.as_ref(), album_title.as_ref())
            && album.title != *album_title
            && self.albums.values().any(|candidate| {
                candidate.id != album.id
                    && candidate.owner_artist_id == album.owner_artist_id
                    && candidate.title == *album_title
            })
        {
            return Err(CanonicalError::Invalid(format!(
                "owner already has an album titled {album_title:?}"
            )));
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
        let record_rel = self
            .track_records
            .get(&track.id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing track record path".to_owned()))?;
        let track_dto = TrackEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: TRACK_KIND.to_owned(),
            id: track.id.get(),
            title,
            credited_artist_ids: credited_artist_ids
                .iter()
                .map(|artist| artist.get())
                .collect(),
            owner_artist_id: track.owner_artist_id.get(),
            album_id: track.album_id.map(|album| album.get()),
            track_number,
            disc_number,
            duration_ms: track.duration_ms,
            lyrics: track.lyrics,
            artwork: track.artwork.clone(),
            fingerprint: track.fingerprint,
            created_unix_ms: track.created_unix_ms,
        };
        let mut writes = Vec::new();
        push_file_write(&mut writes, &record_rel, &track_dto)?;
        if let (Some(album), Some(album_title)) = (album, album_title)
            && album.title != album_title
        {
            let album_dto = AlbumEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: ALBUM_KIND.to_owned(),
                id: album.id.get(),
                owner_artist_id: album.owner_artist_id.get(),
                title: album_title,
                release_type: album.release_type,
                cover: album.cover,
                track_order: album.track_order.iter().map(|id| id.get()).collect(),
            };
            push_entity_write(
                &mut writes,
                &album_rel(album.owner_artist_id, album.id),
                "album.flux",
                &album_dto,
            )?;
        }
        let catalog = self.catalog.clone();
        self.commit_writes(Some(catalog), writes)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Plans an explicit move into an existing destination album owned by the
    /// same physical owner, or `None` to detach the Track as standalone.
    pub fn plan_track_move(
        &self,
        track_id: TrackId,
        to_album_id: Option<AlbumId>,
        insertion_index: usize,
    ) -> Result<TrackMovePlan, CanonicalError> {
        let track = self.tracks.get(&track_id).ok_or_else(|| {
            CanonicalError::Invalid(format!("track {track_id} is not in the library"))
        })?;
        let from_album =
            match track.album_id {
                Some(album_id) => Some(self.albums.get(&album_id).ok_or_else(|| {
                    CanonicalError::Invalid("source album is missing".to_owned())
                })?),
                None => None,
            };
        let to_album = match to_album_id {
            Some(album_id) => Some(self.albums.get(&album_id).ok_or_else(|| {
                CanonicalError::Invalid("destination album is missing".to_owned())
            })?),
            None => None,
        };
        if track.album_id == to_album_id {
            return Err(CanonicalError::Invalid(
                "track already has that Album membership".to_owned(),
            ));
        }
        if let Some(to_album) = to_album
            && to_album.owner_artist_id != track.owner_artist_id
        {
            return Err(CanonicalError::Invalid(
                "destination album owner must equal the track's physical owner".to_owned(),
            ));
        }
        if let Some(to_album) = to_album
            && insertion_index > to_album.track_order.len()
        {
            return Err(CanonicalError::Invalid(
                "insertion position is outside the destination album order".to_owned(),
            ));
        }
        let audio_path = self
            .track_paths
            .get(&track_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("track audio path is missing".to_owned()))?;
        let record_path =
            self.track_records.get(&track_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid("track record path is missing".to_owned())
            })?;
        let old_audio_abs = safe_join(&self.root, &audio_path)?;
        if !old_audio_abs.is_file() {
            return Err(CanonicalError::Invalid(
                "track audio is missing; relink or replace it before moving".to_owned(),
            ));
        }
        let fingerprint = ContentFingerprint::of_file(&old_audio_abs)
            .map_err(|_| CanonicalError::Invalid("track audio cannot be read".to_owned()))?;
        let lyrics_path = track
            .lyrics
            .as_ref()
            .and_then(|lyrics| plan_track_lyrics_rel(&record_path, Some(lyrics)));
        let lyrics_fingerprint = match &lyrics_path {
            Some(path) => Some(ContentFingerprint::of_file(&safe_join(&self.root, path)?)?),
            None => None,
        };
        let extension = audio_path
            .display_lossy()
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_owned())
            .ok_or_else(|| CanonicalError::Invalid("audio has no extension".to_owned()))?;
        let destination_audio_path =
            track_audio_rel(track.owner_artist_id, to_album_id, track_id, &extension);
        let destination_record_path =
            track_record_rel(track.owner_artist_id, to_album_id, track_id);
        if safe_join(&self.root, &destination_audio_path)?.exists()
            || safe_join(&self.root, &destination_record_path)?.exists()
            || plan_track_lyrics_rel(&destination_record_path, track.lyrics.as_deref())
                .is_some_and(|path| safe_join(&self.root, &path).is_ok_and(|path| path.exists()))
        {
            return Err(CanonicalError::Invalid(
                "destination track path collides with an existing resource".to_owned(),
            ));
        }
        let expected_to_order = to_album
            .map(|album| album.track_order.clone())
            .unwrap_or_default();
        let mut destination_order = expected_to_order.clone();
        if to_album.is_some() {
            destination_order.insert(insertion_index, track_id);
        }
        Ok(TrackMovePlan {
            track_id,
            title: track.title.clone(),
            owner_artist_id: track.owner_artist_id,
            from_album_id: track.album_id,
            from_album_title: from_album.map(|album| album.title.clone()),
            to_album_id,
            to_album_title: to_album.map(|album| album.title.clone()),
            insertion_index,
            expected_from_order: from_album
                .map(|album| album.track_order.clone())
                .unwrap_or_default(),
            expected_to_order,
            destination_order,
            expected_credited_artist_ids: track.credited_artist_ids.clone(),
            expected_track_number: track.track_number,
            expected_disc_number: track.disc_number,
            expected_duration_ms: track.duration_ms,
            audio_path,
            record_path,
            lyrics_path,
            fingerprint,
            lyrics_fingerprint,
            destination_audio_path,
            destination_record_path,
            generation: self.catalog.generation,
        })
    }

    /// Executes a reviewed track move as one canonical transaction. The source
    /// album remains even when it becomes empty.
    pub fn execute_track_move(
        &mut self,
        plan: &TrackMovePlan,
    ) -> Result<TrackMoveReport, CanonicalError> {
        self.ensure_writable()?;
        self.ensure_no_pending_transactions()?;
        if plan.generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "track move plan is stale: catalogue generation changed; re-plan".to_owned(),
            ));
        }
        let track = self.tracks.get(&plan.track_id).cloned().ok_or_else(|| {
            CanonicalError::Invalid("track disappeared since move preview".to_owned())
        })?;
        let from_album = match plan.from_album_id {
            Some(album_id) => Some(self.albums.get(&album_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid("source album disappeared since move preview".to_owned())
            })?),
            None => None,
        };
        let to_album = match plan.to_album_id {
            Some(album_id) => Some(self.albums.get(&album_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(
                    "destination album disappeared since move preview".to_owned(),
                )
            })?),
            None => None,
        };
        let from_matches = match (from_album.as_ref(), plan.from_album_id) {
            (Some(album), Some(_)) => {
                album.owner_artist_id == plan.owner_artist_id
                    && plan.from_album_title.as_ref() == Some(&album.title)
                    && album.track_order == plan.expected_from_order
            }
            (None, None) => true,
            _ => false,
        };
        let to_matches = match (to_album.as_ref(), plan.to_album_id) {
            (Some(album), Some(_)) => {
                album.owner_artist_id == plan.owner_artist_id
                    && plan.to_album_title.as_ref() == Some(&album.title)
                    && album.track_order == plan.expected_to_order
                    && plan.insertion_index <= album.track_order.len()
            }
            (None, None) => plan.destination_order.is_empty(),
            _ => false,
        };
        if track.album_id != plan.from_album_id
            || track.owner_artist_id != plan.owner_artist_id
            || track.title != plan.title
            || track.credited_artist_ids != plan.expected_credited_artist_ids
            || track.track_number != plan.expected_track_number
            || track.disc_number != plan.expected_disc_number
            || track.duration_ms != plan.expected_duration_ms
            || self.track_paths.get(&track.id) != Some(&plan.audio_path)
            || self.track_records.get(&track.id) != Some(&plan.record_path)
            || !from_matches
            || !to_matches
        {
            return Err(CanonicalError::Invalid(
                "track or album order changed since move preview; re-plan".to_owned(),
            ));
        }
        let old_audio_abs = safe_join(&self.root, &plan.audio_path)?;
        if ContentFingerprint::of_file(&old_audio_abs)? != plan.fingerprint {
            return Err(CanonicalError::Invalid(
                "track audio changed since move preview; re-plan".to_owned(),
            ));
        }
        let current_lyrics = track
            .lyrics
            .as_ref()
            .and_then(|lyrics| plan_track_lyrics_rel(&plan.record_path, Some(lyrics)));
        let current_lyrics_fingerprint = match &current_lyrics {
            Some(path) => Some(ContentFingerprint::of_file(&safe_join(&self.root, path)?)?),
            None => None,
        };
        if current_lyrics != plan.lyrics_path
            || current_lyrics_fingerprint != plan.lyrics_fingerprint
        {
            return Err(CanonicalError::Invalid(
                "track lyrics changed since move preview; re-plan".to_owned(),
            ));
        }
        if safe_join(&self.root, &plan.destination_audio_path)?.exists()
            || safe_join(&self.root, &plan.destination_record_path)?.exists()
        {
            return Err(CanonicalError::Invalid(
                "destination collided after move preview; re-plan".to_owned(),
            ));
        }
        let mut from_order = plan.expected_from_order.clone();
        from_order.retain(|id| *id != track.id);
        let to_order = plan.destination_order.clone();
        if plan.to_album_id.is_some() && to_order.get(plan.insertion_index) != Some(&track.id) {
            return Err(CanonicalError::Invalid(
                "destination order no longer matches the reviewed insertion".to_owned(),
            ));
        }
        let new_lyrics = track
            .lyrics
            .as_ref()
            .and_then(|lyrics| plan_track_lyrics_rel(&plan.destination_record_path, Some(lyrics)));
        let track_dto = TrackEntityDto {
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
            album_id: plan.to_album_id.map(|album| album.get()),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            lyrics: track.lyrics.clone(),
            artwork: track.artwork.clone(),
            fingerprint: Some(plan.fingerprint),
            created_unix_ms: track.created_unix_ms,
        };
        let mut writes = Vec::new();
        push_file_write(&mut writes, &plan.destination_record_path, &track_dto)?;
        writes.push(PendingWrite {
            target: plan.destination_audio_path.clone(),
            bytes: None,
            copy_from: Some(old_audio_abs),
            expected: Some(plan.fingerprint),
        });
        if let (Some(old), Some(new)) = (&plan.lyrics_path, &new_lyrics) {
            writes.push(PendingWrite {
                target: new.clone(),
                bytes: Some(fs::read(safe_join(&self.root, old)?)?),
                copy_from: None,
                expected: None,
            });
        }
        if let Some(from_album) = from_album {
            let from_dto = AlbumEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: ALBUM_KIND.to_owned(),
                id: from_album.id.get(),
                owner_artist_id: from_album.owner_artist_id.get(),
                title: from_album.title,
                release_type: from_album.release_type,
                cover: from_album.cover,
                track_order: from_order.iter().map(|id| id.get()).collect(),
            };
            push_entity_write(
                &mut writes,
                &album_rel(from_album.owner_artist_id, from_album.id),
                "album.flux",
                &from_dto,
            )?;
        }
        if let Some(to_album) = to_album {
            let to_dto = AlbumEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: ALBUM_KIND.to_owned(),
                id: to_album.id.get(),
                owner_artist_id: to_album.owner_artist_id.get(),
                title: to_album.title,
                release_type: to_album.release_type,
                cover: to_album.cover,
                track_order: to_order.iter().map(|id| id.get()).collect(),
            };
            push_entity_write(
                &mut writes,
                &album_rel(to_album.owner_artist_id, to_album.id),
                "album.flux",
                &to_dto,
            )?;
        }
        let mut catalog = self.catalog.clone();
        let entry = catalog
            .tracks
            .iter_mut()
            .find(|entry| entry.id == track.id.get())
            .ok_or_else(|| CanonicalError::Invalid("catalogue track missing".to_owned()))?;
        entry.album_id = plan.to_album_id.map(|album| album.get());
        entry.audio_path = plan.destination_audio_path.display_lossy();
        entry.record_path = plan.destination_record_path.display_lossy();
        let mut cleanup = vec![plan.audio_path.clone(), plan.record_path.clone()];
        if let Some(lyrics) = &plan.lyrics_path {
            cleanup.push(lyrics.clone());
        }
        self.commit_writes_with_cleanup(Some(catalog), writes, cleanup)?;
        self.reopen_after()?;
        Ok(TrackMoveReport {
            track_id: track.id,
            from_album_id: plan.from_album_id,
            to_album_id: plan.to_album_id,
            insertion_index: plan.insertion_index,
            audio_path: plan.destination_audio_path.clone(),
        })
    }

    /// Builds an immutable plan for replacing managed audio without modifying
    /// the existing file or catalogue until the commit point.
    pub fn plan_track_audio_replacement(
        &self,
        track_id: TrackId,
        source: &Path,
    ) -> Result<TrackAudioReplacementPlan, CanonicalError> {
        let track = self.tracks.get(&track_id).ok_or_else(|| {
            CanonicalError::Invalid(format!("track {track_id} is not in the library"))
        })?;
        let current_audio_path = self
            .track_paths
            .get(&track_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("track audio path is missing".to_owned()))?;
        let record_path =
            self.track_records.get(&track_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid("track record path is missing".to_owned())
            })?;
        let current_audio_abs = safe_join(&self.root, &current_audio_path)?;
        let current_present = current_audio_abs.is_file();
        if !source.is_file() {
            return Err(CanonicalError::Invalid(
                "replacement source is not a readable file".to_owned(),
            ));
        }
        if fs::metadata(source)?.len() == 0 {
            return Err(CanonicalError::Invalid(
                "replacement source is empty".to_owned(),
            ));
        }
        if !crate::is_supported_audio_path(source) {
            return Err(CanonicalError::Invalid(
                "replacement source is not a supported audio file".to_owned(),
            ));
        }
        if current_present
            && fs::canonicalize(&current_audio_abs).ok() == fs::canonicalize(source).ok()
        {
            return Err(CanonicalError::Invalid(
                "replacement source is the current managed audio".to_owned(),
            ));
        }
        let extension = source
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .ok_or_else(|| CanonicalError::Invalid("replacement has no extension".to_owned()))?;
        if extension.is_empty() || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(CanonicalError::Invalid(
                "replacement extension is not a safe canonical suffix".to_owned(),
            ));
        }
        let current_fingerprint = current_present
            .then(|| ContentFingerprint::of_file(&current_audio_abs))
            .transpose()?;
        let source_fingerprint = ContentFingerprint::of_file(source)?;
        if current_fingerprint == Some(source_fingerprint) {
            return Err(CanonicalError::Invalid(
                "replacement source has identical audio content".to_owned(),
            ));
        }
        let mut destination_audio_path = None;
        for attempt in 0..10_000u64 {
            let candidate = track_replacement_audio_rel(
                track.owner_artist_id,
                track.album_id,
                track.id,
                &extension,
                self.catalog.generation.saturating_add(attempt),
            );
            if !safe_join(&self.root, &candidate)?.exists() {
                destination_audio_path = Some(candidate);
                break;
            }
        }
        let destination_audio_path = destination_audio_path.ok_or_else(|| {
            CanonicalError::Invalid("could not allocate a replacement destination".to_owned())
        })?;
        Ok(TrackAudioReplacementPlan {
            track_id,
            title: track.title.clone(),
            owner_artist_id: track.owner_artist_id,
            album_id: track.album_id,
            expected_credited_artist_ids: track.credited_artist_ids.clone(),
            expected_track_number: track.track_number,
            expected_disc_number: track.disc_number,
            expected_duration_ms: track.duration_ms,
            current_audio_path,
            current_fingerprint,
            source: source.to_path_buf(),
            source_fingerprint,
            destination_audio_path,
            record_path,
            generation: self.catalog.generation,
        })
    }

    /// Executes a reviewed audio replacement. New audio is staged and verified;
    /// the old audio is cleaned only after the catalogue commit point.
    pub fn execute_track_audio_replacement(
        &mut self,
        plan: &TrackAudioReplacementPlan,
    ) -> Result<TrackAudioReplacementReport, CanonicalError> {
        self.ensure_writable()?;
        self.ensure_no_pending_transactions()?;
        if plan.generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "replacement plan is stale: catalogue generation changed; re-plan".to_owned(),
            ));
        }
        let track = self.tracks.get(&plan.track_id).cloned().ok_or_else(|| {
            CanonicalError::Invalid("track disappeared since replacement preview".to_owned())
        })?;
        if track.album_id != plan.album_id
            || track.owner_artist_id != plan.owner_artist_id
            || track.title != plan.title
            || track.credited_artist_ids != plan.expected_credited_artist_ids
            || track.track_number != plan.expected_track_number
            || track.disc_number != plan.expected_disc_number
            || track.duration_ms != plan.expected_duration_ms
            || self.track_paths.get(&track.id) != Some(&plan.current_audio_path)
            || self.track_records.get(&track.id) != Some(&plan.record_path)
        {
            return Err(CanonicalError::Invalid(
                "track changed since replacement preview; re-plan".to_owned(),
            ));
        }
        let current_audio_abs = safe_join(&self.root, &plan.current_audio_path)?;
        match plan.current_fingerprint {
            Some(expected) => {
                if ContentFingerprint::of_file(&current_audio_abs)? != expected {
                    return Err(CanonicalError::Invalid(
                        "current managed audio changed since replacement preview; re-plan"
                            .to_owned(),
                    ));
                }
            }
            None => {
                if current_audio_abs.is_file() {
                    return Err(CanonicalError::Invalid(
                        "missing managed audio reappeared since preview; re-plan".to_owned(),
                    ));
                }
            }
        }
        if ContentFingerprint::of_file(&plan.source)? != plan.source_fingerprint {
            return Err(CanonicalError::Invalid(
                "replacement source changed since preview; re-plan".to_owned(),
            ));
        }
        if safe_join(&self.root, &plan.destination_audio_path)?.exists() {
            return Err(CanonicalError::Invalid(
                "replacement destination collided after preview; re-plan".to_owned(),
            ));
        }
        let track_dto = TrackEntityDto {
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
            album_id: track.album_id.map(|album| album.get()),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            lyrics: track.lyrics.clone(),
            artwork: track.artwork.clone(),
            fingerprint: Some(plan.source_fingerprint),
            created_unix_ms: track.created_unix_ms,
        };
        let mut writes = Vec::new();
        push_file_write(&mut writes, &plan.record_path, &track_dto)?;
        writes.push(PendingWrite {
            target: plan.destination_audio_path.clone(),
            bytes: None,
            copy_from: Some(plan.source.clone()),
            expected: Some(plan.source_fingerprint),
        });
        let mut catalog = self.catalog.clone();
        let entry = catalog
            .tracks
            .iter_mut()
            .find(|entry| entry.id == track.id.get())
            .ok_or_else(|| CanonicalError::Invalid("catalogue track missing".to_owned()))?;
        entry.audio_path = plan.destination_audio_path.display_lossy();
        self.commit_writes_with_cleanup(
            Some(catalog),
            writes,
            vec![plan.current_audio_path.clone()],
        )?;
        self.reopen_after()?;
        Ok(TrackAudioReplacementReport {
            track_id: track.id,
            audio_path: plan.destination_audio_path.clone(),
            fingerprint: plan.source_fingerprint,
            restored_missing_audio: plan.current_fingerprint.is_none(),
        })
    }

    /// Builds a read-only review plan for deleting exactly one track. The
    /// album, artist and all unrelated identities/resources are retained.
    pub fn plan_track_deletion(
        &self,
        track_id: TrackId,
    ) -> Result<TrackDeletionPlan, CanonicalError> {
        let track = self.tracks.get(&track_id).ok_or_else(|| {
            CanonicalError::Invalid(format!("track {track_id} is not in the library"))
        })?;
        let audio_path = self
            .track_paths
            .get(&track_id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("track audio path is missing".to_owned()))?;
        let record_path =
            self.track_records.get(&track_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid("track record path is missing".to_owned())
            })?;
        let audio_abs = safe_join(&self.root, &audio_path)?;
        let audio_present = audio_abs.is_file();
        let audio_fingerprint = if audio_present {
            ContentFingerprint::of_file(&audio_abs).ok()
        } else {
            None
        };
        let lyrics_path = plan_track_lyrics_rel(&record_path, track.lyrics.as_deref());
        let artwork_path = track
            .artwork
            .as_ref()
            .map(|artwork| RelPath::from_utf8(&format!("{TRACK_ARTWORK_DIR_NAME}/{artwork}")))
            .transpose()?;
        let mut playlists = self
            .playlists
            .values()
            .filter(|playlist| playlist.track_ids.contains(&track_id))
            .map(|playlist| playlist.name.clone())
            .collect::<Vec<_>>();
        playlists.sort();
        Ok(TrackDeletionPlan {
            track_id,
            title: track.title.clone(),
            album_id: track.album_id,
            owner_artist_id: track.owner_artist_id,
            audio_path,
            audio_present,
            audio_fingerprint,
            record_path,
            lyrics_path,
            artwork_path,
            playlists,
            generation: self.catalog.generation,
        })
    }

    /// Deletes one catalogue track atomically, retaining its album and artist.
    /// Managed audio/record/lyrics cleanup runs only after the catalogue commit
    /// point and reports failures without rolling the committed deletion back.
    pub fn delete_track(
        &mut self,
        plan: &TrackDeletionPlan,
    ) -> Result<TrackDeletionReport, CanonicalError> {
        self.ensure_writable()?;
        self.ensure_no_pending_transactions()?;
        if plan.generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "track deletion plan is stale: catalogue generation changed; re-plan".to_owned(),
            ));
        }
        let track = self.tracks.get(&plan.track_id).cloned().ok_or_else(|| {
            CanonicalError::Invalid("track disappeared since deletion review".to_owned())
        })?;
        if track.title != plan.title
            || track.album_id != plan.album_id
            || track.owner_artist_id != plan.owner_artist_id
            || self.track_paths.get(&track.id) != Some(&plan.audio_path)
            || self.track_records.get(&track.id) != Some(&plan.record_path)
        {
            return Err(CanonicalError::Invalid(
                "track changed since deletion review; re-plan".to_owned(),
            ));
        }
        let audio_abs = safe_join(&self.root, &plan.audio_path)?;
        match (plan.audio_present, plan.audio_fingerprint) {
            (true, Some(expected)) => {
                if !audio_abs.is_file() || ContentFingerprint::of_file(&audio_abs)? != expected {
                    return Err(CanonicalError::Invalid(
                        "managed audio changed since deletion review; re-plan".to_owned(),
                    ));
                }
            }
            (true, None) => {
                if !audio_abs.is_file() {
                    return Err(CanonicalError::Invalid(
                        "managed audio disappeared since deletion review; re-plan".to_owned(),
                    ));
                }
            }
            (false, _) => {
                if audio_abs.is_file() {
                    return Err(CanonicalError::Invalid(
                        "previously missing managed audio reappeared; re-plan deletion".to_owned(),
                    ));
                }
            }
        }
        if plan_track_lyrics_rel(&plan.record_path, track.lyrics.as_deref()) != plan.lyrics_path {
            return Err(CanonicalError::Invalid(
                "track lyrics reference changed since deletion review; re-plan".to_owned(),
            ));
        }

        let mut catalog = self.catalog.clone();
        let mut albums = self.albums.clone();
        let mut ratings = self.ratings.clone();
        let mut playlists = self.playlists.clone();
        let mut writes = Vec::new();

        if let Some(album_id) = track.album_id {
            let album = albums.get_mut(&album_id).ok_or_else(|| {
                CanonicalError::Invalid("track album disappeared during deletion".to_owned())
            })?;
            album.track_order.retain(|track_id| *track_id != track.id);
            let album_dto = AlbumEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: ALBUM_KIND.to_owned(),
                id: album.id.get(),
                owner_artist_id: album.owner_artist_id.get(),
                title: album.title.clone(),
                release_type: album.release_type,
                cover: album.cover.clone(),
                track_order: album.track_order.iter().map(|id| id.get()).collect(),
            };
            push_entity_write(
                &mut writes,
                &album_rel(album.owner_artist_id, album.id),
                "album.flux",
                &album_dto,
            )?;
        }

        for playlist in playlists.values_mut() {
            playlist.track_ids.retain(|track_id| *track_id != track.id);
        }
        for playlist in playlists.values() {
            if self.playlists.get(&playlist.id) != Some(playlist) {
                let dto = PlaylistEntityDto {
                    schema_version: CANONICAL_SCHEMA_VERSION,
                    kind: PLAYLIST_KIND.to_owned(),
                    id: playlist.id.get(),
                    name: playlist.name.clone(),
                    track_ids: playlist.track_ids.iter().map(|id| id.get()).collect(),
                };
                push_file_write(&mut writes, &playlist_rel(playlist.id), &dto)?;
            }
        }
        ratings.remove(&track.id);
        let ratings_dto = RatingsDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: RATINGS_KIND.to_owned(),
            entries: ratings
                .iter()
                .map(|(track_id, rating)| RatingEntryDto {
                    track_id: track_id.get(),
                    rating: rating.rating,
                    favorite: rating.favorite,
                })
                .collect(),
        };
        let ratings_bytes =
            serde_json::to_vec_pretty(&ratings_dto).map_err(|err| CanonicalError::Malformed {
                file: RATINGS_FILE_NAME.to_owned(),
                message: err.to_string(),
            })?;
        writes.push(PendingWrite {
            target: RelPath::from_utf8(RATINGS_FILE_NAME)?,
            bytes: Some(ratings_bytes),
            copy_from: None,
            expected: None,
        });
        catalog.tracks.retain(|entry| entry.id != track.id.get());
        self.commit_writes_with_cleanup(Some(catalog), writes, Vec::new())?;
        self.reopen_after()?;

        let mut cleanup_errors = Vec::new();
        for path in [
            Some(plan.audio_path.clone()),
            Some(plan.record_path.clone()),
            plan.lyrics_path.clone(),
            plan.artwork_path.clone(),
        ]
        .into_iter()
        .flatten()
        {
            if let Err(error) = self.cleanup_unreferenced_track_resource(&path) {
                cleanup_errors.push(error);
            }
        }
        Ok(TrackDeletionReport {
            track_id: track.id,
            album_id: track.album_id,
            audio_was_present: plan.audio_present,
            cleanup_errors,
        })
    }

    fn cleanup_unreferenced_track_resource(&self, target: &RelPath) -> Result<(), String> {
        let text = target.display_lossy();
        if self
            .catalog
            .tracks
            .iter()
            .any(|track| track.audio_path == text || track.record_path == text)
        {
            return Err(format!(
                "kept managed resource still referenced by the catalogue: {text}"
            ));
        }
        if self.tracks.values().any(|track| {
            self.track_records
                .get(&track.id)
                .and_then(|record| plan_track_lyrics_rel(record, track.lyrics.as_deref()))
                .is_some_and(|lyrics| lyrics == *target)
        }) {
            return Err(format!(
                "kept managed lyrics still referenced by another track: {text}"
            ));
        }
        reject_symlink_escape(&self.root, target).map_err(|error| error.to_string())?;
        let path = safe_join(&self.root, target).map_err(|error| error.to_string())?;
        match fs::remove_file(&path) {
            Ok(()) => {
                if let Some(parent) = path.parent() {
                    sync_directory(parent).map_err(|error| error.to_string())?;
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("could not remove {}: {error}", path.display())),
        }
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
            album_id: track.album_id.map(|album| album.get()),
            track_number,
            disc_number,
            duration_ms,
            lyrics: track.lyrics,
            artwork: track.artwork.clone(),
            fingerprint: track.fingerprint,
            created_unix_ms: track.created_unix_ms,
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
        let old_cover = album.cover.clone();
        let mut writes = Vec::new();
        push_entity_write(&mut writes, &album_rel_path, "album.flux", &album_dto)?;
        writes.push(PendingWrite {
            target: cover_rel,
            bytes: None,
            copy_from: Some(source.to_path_buf()),
            expected: Some(ContentFingerprint::of_file(source)?),
        });
        let mut cleanup = Vec::new();
        if let Some(old) = old_cover
            && old != cover_name
        {
            cleanup.push(RelPath::from_utf8(&format!(
                "{}/{}",
                album_rel_path.display_lossy(),
                old
            ))?);
        }
        self.commit_writes_with_cleanup(None, writes, cleanup)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Absolute path for a Track-specific artwork override, when present.
    #[must_use]
    pub fn track_artwork_path(&self, id: TrackId) -> Option<PathBuf> {
        let artwork = self.tracks.get(&id)?.artwork.as_ref()?;
        Some(self.root.join(TRACK_ARTWORK_DIR_NAME).join(artwork))
    }

    /// Writes or replaces a Track-specific artwork override. Album artwork
    /// remains untouched and is the fallback when this is absent.
    pub fn set_track_artwork(
        &mut self,
        id: TrackId,
        source: &Path,
        extension: &str,
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let extension = validate_image_extension_size(source, extension, "track artwork")?;
        let track =
            self.tracks.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("track {id} is not in the library"))
            })?;
        let record_rel =
            self.track_records.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid("track record path is missing".to_owned())
            })?;
        let artwork_name = format!("track_{}.{}", id.get(), extension);
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
            album_id: track.album_id.map(|album| album.get()),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            lyrics: track.lyrics,
            artwork: Some(artwork_name.clone()),
            fingerprint: track.fingerprint,
            created_unix_ms: track.created_unix_ms,
        };
        let artwork_rel = RelPath::from_utf8(&format!("{TRACK_ARTWORK_DIR_NAME}/{artwork_name}"))?;
        let mut writes = Vec::new();
        push_file_write(&mut writes, &record_rel, &dto)?;
        writes.push(PendingWrite {
            target: artwork_rel,
            bytes: None,
            copy_from: Some(source.to_path_buf()),
            expected: Some(ContentFingerprint::of_file(source)?),
        });
        let mut cleanup = Vec::new();
        if let Some(old) = track.artwork
            && old != artwork_name
        {
            cleanup.push(RelPath::from_utf8(&format!(
                "{TRACK_ARTWORK_DIR_NAME}/{old}"
            ))?);
        }
        self.commit_writes_with_cleanup(None, writes, cleanup)?;
        self.reopen_after()?;
        Ok(())
    }

    /// Removes a Track-specific artwork override; Album artwork fallback is
    /// unaffected.
    pub fn remove_track_artwork(&mut self, id: TrackId) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let track =
            self.tracks.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("track {id} is not in the library"))
            })?;
        let Some(old) = track.artwork.clone() else {
            return Ok(());
        };
        let record_rel =
            self.track_records.get(&id).cloned().ok_or_else(|| {
                CanonicalError::Invalid("track record path is missing".to_owned())
            })?;
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
            album_id: track.album_id.map(|album| album.get()),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            lyrics: track.lyrics,
            artwork: None,
            fingerprint: track.fingerprint,
            created_unix_ms: track.created_unix_ms,
        };
        let mut writes = Vec::new();
        push_file_write(&mut writes, &record_rel, &dto)?;
        let cleanup = vec![RelPath::from_utf8(&format!(
            "{TRACK_ARTWORK_DIR_NAME}/{old}"
        ))?];
        self.commit_writes_with_cleanup(None, writes, cleanup)?;
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
            album_id: track.album_id.map(|album| album.get()),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            lyrics: lyrics_name.clone(),
            artwork: track.artwork.clone(),
            fingerprint: track.fingerprint,
            created_unix_ms: track.created_unix_ms,
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
        let mut cleanup = Vec::new();
        if lyrics.is_none()
            && let Some(old) = &track.lyrics
        {
            cleanup.push(RelPath::from_utf8(&format!(
                "{}/{}",
                record_rel
                    .display_lossy()
                    .rsplit_once('/')
                    .map_or("", |(dir, _)| dir),
                old
            ))?);
        }
        let catalog = self.catalog.clone();
        self.commit_writes_with_cleanup(Some(catalog), writes, cleanup)?;
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
            catalog.next_playlist_id = advance_counter(catalog.next_playlist_id, "playlist")?;
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

    /// Deletes only the playlist entity and its metadata file. Tracks, albums,
    /// artists, ratings, queue entries and media are untouched.
    pub fn delete_playlist(&mut self, id: PlaylistId) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        if !self.playlists.contains_key(&id) {
            return Err(CanonicalError::Invalid(format!(
                "playlist {id} is not in the library"
            )));
        }
        let path = self
            .playlist_paths
            .get(&id)
            .cloned()
            .ok_or_else(|| CanonicalError::Invalid("missing playlist path".to_owned()))?;
        let mut catalog = self.catalog.clone();
        catalog.playlists.retain(|entry| entry.id != id.get());
        self.commit_writes_with_cleanup(Some(catalog), Vec::new(), vec![path])?;
        self.reopen_after()?;
        Ok(())
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

    /// Applies a batch of favorite/rating updates in one canonical
    /// transaction. Every TrackId is validated before any write.
    pub fn set_ratings(
        &mut self,
        updates: &[(TrackId, Option<u8>, bool)],
    ) -> Result<(), CanonicalError> {
        self.ensure_writable()?;
        let mut seen = HashSet::new();
        for (id, rating, _) in updates {
            if !seen.insert(*id) {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} appears more than once in the batch"
                )));
            }
            if !self.tracks.contains_key(id) {
                return Err(CanonicalError::Invalid(format!(
                    "track {id} is not in the library"
                )));
            }
            if rating.is_some_and(|rating| rating > 5) {
                return Err(CanonicalError::Invalid("rating must be 0..=5".to_owned()));
            }
        }
        let mut ratings = self.ratings.clone();
        for (id, rating, favorite) in updates {
            ratings.insert(
                *id,
                TrackRating {
                    rating: *rating,
                    favorite: *favorite,
                },
            );
        }
        let entries = ratings
            .iter()
            .map(|(track, value)| RatingEntryDto {
                track_id: track.get(),
                rating: value.rating,
                favorite: value.favorite,
            })
            .collect::<Vec<_>>();
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
        let catalog = self.catalog.clone();
        self.commit_writes(Some(catalog), writes)?;
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
        AlbumId, CANONICAL_SCHEMA_VERSION, CATALOG_FILE_NAME, CanonicalError,
        CanonicalImportAlbumRef, CanonicalImportArtistRef, CanonicalImportTrackRequest,
        CanonicalImportedTrack, CanonicalLibrary, ContentFingerprint, RATINGS_FILE_NAME,
        ReleaseType, TRANSACTIONS_DIR_NAME, TrackRating, TransactionFailPoint, TransactionFailure,
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

    fn metadata_fixture(
        name: &str,
    ) -> (
        TempDir,
        PathBuf,
        CanonicalLibrary,
        ArtistId,
        AlbumId,
        TrackId,
    ) {
        let dir = TempDir::new(name);
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let track = import(&dir, &mut library, "one.wav");
        let track_ref = library.track(track).expect("track");
        let artist = track_ref.owner_artist_id;
        let album = track_ref.album_id.expect("album");
        (dir, root, library, artist, album, track)
    }

    fn transaction_directory_count(root: &Path) -> usize {
        let dir = root.join(TRANSACTIONS_DIR_NAME);
        fs::read_dir(dir)
            .expect("transactions directory")
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .count()
    }

    fn rewrite_catalog(root: &Path, mutate: impl FnOnce(&mut serde_json::Value)) {
        let path = root.join(CATALOG_FILE_NAME);
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("catalog")).expect("catalog json");
        mutate(&mut catalog);
        fs::write(
            &path,
            serde_json::to_vec_pretty(&catalog).expect("catalog bytes"),
        )
        .expect("write catalog");
    }

    #[derive(Clone, Copy, Debug)]
    enum MetadataStageOutcome {
        SetupFailed,
        RolledBack,
        Committed,
    }

    #[test]
    fn metadata_only_transactions_recover_at_every_journal_stage() {
        use MetadataStageOutcome::{Committed, RolledBack, SetupFailed};

        let cases = [
            (TransactionFailPoint::BeforeJournal, SetupFailed),
            (TransactionFailPoint::AfterTransactionDirectory, SetupFailed),
            (TransactionFailPoint::AfterBackups, SetupFailed),
            (TransactionFailPoint::BeforeJournalWrite, SetupFailed),
            (TransactionFailPoint::AfterJournal, RolledBack),
            (TransactionFailPoint::BeforeStaging, RolledBack),
            (TransactionFailPoint::DuringCopy, RolledBack),
            (TransactionFailPoint::AfterCopyBeforeVerify, RolledBack),
            (TransactionFailPoint::AfterVerify, RolledBack),
            (TransactionFailPoint::BeforeMetadataPublish, RolledBack),
            (TransactionFailPoint::DuringMetadataPublish, RolledBack),
            (TransactionFailPoint::BeforeCatalogPublish, RolledBack),
            (TransactionFailPoint::AfterCatalogPublish, Committed),
            (TransactionFailPoint::BeforeCleanup, Committed),
            (TransactionFailPoint::AfterCleanup, Committed),
            (TransactionFailPoint::BeforeJournalFinalize, Committed),
        ];
        for (point, outcome) in cases {
            let (_dir, root, mut library, artist, _album, _track) =
                metadata_fixture(&format!("metadata-stage-{point:?}"));
            let generation = library.generation();
            let prior_transaction = library.catalog.transaction_id.clone();
            library.set_transaction_failure(Some(TransactionFailure {
                point,
                interrupt: true,
                abort: false,
            }));
            let error = library
                .rename_artist(artist, "Renamed")
                .expect_err("failure point must interrupt the metadata transaction");
            assert!(
                error.to_string().contains("injected transaction failure"),
                "{point:?}: {error}"
            );
            if matches!(outcome, SetupFailed) {
                assert_eq!(
                    transaction_directory_count(&root),
                    0,
                    "{point:?}: setup failure left a blocking transaction directory"
                );
            }
            drop(library);

            let reopened = CanonicalLibrary::open(&root)
                .unwrap_or_else(|err| panic!("{point:?}: recovery failed: {err}"));
            let expected_name = if matches!(outcome, Committed) {
                "Renamed"
            } else {
                "Artist One"
            };
            assert_eq!(
                reopened.artist(artist).map(|value| value.name.as_str()),
                Some(expected_name),
                "{point:?}: metadata was neither fully committed nor fully rolled back"
            );
            assert_eq!(
                reopened.generation(),
                if matches!(outcome, Committed) {
                    generation + 1
                } else {
                    generation
                },
                "{point:?}: catalogue generation did not follow the commit point"
            );
            if matches!(outcome, Committed) {
                assert!(
                    reopened.catalog.transaction_id.is_some()
                        && reopened.catalog.transaction_id != prior_transaction,
                    "{point:?}: committed metadata transaction was not recorded in the catalogue"
                );
            } else {
                assert_eq!(
                    reopened.catalog.transaction_id, prior_transaction,
                    "{point:?}: rolled-back metadata transaction replaced the catalogue commit identity"
                );
            }
            assert_eq!(
                transaction_directory_count(&root),
                0,
                "{point:?}: transaction directory was not finalized"
            );
            drop(reopened);
            assert!(
                CanonicalLibrary::open(&root).is_ok(),
                "{point:?}: second recovery was not idempotent"
            );
        }
    }

    #[test]
    fn metadata_only_cleanup_stage_recovers_and_advances_generation() {
        let (dir, root, mut library, _artist, album, _track) =
            metadata_fixture("metadata-cleanup-stage");
        let first = dir.write("cover-one.png", b"first cover");
        library
            .set_cover(album, &first, "png")
            .expect("first cover");
        let generation = library.generation();
        let second = dir.write("cover-two.jpg", b"second cover");
        library.set_transaction_failure(Some(TransactionFailure {
            point: TransactionFailPoint::DuringCleanup,
            interrupt: true,
            abort: false,
        }));
        library
            .set_cover(album, &second, "jpg")
            .expect_err("cleanup failure must interrupt the metadata transaction");
        drop(library);

        let reopened = CanonicalLibrary::open(&root).expect("cleanup recovery");
        assert_eq!(
            reopened.album(album).and_then(|album| album.cover.clone()),
            Some("cover.jpg".to_owned())
        );
        assert_eq!(reopened.generation(), generation + 1);
        let old_cover = root.join("artists/artist_000001/albums/album_000001/cover.png");
        assert!(!old_cover.exists(), "old cover was not cleaned up");
        assert_eq!(transaction_directory_count(&root), 0);
        drop(reopened);
        assert!(CanonicalLibrary::open(&root).is_ok());
    }

    #[test]
    fn setup_failures_remove_only_the_current_transaction_directory() {
        for point in [
            TransactionFailPoint::BeforeJournal,
            TransactionFailPoint::AfterTransactionDirectory,
            TransactionFailPoint::AfterBackups,
            TransactionFailPoint::BeforeJournalWrite,
        ] {
            let (_dir, root, mut library, artist, _album, _track) =
                metadata_fixture(&format!("metadata-setup-{point:?}"));
            let marker = root.join(TRANSACTIONS_DIR_NAME).join("unrelated.keep");
            fs::write(&marker, b"keep").expect("marker");
            let backup_marker = root.join("backups").join("unrelated.keep");
            fs::write(&backup_marker, b"keep").expect("backup marker");
            let catalog_before = fs::read(root.join(CATALOG_FILE_NAME)).expect("catalog");
            let generation = library.generation();

            library.set_transaction_failure(Some(TransactionFailure {
                point,
                interrupt: true,
                abort: false,
            }));
            let error = library
                .rename_artist(artist, "Should Not Commit")
                .expect_err("setup failure must abort the transaction");
            assert!(
                error.to_string().contains("injected transaction failure"),
                "{point:?}: {error}"
            );
            assert_eq!(
                transaction_directory_count(&root),
                0,
                "{point:?}: partial transaction directory remained"
            );
            assert!(marker.is_file(), "{point:?}: unrelated marker removed");
            assert!(
                backup_marker.is_file(),
                "{point:?}: unrelated backup removed"
            );
            assert_eq!(
                fs::read(root.join(CATALOG_FILE_NAME)).expect("catalog"),
                catalog_before,
                "{point:?}: catalogue changed before the journal was durable"
            );
            assert_eq!(library.generation(), generation);

            library.set_transaction_failure(None);
            library
                .rename_artist(artist, "Committed After Retry")
                .expect("a later transaction must succeed");
            assert_eq!(library.generation(), generation + 1);
            assert!(marker.is_file());
            assert!(backup_marker.is_file());
        }
    }

    #[test]
    fn setup_cleanup_preserves_a_partially_published_journal() {
        let (_dir, root, library, _artist, _album, _track) =
            metadata_fixture("metadata-setup-journal-preserve");
        let txn = root.join(TRANSACTIONS_DIR_NAME).join("txn-partial-journal");
        fs::create_dir(&txn).expect("transaction dir");
        fs::write(txn.join("journal.flux"), br#"{"partial":true}"#).expect("partial journal");
        let result = library.cleanup_incomplete_transaction_setup(
            &txn,
            "txn-partial-journal",
            CanonicalError::Invalid("simulated setup failure".to_owned()),
        );
        assert!(result.to_string().contains("simulated setup failure"));
        assert!(txn.join("journal.flux").is_file());
        assert!(
            txn.is_dir(),
            "any published journal is preserved for recovery"
        );
        fs::remove_dir_all(&txn).expect("remove test transaction");
    }

    #[test]
    fn setup_cleanup_removes_only_the_current_transaction_directory() {
        let (_dir, root, library, _artist, _album, _track) =
            metadata_fixture("metadata-setup-current-only");
        let transactions = root.join(TRANSACTIONS_DIR_NAME);
        let current = transactions.join("txn-current");
        let other = transactions.join("txn-other");
        fs::create_dir(&current).expect("current txn");
        fs::create_dir(&other).expect("other txn");
        fs::write(current.join("backup-0"), b"current backup").expect("current backup");
        fs::write(other.join("valuable.bin"), b"other txn data").expect("other data");
        let result = library.cleanup_incomplete_transaction_setup(
            &current,
            "txn-current",
            CanonicalError::Invalid("simulated setup failure".to_owned()),
        );
        assert!(result.to_string().contains("simulated setup failure"));
        assert!(!current.exists(), "current setup directory was not removed");
        assert!(other.is_dir(), "another transaction directory was removed");
        assert_eq!(
            fs::read(other.join("valuable.bin")).expect("other data"),
            b"other txn data"
        );
    }

    #[cfg(unix)]
    #[test]
    fn setup_cleanup_failure_reports_both_errors() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, root, library, _artist, _album, _track) =
            metadata_fixture("metadata-setup-cleanup-failure");
        let transactions = root.join(TRANSACTIONS_DIR_NAME);
        let txn = transactions.join("txn-cleanup-failure");
        fs::create_dir(&txn).expect("transaction dir");
        fs::write(txn.join("backup-0"), b"backup").expect("backup");
        let mode = fs::metadata(&transactions)
            .expect("transactions metadata")
            .permissions()
            .mode()
            & 0o777;
        fs::set_permissions(&transactions, fs::Permissions::from_mode(0o555))
            .expect("make transactions read-only");
        let result = library.cleanup_incomplete_transaction_setup(
            &txn,
            "txn-cleanup-failure",
            CanonicalError::Invalid("simulated setup failure".to_owned()),
        );
        fs::set_permissions(&transactions, fs::Permissions::from_mode(mode))
            .expect("restore transactions permissions");
        let message = result.to_string();
        assert!(message.contains("simulated setup failure"), "{message}");
        assert!(
            message.contains("cleanup of the incomplete transaction directory"),
            "{message}"
        );
        assert!(
            txn.is_dir(),
            "incomplete directory must be preserved on cleanup failure"
        );
        fs::remove_dir_all(&txn).expect("remove test transaction");
    }

    #[test]
    fn counters_must_not_reuse_ids_and_corruption_fails_before_writing() {
        #[derive(Clone, Copy, Debug)]
        enum CounterMutation {
            ReuseArtist,
            ReuseAlbum,
            ReuseTrack,
            ReusePlaylist,
            ZeroArtist,
            MaxTrack,
            DuplicateArtist,
        }

        let mutations = [
            CounterMutation::ReuseArtist,
            CounterMutation::ReuseAlbum,
            CounterMutation::ReuseTrack,
            CounterMutation::ReusePlaylist,
            CounterMutation::ZeroArtist,
            CounterMutation::MaxTrack,
            CounterMutation::DuplicateArtist,
        ];
        for mutation in mutations {
            let (dir, root, mut library, artist, album, track) =
                metadata_fixture(&format!("counter-{mutation:?}"));
            let playlist = library
                .set_playlist(None, "Playlist", vec![track])
                .expect("playlist");
            drop(library);
            rewrite_catalog(&root, |catalog| match mutation {
                CounterMutation::ReuseArtist => {
                    catalog["next_artist_id"] = serde_json::json!(artist.get());
                }
                CounterMutation::ReuseAlbum => {
                    catalog["next_album_id"] = serde_json::json!(album.get());
                }
                CounterMutation::ReuseTrack => {
                    catalog["next_track_id"] = serde_json::json!(track.get());
                }
                CounterMutation::ReusePlaylist => {
                    catalog["next_playlist_id"] = serde_json::json!(playlist.get());
                }
                CounterMutation::ZeroArtist => {
                    catalog["next_artist_id"] = serde_json::json!(0);
                }
                CounterMutation::MaxTrack => {
                    catalog["next_track_id"] = serde_json::json!(u64::MAX);
                }
                CounterMutation::DuplicateArtist => {
                    let duplicate = catalog["artists"][0].clone();
                    catalog["artists"]
                        .as_array_mut()
                        .expect("artists array ")
                        .push(duplicate);
                }
            });
            let catalog_path = root.join(CATALOG_FILE_NAME);
            let before = fs::read(&catalog_path).expect("catalog");
            let error = CanonicalLibrary::open(&root)
                .err()
                .unwrap_or_else(|| panic!("{mutation:?}: corrupted counters must not open"));
            let expected = match mutation {
                CounterMutation::ReuseArtist => "next_artist_id",
                CounterMutation::ReuseAlbum => "next_album_id",
                CounterMutation::ReuseTrack => "next_track_id",
                CounterMutation::ReusePlaylist => "next_playlist_id",
                CounterMutation::ZeroArtist => "next_artist_id is 0",
                CounterMutation::MaxTrack => "u64::MAX",
                CounterMutation::DuplicateArtist => "appears more than once",
            };
            assert!(
                error.to_string().contains(expected),
                "{mutation:?}: expected {expected:?} in actionable error, got: {error}"
            );
            assert_eq!(
                fs::read(&catalog_path).expect("catalog"),
                before,
                "{mutation:?}: opening a corrupt catalogue must not write"
            );
            drop(dir);
        }
    }

    #[test]
    fn valid_counter_gaps_open_and_exhausted_allocation_fails_without_writing() {
        let (_dir, root, library, _artist, _album, _track) = metadata_fixture("counter-valid-gaps");
        drop(library);
        rewrite_catalog(&root, |catalog| {
            catalog["next_artist_id"] = serde_json::json!(100);
            catalog["next_album_id"] = serde_json::json!(100);
            catalog["next_track_id"] = serde_json::json!(100);
            catalog["next_playlist_id"] = serde_json::json!(100);
        });
        let reopened = CanonicalLibrary::open(&root).expect("gapped counters open");
        assert_eq!(reopened.next_artist_id(), 100);
        assert_eq!(reopened.next_album_id(), 100);
        assert_eq!(reopened.next_track_id(), 100);
        assert_eq!(reopened.next_playlist_id(), 100);
        drop(reopened);

        rewrite_catalog(&root, |catalog| {
            catalog["next_artist_id"] = serde_json::json!(u64::MAX - 1);
        });
        let mut reopened = CanonicalLibrary::open(&root).expect("boundary counter opens");
        let generation = reopened.generation();
        let before = fs::read(root.join(CATALOG_FILE_NAME)).expect("catalog");
        let error = reopened
            .create_artist("Exhausted", Vec::new())
            .expect_err("allocation must refuse to create an unopenable counter state");
        assert!(error.to_string().contains("exhausted"), "{error}");
        assert_eq!(reopened.generation(), generation);
        assert_eq!(
            fs::read(root.join(CATALOG_FILE_NAME)).expect("catalog"),
            before
        );
    }

    #[test]
    fn ratings_required_collection_and_fields_fail_closed_without_rewriting() {
        #[derive(Clone, Copy, Debug)]
        enum RatingMutation {
            MissingEntries,
            WrongEntriesType,
            MissingTrackId,
            MissingFavorite,
            RatingTooHigh,
            DuplicateTrack,
            MissingTrack,
            FutureVersion,
            WrongKind,
        }

        let (_dir, root, mut library, _artist, _album, track) =
            metadata_fixture("ratings-fail-closed");
        library.set_rating(track, Some(4), true).expect("rating");
        drop(library);
        let ratings_path = root.join(RATINGS_FILE_NAME);
        let base: serde_json::Value =
            serde_json::from_slice(&fs::read(&ratings_path).expect("ratings"))
                .expect("ratings json");
        let mutations = [
            RatingMutation::MissingEntries,
            RatingMutation::WrongEntriesType,
            RatingMutation::MissingTrackId,
            RatingMutation::MissingFavorite,
            RatingMutation::RatingTooHigh,
            RatingMutation::DuplicateTrack,
            RatingMutation::MissingTrack,
            RatingMutation::FutureVersion,
            RatingMutation::WrongKind,
        ];
        for mutation in mutations {
            let mut value = base.clone();
            match mutation {
                RatingMutation::MissingEntries => {
                    value.as_object_mut().expect("object").remove("entries");
                }
                RatingMutation::WrongEntriesType => {
                    value["entries"] = serde_json::json!({});
                }
                RatingMutation::MissingTrackId => {
                    value["entries"][0]
                        .as_object_mut()
                        .expect("entry")
                        .remove("track_id");
                }
                RatingMutation::MissingFavorite => {
                    value["entries"][0]
                        .as_object_mut()
                        .expect("entry")
                        .remove("favorite");
                }
                RatingMutation::RatingTooHigh => {
                    value["entries"][0]["rating"] = serde_json::json!(6);
                }
                RatingMutation::DuplicateTrack => {
                    let entry = value["entries"][0].clone();
                    value["entries"]
                        .as_array_mut()
                        .expect("entries")
                        .push(entry);
                }
                RatingMutation::MissingTrack => {
                    value["entries"][0]["track_id"] = serde_json::json!(999_999);
                }
                RatingMutation::FutureVersion => {
                    value["schema_version"] = serde_json::json!(CANONICAL_SCHEMA_VERSION + 1);
                }
                RatingMutation::WrongKind => {
                    value["kind"] = serde_json::json!("flux-not-ratings");
                }
            }
            let bytes = serde_json::to_vec_pretty(&value).expect("fixture bytes");
            fs::write(&ratings_path, &bytes).expect("ratings fixture");
            let error = CanonicalLibrary::open(&root)
                .err()
                .unwrap_or_else(|| panic!("{mutation:?}: invalid ratings must not open"));
            assert!(!error.to_string().is_empty(), "{mutation:?}: empty error");
            assert_eq!(
                fs::read(&ratings_path).expect("preserved"),
                bytes,
                "{mutation:?}: failed open rewrote ratings.flux"
            );
        }

        fs::write(&ratings_path, b"{not json").expect("malformed ratings");
        let before = fs::read(&ratings_path).expect("ratings");
        assert!(CanonicalLibrary::open(&root).is_err());
        assert_eq!(fs::read(&ratings_path).expect("preserved"), before);

        fs::write(
            &ratings_path,
            serde_json::to_vec_pretty(&base).expect("base bytes"),
        )
        .expect("restore ratings");
        let reopened = CanonicalLibrary::open(&root).expect("valid ratings reopen");
        assert_eq!(
            reopened.rating(track),
            Some(TrackRating {
                rating: Some(4),
                favorite: true
            })
        );
    }

    #[test]
    fn unrelated_metadata_transaction_does_not_overwrite_invalid_ratings() {
        let (_dir, root, mut library, artist, _album, track) =
            metadata_fixture("ratings-unrelated-transaction");
        library.set_rating(track, Some(3), false).expect("rating");
        let ratings_path = root.join(RATINGS_FILE_NAME);
        let valid_ratings = fs::read(&ratings_path).expect("ratings");
        let mut corrupt: serde_json::Value =
            serde_json::from_slice(&valid_ratings).expect("ratings json");
        corrupt.as_object_mut().expect("object").remove("entries");
        let corrupt_bytes = serde_json::to_vec_pretty(&corrupt).expect("corrupt bytes");
        fs::write(&ratings_path, &corrupt_bytes).expect("corrupt ratings");

        assert!(
            library
                .rename_artist(artist, "Renamed With Corrupt Ratings")
                .is_err(),
            "reopening must fail while ratings are invalid"
        );
        assert_eq!(
            fs::read(&ratings_path).expect("ratings preserved"),
            corrupt_bytes,
            "the unrelated transaction rewrote or discarded invalid ratings"
        );

        fs::write(&ratings_path, valid_ratings).expect("restore ratings");
        let reopened = CanonicalLibrary::open(&root).expect("recover valid ratings");
        assert_eq!(
            reopened.artist(artist).map(|value| value.name.as_str()),
            Some("Renamed With Corrupt Ratings")
        );
        assert_eq!(
            reopened.rating(track),
            Some(TrackRating {
                rating: Some(3),
                favorite: false
            })
        );
    }

    #[test]
    fn valid_empty_ratings_remain_valid() {
        let (_dir, root, _library, _artist, _album, _track) =
            metadata_fixture("ratings-empty-valid");
        let ratings: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(RATINGS_FILE_NAME)).expect("ratings"))
                .expect("ratings json");
        assert_eq!(ratings["entries"].as_array().expect("entries").len(), 0);
        let reopened = CanonicalLibrary::open(&root).expect("empty ratings open");
        assert!(reopened.ratings.is_empty());
    }

    #[test]
    fn metadata_operations_advance_generation_and_stale_imports_fail() {
        let (dir, _root, mut library, artist, album, track) =
            metadata_fixture("metadata-generation");
        let mut generation = library.generation();

        library
            .rename_artist(artist, "Renamed Artist")
            .expect("artist rename");
        generation += 1;
        assert_eq!(library.generation(), generation);
        library
            .update_album_metadata(album, "Renamed Album", ReleaseType::Album)
            .expect("album metadata");
        generation += 1;
        assert_eq!(library.generation(), generation);
        library
            .update_track_metadata(track, "Renamed Track", vec![artist], None, None, None)
            .expect("track metadata");
        generation += 1;
        assert_eq!(library.generation(), generation);
        let cover = dir.write("generation-cover.png", b"cover");
        library.set_cover(album, &cover, "png").expect("cover");
        generation += 1;
        assert_eq!(library.generation(), generation);
        library.set_rating(track, Some(5), true).expect("rating");
        generation += 1;
        assert_eq!(library.generation(), generation);
        library
            .set_album_track_order(album, vec![track])
            .expect("track order");
        generation += 1;
        assert_eq!(library.generation(), generation);

        let read_only_generation = library.generation();
        let _ = library.artist(artist);
        let _ = library.album(album);
        let _ = library.track(track);
        let _ = library.rating(track);
        assert_eq!(
            library.generation(),
            read_only_generation,
            "reads must not write"
        );

        let stale_generation = library.generation();
        library
            .rename_artist(artist, "Renamed Again")
            .expect("second rename");
        let source = dir.write("stale-import.wav", b"stale import source");
        let request = CanonicalImportTrackRequest {
            source: source.clone(),
            title: "Stale Import".to_owned(),
            credited_artists: vec![CanonicalImportArtistRef::Existing(artist)],
            owner_index: 0,
            album: CanonicalImportAlbumRef::Existing(album),
            track_number: None,
            disc_number: None,
            duration_ms: None,
            artwork: None,
            album_artwork: None,
            lyrics: None,
            allow_duplicate_content: false,
            expected_source_fingerprint: Some(
                ContentFingerprint::of_file(&source).expect("fingerprint"),
            ),
        };
        let error = library
            .import_tracks_batch(stale_generation, vec![request])
            .expect_err("metadata mutation must stale the import plan");
        assert!(error.to_string().contains("stale"), "{error}");
        assert_eq!(
            library.generation(),
            stale_generation + 1,
            "failed stale import must not mutate the catalogue"
        );
        let first_transaction = library.catalog.transaction_id.clone();
        library
            .rename_artist(artist, "Renamed A Third Time")
            .expect("third rename");
        assert_ne!(
            library.catalog.transaction_id, first_transaction,
            "consecutive transactions must not reuse the catalogue transaction ID"
        );
    }

    #[test]
    fn legacy_metadata_committed_journal_recovers_cleanup() {
        let (_dir, root, library, _artist, _album, _track) =
            metadata_fixture("legacy-metadata-journal");
        let generation = library.generation();
        drop(library);
        let obsolete = root.join("legacy-obsolete.txt");
        fs::write(&obsolete, b"obsolete").expect("obsolete");
        let txn = "txn-legacy-metadata-commit";
        let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join(txn);
        fs::create_dir_all(&txn_dir).expect("txn");
        fs::write(
            txn_dir.join("journal.flux"),
            format!(
                r#"{{
                    "schema_version": 1,
                    "kind": "flux-transaction",
                    "transaction_id": "{txn}",
                    "operation": "metadata-write",
                    "state": "committed",
                    "intended_generation": {generation},
                    "actions": [],
                    "cleanup": ["legacy-obsolete.txt"]
                }}"#
            ),
        )
        .expect("journal");
        let reopened = CanonicalLibrary::open(&root).expect("legacy metadata commit recovery");
        assert!(
            !obsolete.exists(),
            "legacy journal cleanup was not completed"
        );
        assert!(!txn_dir.exists(), "legacy journal was not finalized");
        assert_eq!(reopened.generation(), generation);
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
        let album_id = library
            .track(track_id)
            .expect("track")
            .album_id
            .expect("album");
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
        let album_id = library
            .track(track_id)
            .expect("track")
            .album_id
            .expect("album");
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
                description: artist.description.clone(),
                image: artist.image.clone(),
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
            if let Some(album_id) = track.album_id {
                let Some(album) = albums.get(&album_id) else {
                    return Err(CanonicalError::Invalid(format!(
                        "seed track {} references missing album {album_id}",
                        track.id
                    )));
                };
                if album.owner_artist_id != track.owner_artist_id {
                    return Err(CanonicalError::Invalid(format!(
                        "seed track {} owner is not the album owner",
                        track.id
                    )));
                }
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
                album_id: track.album_id.map(|album| album.get()),
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
                album_id: track.album_id.map(|album| album.get()),
                track_number: track.track_number,
                disc_number: track.disc_number,
                duration_ms: track.duration_ms,
                lyrics,
                artwork: track.artwork.clone(),
                fingerprint: track.fingerprint.or(Some(fingerprint)),
                created_unix_ms: track.created_unix_ms,
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
        validate_next_counter(
            "seed next_track_id",
            catalog.next_track_id,
            catalog.tracks.iter().map(|track| track.id).max(),
            "track",
        )?;
        validate_next_counter(
            "seed next_artist_id",
            catalog.next_artist_id,
            catalog.artists.iter().map(|artist| artist.id).max(),
            "artist",
        )?;
        validate_next_counter(
            "seed next_album_id",
            catalog.next_album_id,
            catalog.albums.iter().map(|album| album.id).max(),
            "album",
        )?;
        validate_next_counter(
            "seed next_playlist_id",
            catalog.next_playlist_id,
            catalog.playlists.iter().map(|playlist| playlist.id).max(),
            "playlist",
        )?;
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
    generation: u64,
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
        let source_album = track.album_id.ok_or_else(|| {
            CanonicalError::Invalid(
                "ownership transfer currently requires the Track to belong to an Album; add it to an Album first"
                    .to_owned(),
            )
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
            if source_album != destination_album {
                return Err(CanonicalError::Invalid(
                    "the artist already owns this track; moving it to another album is a separate operation"
                        .to_owned(),
                ));
            }
            return Ok(OwnershipTransferPlan {
                track_id,
                from_owner: track.owner_artist_id,
                to_owner: new_owner,
                source_album,
                destination_album,
                new_album: None,
                old_audio: self.track_paths[&track_id].clone(),
                old_record: self.track_records[&track_id].clone(),
                old_lyrics: None,
                new_audio: self.track_paths[&track_id].clone(),
                new_record: self.track_records[&track_id].clone(),
                new_lyrics: None,
                expected_fingerprint: track.fingerprint,
                generation: self.catalog.generation,
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
        let new_audio = track_audio_rel(new_owner, Some(destination_album), track_id, &extension);
        let new_record = track_record_rel(new_owner, Some(destination_album), track_id);
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
            source_album,
            destination_album,
            new_album,
            old_audio,
            old_record,
            old_lyrics,
            new_audio,
            new_record,
            new_lyrics,
            expected_fingerprint,
            generation: self.catalog.generation,
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
        if plan.generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "the track changed after the transfer plan was built; re-plan".to_owned(),
            ));
        }
        let track = self.tracks.get(&plan.track_id).cloned().ok_or_else(|| {
            CanonicalError::Invalid("track disappeared before transfer".to_owned())
        })?;
        if track.owner_artist_id != plan.from_owner || track.album_id != Some(plan.source_album) {
            return Err(CanonicalError::Invalid(
                "the track changed after the transfer plan was built; re-plan".to_owned(),
            ));
        }
        if !track.credited_artist_ids.contains(&plan.to_owner) {
            return Err(CanonicalError::Invalid(
                "the destination artist is no longer credited; re-plan".to_owned(),
            ));
        }
        let current_old_lyrics =
            plan_track_lyrics_rel(&self.track_records[&plan.track_id], track.lyrics.as_deref());
        if current_old_lyrics != plan.old_lyrics {
            return Err(CanonicalError::Invalid(
                "the track lyrics changed after the transfer plan was built; re-plan".to_owned(),
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
            album_id: Some(plan.destination_album.get()),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            lyrics: track.lyrics,
            artwork: track.artwork.clone(),
            fingerprint: plan.expected_fingerprint,
            created_unix_ms: track.created_unix_ms,
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
        track_descriptor.album_id = Some(plan.destination_album.get());
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
        assert_eq!(track.album_id, Some(destination));
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
                .album(library.track(id).expect("track").album_id.expect("album"))
                .map(|album| album.title.as_str()),
            Some("Back Home")
        );
        assert_eq!(library.tracks().count(), 1);
    }

    #[test]
    fn ownership_transfer_rejects_lyrics_or_other_changes_after_review() {
        let dir = TempDir::new("transfer-stale-lyrics");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source = dir.write("source/song.wav", b"opaque stale transfer bytes");
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
        library
            .set_lyrics(id, Some("[00:00.00]before"))
            .expect("lyrics");
        let destination = library
            .create_album(b, "Destination", ReleaseType::Album)
            .expect("album");
        let plan = library
            .plan_ownership_transfer(id, b, OwnershipDestination::Existing(destination))
            .expect("plan");
        library
            .set_lyrics(id, Some("[00:00.00]after review"))
            .expect("new lyrics");
        let error = library
            .execute_ownership_transfer(&plan)
            .expect_err("stale plan must be rejected");
        assert!(error.to_string().contains("changed after"), "{error}");
        assert_eq!(
            library.track(id).map(|track| track.owner_artist_id),
            Some(a)
        );
        assert_eq!(
            fs::read_to_string(library.track_lyrics_path(id).expect("lyrics path"))
                .expect("lyrics text"),
            "[00:00.00]after review"
        );
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
        let album = library.track(id).expect("track").album_id.expect("album");
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
        assert_eq!(library.track(id).expect("track").album_id, Some(album));
        assert_eq!(
            library.track(id).expect("track").credited_artist_ids,
            vec![a]
        );
        assert!(library.track_audio_path(id).expect("audio").is_file());
    }
}

// --------------------------------------------------- artist deletion ---

/// Destination for an owned track transferred during artist deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistDeletionDestination {
    /// Move into an existing album owned by the destination artist.
    Existing(AlbumId),
    /// Create an explicit new release under the destination artist.
    New {
        /// Release title.
        title: String,
        /// Release type.
        release_type: ReleaseType,
    },
}

/// One explicit per-track decision in an artist-deletion plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtistDeletionDecision {
    /// No decision yet; blocks execution.
    Unresolved,
    /// Move physical ownership and remove the deleted artist from credits.
    Transfer {
        /// Surviving credited artist who becomes owner.
        new_owner: ArtistId,
        /// Explicit destination release.
        destination: ArtistDeletionDestination,
    },
    /// For a track owned by someone else, remove only the deleted artist's
    /// credit. `replacement` is required only if the resulting list would be
    /// empty.
    RemoveCredit {
        /// Explicit replacement credit when needed.
        replacement: Option<ArtistId>,
    },
    /// Delete this owned track and its managed resources after confirmation.
    Delete {
        /// Must be true; there is no default destructive decision.
        confirmed: bool,
    },
}

/// Album-level inventory for the deletion preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistDeletionAlbum {
    /// Stable album ID.
    pub id: AlbumId,
    /// Display title.
    pub title: String,
    /// Current owner.
    pub owner_artist_id: ArtistId,
    /// Number of tracks currently in the album.
    pub track_count: usize,
    /// Managed cover path, if any.
    pub cover: Option<RelPath>,
}

/// One affected track in an artist-deletion plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistDeletionTrack {
    /// Track identity.
    pub track_id: TrackId,
    /// Current album, or `None` for standalone.
    pub album_id: Option<AlbumId>,
    /// Current album title.
    pub album_title: String,
    /// Track title.
    pub title: String,
    /// Current physical owner.
    pub owner_artist_id: ArtistId,
    /// Ordered credits at plan time.
    pub credited_artist_ids: Vec<ArtistId>,
    /// Canonical audio path at plan time.
    pub audio_path: RelPath,
    /// Canonical managed lyrics path, if any.
    pub lyrics_path: Option<RelPath>,
    /// Lyrics content fingerprint at plan time.
    pub lyrics_fingerprint: Option<ContentFingerprint>,
    /// Playlists that reference this track.
    pub playlist_ids: Vec<PlaylistId>,
    /// Current rating/favorite, if any.
    pub rating: Option<TrackRating>,
    /// Surviving credited artists eligible to become physical owner.
    pub eligible_owners: Vec<ArtistId>,
    /// Explicit decision; defaults to unresolved.
    pub decision: ArtistDeletionDecision,
    /// Decision/resource issues for this track.
    pub issues: Vec<String>,
}

/// Complete artist-deletion preview and decision set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistDeletionPlan {
    /// Artist being deleted.
    pub artist_id: ArtistId,
    /// Display name at plan time.
    pub artist_name: String,
    /// Catalogue generation captured by the plan.
    pub generation: u64,
    /// Owned album IDs.
    pub owned_albums: Vec<AlbumId>,
    /// Owned album details, including empty albums.
    pub albums: Vec<ArtistDeletionAlbum>,
    /// All albums available as transfer destinations.
    pub destination_albums: Vec<ArtistDeletionAlbum>,
    /// Owned track decisions.
    pub owned_tracks: Vec<ArtistDeletionTrack>,
    /// Credited-only track decisions.
    pub credited_tracks: Vec<ArtistDeletionTrack>,
    /// IDs of collaborative owned tracks.
    pub collaborative_tracks: Vec<TrackId>,
    /// IDs of owned tracks with no surviving credited owner.
    pub non_collaborative_tracks: Vec<TrackId>,
    /// IDs of credited-only tracks.
    pub credited_only_tracks: Vec<TrackId>,
    /// Playlists referencing any affected track.
    pub affected_playlists: Vec<PlaylistId>,
    /// Blocking reasons.
    pub issues: Vec<String>,
    /// Resource issues independent of decisions.
    snapshot_issues: Vec<String>,
}

impl ArtistDeletionPlan {
    /// Whether every affected track is resolved and no blocking issue remains.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.issues.is_empty()
    }

    /// First blocking issue.
    #[must_use]
    pub fn blocking_reason(&self) -> Option<&str> {
        self.issues.first().map(String::as_str)
    }

    fn owned_track_mut(&mut self, id: TrackId) -> Option<&mut ArtistDeletionTrack> {
        self.owned_tracks
            .iter_mut()
            .find(|track| track.track_id == id)
    }

    fn credited_track_mut(&mut self, id: TrackId) -> Option<&mut ArtistDeletionTrack> {
        self.credited_tracks
            .iter_mut()
            .find(|track| track.track_id == id)
    }

    /// Sets an explicit transfer decision for an owned track.
    pub fn set_transfer(
        &mut self,
        id: TrackId,
        new_owner: ArtistId,
        destination: ArtistDeletionDestination,
    ) -> Result<(), String> {
        let artist_id = self.artist_id;
        let destination_albums = self.destination_albums.clone();
        let track = self
            .owned_track_mut(id)
            .ok_or_else(|| format!("track {id} is not owned by this artist"))?;
        if new_owner == artist_id || !track.eligible_owners.contains(&new_owner) {
            return Err("destination artist must be another credited artist".to_owned());
        }
        if let ArtistDeletionDestination::Existing(album_id) = &destination {
            let album = destination_albums
                .iter()
                .find(|album| album.id == *album_id)
                .ok_or_else(|| format!("destination album {album_id} does not exist"))?;
            if album.owner_artist_id != new_owner {
                return Err("destination album belongs to a different artist".to_owned());
            }
        }
        if let ArtistDeletionDestination::New { title, .. } = &destination
            && title.trim().is_empty()
        {
            return Err("new destination album needs a title".to_owned());
        }
        track.decision = ArtistDeletionDecision::Transfer {
            new_owner,
            destination,
        };
        self.refresh_issues();
        Ok(())
    }

    /// Sets a credited-only decision to remove the deleted artist's credit.
    pub fn set_remove_credit(
        &mut self,
        id: TrackId,
        replacement: Option<ArtistId>,
    ) -> Result<(), String> {
        let track = self
            .credited_track_mut(id)
            .ok_or_else(|| format!("track {id} is not a credited-only track"))?;
        track.decision = ArtistDeletionDecision::RemoveCredit { replacement };
        self.refresh_issues();
        Ok(())
    }

    /// Sets an explicit confirmed deletion for an owned track.
    pub fn set_delete(&mut self, id: TrackId, confirmed: bool) -> Result<(), String> {
        let track = self
            .owned_track_mut(id)
            .ok_or_else(|| format!("track {id} is not owned by this artist"))?;
        track.decision = ArtistDeletionDecision::Delete { confirmed };
        self.refresh_issues();
        Ok(())
    }

    /// Resets an owned or credited-only track to unresolved.
    pub fn set_unresolved(&mut self, id: TrackId) {
        if let Some(track) = self.owned_track_mut(id) {
            track.decision = ArtistDeletionDecision::Unresolved;
        } else if let Some(track) = self.credited_track_mut(id) {
            track.decision = ArtistDeletionDecision::Unresolved;
        }
        self.refresh_issues();
    }

    /// Recomputed blocking issues after decisions change.
    pub fn refresh_issues(&mut self) {
        let artist_id = self.artist_id;
        let destination_albums = self.destination_albums.clone();
        let snapshot_issues = self.snapshot_issues.clone();
        self.issues.clear();
        self.issues.extend(snapshot_issues);
        for track in &mut self.owned_tracks {
            track.issues.clear();
            match &track.decision {
                ArtistDeletionDecision::Unresolved => {
                    track
                        .issues
                        .push("choose transfer or explicit deletion".to_owned());
                }
                ArtistDeletionDecision::Transfer {
                    new_owner,
                    destination,
                } => {
                    if *new_owner == artist_id || !track.eligible_owners.contains(new_owner) {
                        track.issues.push("invalid destination artist".to_owned());
                    }
                    match destination {
                        ArtistDeletionDestination::Existing(album_id) => {
                            if !destination_albums.iter().any(|album| {
                                album.id == *album_id && album.owner_artist_id == *new_owner
                            }) {
                                track.issues.push("invalid destination album".to_owned());
                            }
                        }
                        ArtistDeletionDestination::New { title, .. } if title.trim().is_empty() => {
                            track
                                .issues
                                .push("destination album title is empty".to_owned());
                        }
                        ArtistDeletionDestination::New { .. } => {}
                    }
                }
                ArtistDeletionDecision::RemoveCredit { .. } => {
                    track
                        .issues
                        .push("owned tracks cannot use remove-credit".to_owned());
                }
                ArtistDeletionDecision::Delete { confirmed } => {
                    if !*confirmed {
                        track
                            .issues
                            .push("explicit deletion confirmation required".to_owned());
                    }
                }
            }
            if !track.issues.is_empty() {
                self.issues.push(format!(
                    "track {}: {}",
                    track.track_id,
                    track.issues.join("; ")
                ));
            }
        }
        for track in &mut self.credited_tracks {
            track.issues.clear();
            match &track.decision {
                ArtistDeletionDecision::Unresolved => {
                    track.issues.push("choose remove-credit".to_owned());
                }
                ArtistDeletionDecision::RemoveCredit { replacement } => {
                    let remaining = track
                        .credited_artist_ids
                        .iter()
                        .filter(|artist| **artist != self.artist_id)
                        .copied()
                        .collect::<Vec<_>>();
                    if remaining.is_empty() {
                        match replacement {
                            Some(replacement) if *replacement != self.artist_id => {}
                            _ => track
                                .issues
                                .push("a replacement credit is required".to_owned()),
                        }
                    }
                    if !remaining.is_empty() && !remaining.contains(&track.owner_artist_id) {
                        track
                            .issues
                            .push("resulting credits omit the physical owner".to_owned());
                    }
                }
                ArtistDeletionDecision::Transfer { .. } | ArtistDeletionDecision::Delete { .. } => {
                    track
                        .issues
                        .push("credited-only tracks cannot be moved or deleted".to_owned());
                }
            }
            if !track.issues.is_empty() {
                self.issues.push(format!(
                    "track {}: {}",
                    track.track_id,
                    track.issues.join("; ")
                ));
            }
        }
    }

    /// Track IDs explicitly selected for deletion.
    #[must_use]
    pub fn deleted_track_ids(&self) -> Vec<TrackId> {
        self.owned_tracks
            .iter()
            .filter(|track| {
                matches!(
                    track.decision,
                    ArtistDeletionDecision::Delete { confirmed: true }
                )
            })
            .map(|track| track.track_id)
            .collect()
    }
}

/// Result of a committed artist deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistDeletionReport {
    /// Deleted artist ID.
    pub artist_id: ArtistId,
    /// Tracks transferred to another owner.
    pub transferred: usize,
    /// Tracks retained under their existing owner with a credit removed.
    pub credits_removed: usize,
    /// Tracks explicitly deleted.
    pub deleted_tracks: Vec<TrackId>,
    /// Albums removed with the artist.
    pub deleted_albums: Vec<AlbumId>,
}

impl CanonicalLibrary {
    /// Builds a read-only, generation-tagged deletion preview. Every affected
    /// track starts unresolved.
    pub fn plan_artist_deletion(
        &self,
        artist_id: ArtistId,
    ) -> Result<ArtistDeletionPlan, CanonicalError> {
        let artist = self.artists.get(&artist_id).ok_or_else(|| {
            CanonicalError::Invalid(format!("artist {artist_id} is not in the library"))
        })?;
        let owned_albums = self
            .albums
            .values()
            .filter(|album| album.owner_artist_id == artist_id)
            .map(|album| album.id)
            .collect::<Vec<_>>();
        let albums = self
            .albums
            .values()
            .filter(|album| album.owner_artist_id == artist_id)
            .map(|album| ArtistDeletionAlbum {
                id: album.id,
                title: album.title.clone(),
                owner_artist_id: album.owner_artist_id,
                track_count: album.track_order.len(),
                cover: album.cover.as_ref().map(|cover| {
                    RelPath::from_utf8(&format!(
                        "{}/{}",
                        album_rel(album.owner_artist_id, album.id).display_lossy(),
                        cover
                    ))
                    .expect("cover path is valid")
                }),
            })
            .collect::<Vec<_>>();
        let destination_albums = self
            .albums
            .values()
            .map(|album| ArtistDeletionAlbum {
                id: album.id,
                title: album.title.clone(),
                owner_artist_id: album.owner_artist_id,
                track_count: album.track_order.len(),
                cover: None,
            })
            .collect::<Vec<_>>();
        let mut owned_tracks = Vec::new();
        let mut credited_tracks = Vec::new();
        let mut collaborative = Vec::new();
        let mut non_collaborative = Vec::new();
        let mut credited_only = Vec::new();
        let mut affected = HashSet::new();
        for track in self.tracks.values() {
            let is_owner = track.owner_artist_id == artist_id;
            let is_credited = track.credited_artist_ids.contains(&artist_id);
            if !is_owner && !is_credited {
                continue;
            }
            let album_title = track
                .album_id
                .and_then(|album_id| self.albums.get(&album_id))
                .map(|album| album.title.clone())
                .unwrap_or_default();
            let audio_path = self.track_paths[&track.id].clone();
            let lyrics_path = self.track_lyrics_path(track.id).and_then(|path| {
                path.strip_prefix(&self.root).ok().and_then(|relative| {
                    relative
                        .to_str()
                        .and_then(|text| RelPath::from_utf8(text).ok())
                })
            });
            let playlist_ids = self
                .playlists
                .values()
                .filter(|playlist| playlist.track_ids.contains(&track.id))
                .map(|playlist| playlist.id)
                .collect::<Vec<_>>();
            let lyrics_fingerprint = lyrics_path.as_ref().and_then(|path| {
                safe_join(&self.root, path)
                    .ok()
                    .and_then(|path| ContentFingerprint::of_file(&path).ok())
            });
            let mut issues = Vec::new();
            if !self.track_available(track.id) {
                issues.push("audio resource is missing".to_owned());
            }
            if track.lyrics.is_some()
                && lyrics_path
                    .as_ref()
                    .is_none_or(|path| !safe_join(&self.root, path).is_ok_and(|p| p.is_file()))
            {
                issues.push("lyrics resource is missing".to_owned());
            }
            let eligible_owners = track
                .credited_artist_ids
                .iter()
                .copied()
                .filter(|artist| *artist != artist_id && self.artists.contains_key(artist))
                .collect::<Vec<_>>();
            let planned = ArtistDeletionTrack {
                track_id: track.id,
                album_id: track.album_id,
                album_title,
                title: track.title.clone(),
                owner_artist_id: track.owner_artist_id,
                credited_artist_ids: track.credited_artist_ids.clone(),
                audio_path,
                lyrics_path,
                lyrics_fingerprint,
                playlist_ids,
                rating: self.rating(track.id),
                eligible_owners: eligible_owners.clone(),
                decision: ArtistDeletionDecision::Unresolved,
                issues,
            };
            affected.insert(track.id);
            if is_owner {
                if eligible_owners.is_empty() {
                    non_collaborative.push(track.id);
                } else {
                    collaborative.push(track.id);
                }
                owned_tracks.push(planned);
            } else {
                credited_only.push(track.id);
                credited_tracks.push(planned);
            }
        }
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
        let snapshot_issues = owned_tracks
            .iter()
            .chain(credited_tracks.iter())
            .flat_map(|track| {
                track
                    .issues
                    .iter()
                    .map(|issue| format!("track {}: {issue}", track.track_id))
            })
            .collect::<Vec<_>>();
        let mut plan = ArtistDeletionPlan {
            artist_id,
            artist_name: artist.name.clone(),
            generation: self.catalog.generation,
            owned_albums,
            albums,
            destination_albums,
            owned_tracks,
            credited_tracks,
            collaborative_tracks: collaborative,
            non_collaborative_tracks: non_collaborative,
            credited_only_tracks: credited_only,
            affected_playlists,
            issues: Vec::new(),
            snapshot_issues,
        };
        plan.refresh_issues();
        Ok(plan)
    }

    /// Executes an artist-deletion plan as one catalogue transaction. This
    /// stages all transfers, credit edits, deletions, playlist/rating updates
    /// and artist/album removal, then publishes the catalogue last.
    pub fn execute_artist_deletion(
        &mut self,
        plan: &ArtistDeletionPlan,
    ) -> Result<ArtistDeletionReport, CanonicalError> {
        self.ensure_writable()?;
        self.ensure_no_pending_transactions()?;
        let album_for = self
            .artists
            .get(&plan.artist_id)
            .map(|artist| artist.name.clone())
            .ok_or_else(|| CanonicalError::Invalid("deleted artist is gone".to_owned()))?;
        if plan.generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "deletion plan is stale: catalogue generation changed; re-preview".to_owned(),
            ));
        }
        if plan.artist_name != album_for {
            return Err(CanonicalError::Invalid(
                "deletion plan is stale: artist name changed; re-preview".to_owned(),
            ));
        }
        if !plan.is_ready() {
            return Err(CanonicalError::Invalid(format!(
                "deletion plan has unresolved decisions: {}",
                plan.blocking_reason().unwrap_or("unknown issue")
            )));
        }
        for planned in &plan.albums {
            let current = self.albums.get(&planned.id).ok_or_else(|| {
                CanonicalError::Invalid(format!("album {} disappeared; re-preview", planned.id))
            })?;
            if current.owner_artist_id != plan.artist_id
                || current.title != planned.title
                || current.track_order.len() != planned.track_count
                || album_cover_rel(current)? != planned.cover
            {
                return Err(CanonicalError::Invalid(format!(
                    "album {} changed since preview; re-preview",
                    planned.id
                )));
            }
        }
        // Snapshot validation against the current catalogue.
        for planned in plan.owned_tracks.iter().chain(plan.credited_tracks.iter()) {
            let current = self.tracks.get(&planned.track_id).ok_or_else(|| {
                CanonicalError::Invalid(format!(
                    "track {} disappeared; re-preview",
                    planned.track_id
                ))
            })?;
            if current.owner_artist_id != planned.owner_artist_id
                || current.album_id != planned.album_id
                || current.credited_artist_ids != planned.credited_artist_ids
                || current.title != planned.title
                || self.track_paths.get(&current.id) != Some(&planned.audio_path)
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {} changed since preview; re-preview",
                    planned.track_id
                )));
            }
            if self.rating(current.id) != planned.rating {
                return Err(CanonicalError::Invalid(format!(
                    "track {} rating changed since preview; re-preview",
                    planned.track_id
                )));
            }
            let current_album_title = current
                .album_id
                .and_then(|album_id| self.albums.get(&album_id))
                .map(|album| album.title.as_str());
            if current_album_title != Some(planned.album_title.as_str())
                && !(current.album_id.is_none() && planned.album_title.is_empty())
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {} album changed since preview; re-preview",
                    planned.track_id
                )));
            }
            let current_lyrics = current.lyrics.as_deref().and_then(|lyrics| {
                plan_track_lyrics_rel(&self.track_records[&current.id], Some(lyrics))
            });
            if current_lyrics != planned.lyrics_path {
                return Err(CanonicalError::Invalid(format!(
                    "track {} lyrics changed since preview; re-preview",
                    planned.track_id
                )));
            }
            let current_lyrics_fingerprint = current_lyrics.as_ref().and_then(|path| {
                safe_join(&self.root, path)
                    .ok()
                    .and_then(|path| ContentFingerprint::of_file(&path).ok())
            });
            if current_lyrics_fingerprint != planned.lyrics_fingerprint {
                return Err(CanonicalError::Invalid(format!(
                    "track {} lyrics content changed since preview; re-preview",
                    planned.track_id
                )));
            }
        }

        let mut catalog = self.catalog.clone();
        let mut albums = self.albums.clone();
        let mut tracks = self.tracks.clone();
        let mut ratings = self.ratings.clone();
        let mut playlists = self.playlists.clone();
        let mut writes = Vec::new();
        let mut cleanup = Vec::new();
        let mut deleted_tracks = BTreeSet::new();
        let mut new_albums = BTreeMap::new();
        let mut new_album_keys: BTreeMap<(ArtistId, String), AlbumId> = BTreeMap::new();
        let mut next_album = catalog.next_album_id;
        let mut transferred = 0u64;
        let mut credits_removed = 0u64;

        for planned in &plan.owned_tracks {
            let current = tracks.get(&planned.track_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid("track disappeared during deletion".to_owned())
            })?;
            match &planned.decision {
                ArtistDeletionDecision::Transfer {
                    new_owner,
                    destination,
                } => {
                    if !current.credited_artist_ids.contains(new_owner)
                        || *new_owner == plan.artist_id
                        || !self.artists.contains_key(new_owner)
                    {
                        return Err(CanonicalError::Invalid(
                            "invalid destination owner in deletion plan".to_owned(),
                        ));
                    }
                    let destination_album = match destination {
                        ArtistDeletionDestination::Existing(id) => {
                            let album = albums.get(id).ok_or_else(|| {
                                CanonicalError::Invalid(format!("destination album {id} is gone"))
                            })?;
                            if album.owner_artist_id != *new_owner {
                                return Err(CanonicalError::Invalid(
                                    "destination album owner is invalid".to_owned(),
                                ));
                            }
                            *id
                        }
                        ArtistDeletionDestination::New {
                            title,
                            release_type,
                        } => {
                            let title = title.trim().to_owned();
                            if albums.values().any(|album| {
                                album.owner_artist_id == *new_owner && album.title == title
                            }) {
                                return Err(CanonicalError::Invalid(
                                    "a destination album with that title already exists; select it explicitly"
                                        .to_owned(),
                                ));
                            }
                            let key = (*new_owner, title.clone());
                            let id = *new_album_keys.entry(key).or_insert_with(|| {
                                let id = AlbumId::new(next_album);
                                next_album += 1;
                                id
                            });
                            new_albums.entry(id).or_insert_with(|| CanonicalAlbum {
                                id,
                                owner_artist_id: *new_owner,
                                title,
                                release_type: *release_type,
                                cover: None,
                                track_order: Vec::new(),
                            });
                            id
                        }
                    };
                    let destination_album = albums
                        .entry(destination_album)
                        .or_insert_with(|| new_albums[&destination_album].clone());
                    if destination_album.track_order.contains(&current.id) {
                        return Err(CanonicalError::Invalid(
                            "destination album already contains the track".to_owned(),
                        ));
                    }
                    let mut credits = current.credited_artist_ids.clone();
                    credits.retain(|artist| *artist != plan.artist_id);
                    if credits.is_empty() || !credits.contains(new_owner) {
                        return Err(CanonicalError::Invalid(
                            "transfer would leave invalid credits".to_owned(),
                        ));
                    }
                    let old_audio = self.track_paths[&current.id].clone();
                    let old_record = self.track_records[&current.id].clone();
                    let old_lyrics = self.track_lyrics_path(current.id);
                    let extension = old_audio
                        .display_lossy()
                        .rsplit_once('.')
                        .map(|(_, extension)| extension.to_owned())
                        .ok_or_else(|| {
                            CanonicalError::Invalid("audio has no extension".to_owned())
                        })?;
                    let new_audio = track_audio_rel(
                        *new_owner,
                        Some(destination_album.id),
                        current.id,
                        &extension,
                    );
                    let new_record =
                        track_record_rel(*new_owner, Some(destination_album.id), current.id);
                    if safe_join(&self.root, &new_audio)?.exists()
                        || safe_join(&self.root, &new_record)?.exists()
                    {
                        return Err(CanonicalError::Invalid(
                            "destination path collision during deletion".to_owned(),
                        ));
                    }
                    destination_album.track_order.push(current.id);
                    let lyrics = current.lyrics.clone();
                    let new_lyrics = lyrics.as_ref().map(|name| {
                        RelPath::from_utf8(&format!(
                            "{}/{}",
                            new_record
                                .display_lossy()
                                .rsplit_once('/')
                                .map_or("", |(dir, _)| dir),
                            name
                        ))
                        .expect("lyrics path is valid")
                    });
                    let track_dto = TrackEntityDto {
                        schema_version: CANONICAL_SCHEMA_VERSION,
                        kind: TRACK_KIND.to_owned(),
                        id: current.id.get(),
                        title: current.title.clone(),
                        credited_artist_ids: credits.iter().map(|artist| artist.get()).collect(),
                        owner_artist_id: new_owner.get(),
                        album_id: Some(destination_album.id.get()),
                        track_number: current.track_number,
                        disc_number: current.disc_number,
                        duration_ms: current.duration_ms,
                        lyrics,
                        artwork: current.artwork.clone(),
                        fingerprint: current.fingerprint,
                        created_unix_ms: current.created_unix_ms,
                    };
                    let mut updated = current.clone();
                    updated.owner_artist_id = *new_owner;
                    updated.album_id = Some(destination_album.id);
                    updated.credited_artist_ids = credits;
                    tracks.insert(updated.id, updated);
                    push_file_write(&mut writes, &new_record, &track_dto)?;
                    writes.push(PendingWrite {
                        target: new_audio.clone(),
                        bytes: None,
                        copy_from: Some(safe_join(&self.root, &old_audio)?),
                        expected: current.fingerprint.or(Some(ContentFingerprint::of_file(
                            &safe_join(&self.root, &old_audio)?,
                        )?)),
                    });
                    if let (Some(old), Some(new)) = (old_lyrics, new_lyrics) {
                        writes.push(PendingWrite {
                            target: new,
                            bytes: Some(fs::read(old)?),
                            copy_from: None,
                            expected: None,
                        });
                    }
                    cleanup.extend([old_audio, old_record]);
                    cleanup.extend(plan_track_lyrics_rel(
                        &self.track_records[&current.id],
                        current.lyrics.as_deref(),
                    ));
                    let descriptor = catalog
                        .tracks
                        .iter_mut()
                        .find(|entry| entry.id == current.id.get())
                        .ok_or_else(|| {
                            CanonicalError::Invalid("catalogue track missing".to_owned())
                        })?;
                    descriptor.owner_artist_id = new_owner.get();
                    descriptor.album_id = Some(destination_album.id.get());
                    descriptor.audio_path = new_audio.display_lossy();
                    descriptor.record_path = new_record.display_lossy();
                    transferred += 1;
                }
                ArtistDeletionDecision::Delete { confirmed: true } => {
                    deleted_tracks.insert(current.id);
                    cleanup.extend([
                        self.track_paths[&current.id].clone(),
                        self.track_records[&current.id].clone(),
                    ]);
                    cleanup.extend(plan_track_lyrics_rel(
                        &self.track_records[&current.id],
                        current.lyrics.as_deref(),
                    ));
                    if let Some(artwork) = &current.artwork {
                        cleanup.push(RelPath::from_utf8(&format!(
                            "{TRACK_ARTWORK_DIR_NAME}/{artwork}"
                        ))?);
                    }
                    ratings.remove(&current.id);
                    tracks.remove(&current.id);
                    catalog.tracks.retain(|entry| entry.id != current.id.get());
                    for playlist in playlists.values_mut() {
                        playlist.track_ids.retain(|track| *track != current.id);
                    }
                }
                _ => {
                    return Err(CanonicalError::Invalid(
                        "invalid owned-track decision".to_owned(),
                    ));
                }
            }
        }

        for planned in &plan.credited_tracks {
            let current = tracks
                .get(&planned.track_id)
                .cloned()
                .ok_or_else(|| CanonicalError::Invalid("credited track disappeared".to_owned()))?;
            let ArtistDeletionDecision::RemoveCredit { replacement } = planned.decision else {
                return Err(CanonicalError::Invalid(
                    "invalid credited-only decision".to_owned(),
                ));
            };
            let mut credits = current.credited_artist_ids.clone();
            credits.retain(|artist| *artist != plan.artist_id);
            if credits.is_empty() {
                let replacement = replacement.ok_or_else(|| {
                    CanonicalError::Invalid("replacement credit is required".to_owned())
                })?;
                if replacement == plan.artist_id || !self.artists.contains_key(&replacement) {
                    return Err(CanonicalError::Invalid(
                        "replacement credit must be a surviving artist".to_owned(),
                    ));
                }
                credits.push(replacement);
            }
            if !credits.contains(&current.owner_artist_id) {
                return Err(CanonicalError::Invalid(
                    "resulting credits omit the physical owner".to_owned(),
                ));
            }
            let record_path = self.track_records[&current.id].clone();
            let mut updated = current.clone();
            updated.credited_artist_ids = credits;
            tracks.insert(updated.id, updated.clone());
            let dto = TrackEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: TRACK_KIND.to_owned(),
                id: updated.id.get(),
                title: updated.title,
                credited_artist_ids: updated
                    .credited_artist_ids
                    .iter()
                    .map(|artist| artist.get())
                    .collect(),
                owner_artist_id: updated.owner_artist_id.get(),
                album_id: updated.album_id.map(|album| album.get()),
                track_number: updated.track_number,
                disc_number: updated.disc_number,
                duration_ms: updated.duration_ms,
                lyrics: updated.lyrics,
                artwork: updated.artwork,
                fingerprint: updated.fingerprint,
                created_unix_ms: updated.created_unix_ms,
            };
            push_file_write(&mut writes, &record_path, &dto)?;
            credits_removed += 1;
        }

        // Remove every album owned by the deleted artist; all its owned tracks
        // have been transferred or explicitly deleted above.
        for album_id in &plan.owned_albums {
            if let Some(album) = albums.remove(album_id) {
                let rel = album_rel(album.owner_artist_id, album.id);
                push_cleanup_album(&mut cleanup, &rel, &album)?;
                catalog.albums.retain(|entry| entry.id != album.id.get());
            }
        }
        // Add explicitly created destination albums and rewrite existing ones.
        for album in new_albums.values() {
            catalog.albums.push(CatalogAlbumDto {
                id: album.id.get(),
                artist_id: album.owner_artist_id.get(),
                path: album_rel(album.owner_artist_id, album.id).display_lossy(),
            });
        }
        catalog.albums.sort_by_key(|album| album.id);
        let mut destination_ids = BTreeSet::new();
        for track in &plan.owned_tracks {
            if let ArtistDeletionDecision::Transfer {
                new_owner,
                destination,
            } = &track.decision
            {
                match destination {
                    ArtistDeletionDestination::Existing(id) => {
                        destination_ids.insert(*id);
                    }
                    ArtistDeletionDestination::New { title, .. } => {
                        if let Some(id) = new_album_keys.get(&(*new_owner, title.clone())) {
                            destination_ids.insert(*id);
                        }
                    }
                }
            }
        }
        destination_ids.extend(new_albums.keys().copied());
        for album_id in destination_ids
            .into_iter()
            .chain(new_albums.keys().copied())
        {
            if let Some(album) = albums.get(&album_id) {
                let dto = AlbumEntityDto {
                    schema_version: CANONICAL_SCHEMA_VERSION,
                    kind: ALBUM_KIND.to_owned(),
                    id: album.id.get(),
                    owner_artist_id: album.owner_artist_id.get(),
                    title: album.title.clone(),
                    release_type: album.release_type,
                    cover: album.cover.clone(),
                    track_order: album.track_order.iter().map(|id| id.get()).collect(),
                };
                push_entity_write(
                    &mut writes,
                    &album_rel(album.owner_artist_id, album.id),
                    "album.flux",
                    &dto,
                )?;
            }
        }
        // Playlist and ratings consequences for explicitly deleted tracks.
        for playlist in playlists.values() {
            if self.playlists.get(&playlist.id) != Some(playlist) {
                let rel = playlist_rel(playlist.id);
                let dto = PlaylistEntityDto {
                    schema_version: CANONICAL_SCHEMA_VERSION,
                    kind: PLAYLIST_KIND.to_owned(),
                    id: playlist.id.get(),
                    name: playlist.name.clone(),
                    track_ids: playlist.track_ids.iter().map(|track| track.get()).collect(),
                };
                push_file_write(&mut writes, &rel, &dto)?;
            }
        }
        let ratings_dto = RatingsDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: RATINGS_KIND.to_owned(),
            entries: ratings
                .iter()
                .map(|(track, rating)| RatingEntryDto {
                    track_id: track.get(),
                    rating: rating.rating,
                    favorite: rating.favorite,
                })
                .collect(),
        };
        let ratings_bytes =
            serde_json::to_vec_pretty(&ratings_dto).map_err(|err| CanonicalError::Malformed {
                file: RATINGS_FILE_NAME.to_owned(),
                message: err.to_string(),
            })?;
        writes.push(PendingWrite {
            target: RelPath::from_utf8(RATINGS_FILE_NAME)?,
            bytes: Some(ratings_bytes),
            copy_from: None,
            expected: None,
        });
        // Remove the artist only after all tracks/albums/credits are resolved.
        if tracks.values().any(|track| {
            track.owner_artist_id == plan.artist_id
                || track.credited_artist_ids.contains(&plan.artist_id)
        }) {
            return Err(CanonicalError::Invalid(
                "deletion would leave references to the artist".to_owned(),
            ));
        }
        let artist_rel_path = artist_rel(plan.artist_id);
        cleanup.push(RelPath::from_utf8(&format!(
            "{}/artist.flux",
            artist_rel_path.display_lossy()
        ))?);
        if let Some(image) = self
            .artists
            .get(&plan.artist_id)
            .and_then(|artist| artist.image.as_deref())
        {
            cleanup.push(RelPath::from_utf8(&format!(
                "{}/{}",
                artist_rel_path.display_lossy(),
                image
            ))?);
        }
        catalog
            .artists
            .retain(|artist| artist.id != plan.artist_id.get());
        catalog.next_album_id = next_album;
        catalog.tracks.sort_by_key(|track| track.id);
        self.commit_writes_with_cleanup(Some(catalog), writes, cleanup)?;
        self.reopen_after()?;
        Ok(ArtistDeletionReport {
            artist_id: plan.artist_id,
            transferred: transferred as usize,
            credits_removed: credits_removed as usize,
            deleted_tracks: deleted_tracks.into_iter().collect(),
            deleted_albums: plan.owned_albums.clone(),
        })
    }
}

fn album_cover_rel(album: &CanonicalAlbum) -> Result<Option<RelPath>, CanonicalError> {
    Ok(album
        .cover
        .as_ref()
        .map(|cover| {
            RelPath::from_utf8(&format!(
                "{}/{}",
                album_rel(album.owner_artist_id, album.id).display_lossy(),
                cover
            ))
        })
        .transpose()?)
}

fn plan_track_lyrics_rel(record: &RelPath, lyrics: Option<&str>) -> Option<RelPath> {
    lyrics.and_then(|name| {
        RelPath::from_utf8(&format!(
            "{}/{}",
            record
                .display_lossy()
                .rsplit_once('/')
                .map_or("", |(dir, _)| dir),
            name
        ))
        .ok()
    })
}

fn push_cleanup_album(
    cleanup: &mut Vec<RelPath>,
    album_rel: &RelPath,
    album: &CanonicalAlbum,
) -> Result<(), CanonicalError> {
    cleanup.push(RelPath::from_utf8(&format!(
        "{}/album.flux",
        album_rel.display_lossy()
    ))?);
    if let Some(cover) = &album.cover {
        cleanup.push(RelPath::from_utf8(&format!(
            "{}/{}",
            album_rel.display_lossy(),
            cover
        ))?);
    }
    Ok(())
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
                .any(|issue| issue.contains("choose transfer"))
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
                abort: false,
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
                Some(if committed {
                    fx.destination
                } else {
                    AlbumId::new(1)
                })
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
            abort: false,
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

#[cfg(test)]
mod partial_journal_tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    fn valid_journal(txn: &str) -> String {
        format!(
            r#"{{"schema_version":1,"kind":"flux-transaction","transaction_id":"{txn}","operation":"catalogue-write","state":"prepared","intended_generation":1,"actions":[],"cleanup":[]}}"#
        )
    }

    fn root_with_transaction(name: &str) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = TempDir::new(name);
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join("txn-partial");
        fs::create_dir_all(&txn_dir).expect("txn dir");
        (dir, root, txn_dir)
    }

    #[test]
    fn partial_journal_artifacts_never_trigger_destructive_cleanup() {
        // Before temporary journal creation: transaction directory exists, no journal.
        let (_dir, root, txn_dir) = root_with_transaction("journal-before-temp");
        fs::write(txn_dir.join("valuable.bin"), b"not staging").expect("valuable");
        assert!(CanonicalLibrary::open(&root).is_err());
        assert!(txn_dir.join("valuable.bin").is_file());
        assert!(txn_dir.is_dir());

        // Temporary file exists but is incomplete.
        let (_dir, root, txn_dir) = root_with_transaction("journal-partial-temp");
        fs::write(txn_dir.join("journal.flux.tmp"), b"{").expect("temp");
        assert!(CanonicalLibrary::open(&root).is_err());
        assert!(txn_dir.join("journal.flux.tmp").is_file());
        assert!(txn_dir.is_dir());

        // Final journal exists but content is truncated.
        let (_dir, root, txn_dir) = root_with_transaction("journal-truncated");
        fs::write(txn_dir.join("journal.flux"), b"{\"schema_version\":1").expect("journal");
        assert!(CanonicalLibrary::open(&root).is_err());
        assert!(txn_dir.join("journal.flux").is_file());
        assert!(txn_dir.is_dir());

        // Complete content synced into the temporary file, before final rename.
        let (_dir, root, txn_dir) = root_with_transaction("journal-before-rename");
        fs::write(
            txn_dir.join("journal.flux.tmp"),
            valid_journal("txn-partial"),
        )
        .expect("temp complete");
        assert!(CanonicalLibrary::open(&root).is_err());
        assert!(txn_dir.join("journal.flux.tmp").is_file());
        assert!(txn_dir.is_dir());

        // Complete journal after rename, before directory sync: recovery sees a
        // valid uncommitted journal and removes only that transaction directory.
        let (_dir, root, txn_dir) = root_with_transaction("journal-after-rename");
        fs::write(txn_dir.join("journal.flux"), valid_journal("txn-partial")).expect("journal");
        assert!(CanonicalLibrary::open(&root).is_ok());
        assert!(!txn_dir.exists());
    }

    #[test]
    fn valid_transaction_can_proceed_after_ambiguous_journal_is_removed() {
        let dir = TempDir::new("journal-recover-valid");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let txn_dir = root.join(TRANSACTIONS_DIR_NAME).join("txn-broken");
        fs::create_dir_all(&txn_dir).expect("txn");
        fs::write(txn_dir.join("journal.flux"), b"{").expect("broken");
        assert!(CanonicalLibrary::open(&root).is_err());
        // Explicit cleanup of the ambiguous test artifact, never automatic
        // transaction recovery.
        fs::remove_dir_all(&txn_dir).expect("remove ambiguous journal");
        let mut library = CanonicalLibrary::open(&root).expect("reopen");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let source = dir.write("source/song.wav", b"valid after journal failure");
        assert!(
            library
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
                .is_ok()
        );
    }
}

#[cfg(test)]
mod multiple_transaction_tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    fn journal(txn: &str, state: &str, actions: &str, cleanup: &str) -> String {
        format!(
            r#"{{"schema_version":1,"kind":"flux-transaction","transaction_id":"{txn}","operation":"test","state":"{state}","intended_generation":1,"actions":[{actions}],"cleanup":[{cleanup}]}}"#
        )
    }

    #[test]
    fn multiple_uncommitted_transactions_roll_back_independently() {
        let dir = TempDir::new("multi-uncommitted");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let transactions = root.join(TRANSACTIONS_DIR_NAME);
        for name in ["txn-a", "txn-b"] {
            let txn = transactions.join(name);
            fs::create_dir_all(&txn).expect("txn");
            fs::write(
                txn.join("journal.flux"),
                journal(
                    name,
                    "prepared",
                    &format!(r#"{{"target":"{name}.wav","existed":false}}"#),
                    "",
                ),
            )
            .expect("journal");
            fs::write(root.join(format!("{name}.wav")), b"partial").expect("partial");
        }
        fs::write(root.join("unrelated.wav"), b"keep").expect("keep");
        let reopened = CanonicalLibrary::open(&root).expect("rollback both");
        assert!(!root.join("txn-a.wav").exists());
        assert!(!root.join("txn-b.wav").exists());
        assert!(root.join("unrelated.wav").is_file());
        assert_eq!(fs::read_dir(&transactions).expect("txn dir").count(), 0);
        drop(reopened);
        assert!(CanonicalLibrary::open(&root).is_ok());
    }

    #[test]
    fn committed_cleanup_and_uncommitted_rollback_are_independent() {
        let dir = TempDir::new("multi-mixed");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let catalog_path = root.join(CATALOG_FILE_NAME);
        let mut catalog: serde_json::Value =
            serde_json::from_slice(&fs::read(&catalog_path).expect("catalog")).expect("json");
        catalog["transaction_id"] = serde_json::json!("txn-committed");
        fs::write(
            &catalog_path,
            serde_json::to_vec_pretty(&catalog).expect("catalog bytes"),
        )
        .expect("catalog");
        let transactions = root.join(TRANSACTIONS_DIR_NAME);
        let committed = transactions.join("txn-committed");
        fs::create_dir_all(&committed).expect("committed");
        fs::write(
            committed.join("journal.flux"),
            journal("txn-committed", "cleaning", "", r#""obsolete.wav""#),
        )
        .expect("committed journal");
        fs::write(root.join("obsolete.wav"), b"old").expect("old");
        let uncommitted = transactions.join("txn-uncommitted");
        fs::create_dir_all(&uncommitted).expect("uncommitted");
        fs::write(
            uncommitted.join("journal.flux"),
            journal(
                "txn-uncommitted",
                "copying",
                r#"{"target":"created.wav","existed":false}"#,
                "",
            ),
        )
        .expect("uncommitted journal");
        fs::write(root.join("created.wav"), b"partial").expect("partial");
        let reopened = CanonicalLibrary::open(&root).expect("mixed recovery");
        assert!(!root.join("obsolete.wav").exists());
        assert!(!root.join("created.wav").exists());
        assert_eq!(fs::read_dir(&transactions).expect("txn dir").count(), 0);
        drop(reopened);
        assert!(CanonicalLibrary::open(&root).is_ok());
    }
}

#[cfg(test)]
mod process_crash_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use super::*;
    use crate::test_support::TempDir;

    const CHILD_ROOT: &str = "FLUX_TEST_CRASH_ROOT";
    const CHILD_POINT: &str = "FLUX_TEST_CRASH_POINT";
    const CHILD_RECOVERY: &str = "FLUX_TEST_CRASH_RECOVERY_ROOT";

    fn point_from_name(name: &str) -> TransactionFailPoint {
        match name {
            "after-journal" => TransactionFailPoint::AfterJournal,
            "during-audio-copy" => TransactionFailPoint::DuringAudioCopy,
            "during-metadata-publish" => TransactionFailPoint::DuringMetadataPublish,
            "after-catalog-publish" => TransactionFailPoint::AfterCatalogPublish,
            "during-cleanup" => TransactionFailPoint::DuringCleanup,
            "before-journal-finalize" => TransactionFailPoint::BeforeJournalFinalize,
            other => panic!("unknown child boundary {other}"),
        }
    }

    #[test]
    fn child_crash_helper() {
        let Ok(root) = std::env::var(CHILD_ROOT) else {
            return;
        };
        let point = point_from_name(&std::env::var(CHILD_POINT).expect("child crash boundary"));
        let root = PathBuf::from(root);
        let mut library = CanonicalLibrary::create(&root).expect("create child library");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source_dir = root.parent().expect("parent").join("child-source");
        fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join("song.wav");
        fs::write(&source, b"child crash payload").expect("source");
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
            .expect("track");
        library
            .set_lyrics(track, Some("[00:00.00]child lyrics"))
            .expect("lyrics");
        library.set_rating(track, Some(4), true).expect("rating");
        library
            .set_playlist(None, "Keep", vec![track])
            .expect("playlist");
        let destination = library
            .create_album(b, "Destination", ReleaseType::Ep)
            .expect("destination");
        let plan = library
            .plan_ownership_transfer(track, b, OwnershipDestination::Existing(destination))
            .expect("plan");
        library.set_transaction_failure(Some(TransactionFailure {
            point,
            interrupt: true,
            abort: true,
        }));
        let _ = library.execute_ownership_transfer(&plan);
        panic!("child did not terminate at {point:?}");
    }

    #[test]
    fn child_metadata_crash_helper() {
        let Ok(root) = std::env::var(CHILD_ROOT) else {
            return;
        };
        let point = point_from_name(&std::env::var(CHILD_POINT).expect("child crash boundary"));
        let root = PathBuf::from(root);
        let mut library = CanonicalLibrary::create(&root).expect("create child library");
        let artist = library
            .create_artist("Before Metadata", Vec::new())
            .expect("artist");
        let source_dir = root.parent().expect("parent").join("child-metadata-source");
        fs::create_dir_all(&source_dir).expect("source dir");
        let source = source_dir.join("song.wav");
        fs::write(&source, b"metadata child payload").expect("source");
        library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Metadata Song".to_owned(),
                artist_id: Some(artist),
                artist_name: "Before Metadata".to_owned(),
                credited_artist_ids: vec![artist],
                album_title: Some("Metadata Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("track");
        library.set_transaction_failure(Some(TransactionFailure {
            point,
            interrupt: true,
            abort: true,
        }));
        let _ = library.rename_artist(artist, "After Metadata");
        panic!("child did not terminate at {point:?}");
    }

    #[test]
    fn child_recovery_crash_helper() {
        let Ok(root) = std::env::var(CHILD_RECOVERY) else {
            return;
        };
        // The parent prepared a committed journal and sets the abort marker.
        let _ = CanonicalLibrary::open(Path::new(&root));
        panic!("child recovery did not terminate in cleanup");
    }

    fn spawn(boundary: &str, root: &Path) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "canonical::process_crash_tests::child_crash_helper",
                "--nocapture",
            ])
            .env(CHILD_ROOT, root)
            .env(CHILD_POINT, boundary)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn child")
    }

    fn spawn_recovery(root: &Path) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "canonical::process_crash_tests::child_recovery_crash_helper",
                "--nocapture",
            ])
            .env(CHILD_RECOVERY, root)
            .env("FLUX_TEST_ABORT_RECOVERY", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn recovery child")
    }

    fn spawn_metadata(boundary: &str, root: &Path) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "canonical::process_crash_tests::child_metadata_crash_helper",
                "--nocapture",
            ])
            .env(CHILD_ROOT, root)
            .env(CHILD_POINT, boundary)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn metadata child")
    }

    fn assert_transfer_state(root: &Path, committed: bool) {
        let reopened = CanonicalLibrary::open(root).expect("reopen after child crash");
        let id = TrackId::new(1);
        let track = reopened.track(id).expect("track retained");
        assert_eq!(
            track.owner_artist_id,
            if committed {
                ArtistId::new(2)
            } else {
                ArtistId::new(1)
            }
        );
        assert_eq!(
            track.credited_artist_ids,
            vec![ArtistId::new(1), ArtistId::new(2)]
        );
        assert_eq!(
            track.album_id,
            Some(if committed {
                AlbumId::new(2)
            } else {
                AlbumId::new(1)
            })
        );
        assert_eq!(
            reopened.rating(id),
            Some(TrackRating {
                rating: Some(4),
                favorite: true
            })
        );
        assert_eq!(reopened.playlists().count(), 1);
        assert_eq!(reopened.tracks().count(), 1);
        assert_eq!(
            fs::read(reopened.track_audio_path(id).expect("audio")).expect("audio"),
            b"child crash payload"
        );
        let lyrics = reopened.track_lyrics_path(id).expect("lyrics path");
        assert_eq!(
            fs::read_to_string(lyrics).expect("lyrics"),
            "[00:00.00]child lyrics"
        );
        let old_audio =
            root.join("artists/artist_000001/albums/album_000001/tracks/track_000001.wav");
        let new_audio =
            root.join("artists/artist_000002/albums/album_000002/tracks/track_000001.wav");
        if committed {
            assert!(!old_audio.exists());
            assert!(new_audio.is_file());
        } else {
            assert!(old_audio.is_file());
            assert!(!new_audio.exists());
        }
        drop(reopened);
        assert!(
            CanonicalLibrary::open(root).is_ok(),
            "second recovery idempotent"
        );
    }

    #[test]
    fn actual_child_termination_at_transfer_boundaries_recovers() {
        for (boundary, committed) in [
            ("after-journal", false),
            ("during-audio-copy", false),
            ("during-metadata-publish", false),
            ("after-catalog-publish", true),
            ("during-cleanup", true),
        ] {
            let dir = TempDir::new(&format!("child-{boundary}"));
            let root = dir.path().join("FluxLibrary");
            let status = spawn(boundary, &root);
            assert!(
                !status.success(),
                "child must terminate abruptly at {boundary}, got {status:?}"
            );
            assert_transfer_state(&root, committed);
        }
    }

    #[test]
    fn actual_child_termination_at_metadata_boundaries_recovers() {
        for (boundary, committed) in [
            ("after-journal", false),
            ("during-metadata-publish", false),
            ("after-catalog-publish", true),
            ("before-journal-finalize", true),
        ] {
            let dir = TempDir::new(&format!("child-metadata-{boundary}"));
            let root = dir.path().join("FluxLibrary");
            let status = spawn_metadata(boundary, &root);
            assert!(
                !status.success(),
                "child must terminate abruptly at {boundary}, got {status:?}"
            );
            let reopened = CanonicalLibrary::open(&root)
                .unwrap_or_else(|err| panic!("{boundary}: reopen failed: {err}"));
            assert_eq!(
                reopened
                    .artist(flux_core::ArtistId::new(1))
                    .map(|artist| artist.name.as_str()),
                Some(if committed {
                    "After Metadata"
                } else {
                    "Before Metadata"
                }),
                "{boundary}: metadata commit boundary was inconsistent"
            );
            assert_eq!(reopened.generation(), if committed { 4 } else { 3 });
            assert_eq!(
                fs::read_dir(root.join(TRANSACTIONS_DIR_NAME))
                    .expect("transactions")
                    .flatten()
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .count(),
                0,
                "{boundary}: transaction directory was not finalized"
            );
            drop(reopened);
            assert!(CanonicalLibrary::open(&root).is_ok());
        }
    }

    #[test]
    fn actual_child_termination_during_recovery_can_resume() {
        let dir = TempDir::new("child-recovery");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let obsolete = root.join("obsolete.wav");
        fs::write(&obsolete, b"old").expect("obsolete");
        let txn = "txn-child-recovery";
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
                "transaction_id": "txn-child-recovery",
                "operation": "ownership-transfer",
                "state": "cleaning",
                "intended_generation": 2,
                "actions": [],
                "cleanup": ["obsolete.wav"]
            }"#,
        )
        .expect("journal");
        let status = spawn_recovery(&root);
        assert!(!status.success(), "recovery child must terminate abruptly");
        assert!(
            obsolete.is_file(),
            "interrupted recovery leaves journal-owned file"
        );
        assert!(txn_dir.is_dir());
        let reopened = CanonicalLibrary::open(&root).expect("resume recovery");
        assert!(!obsolete.exists());
        assert!(!txn_dir.exists());
        drop(reopened);
        assert!(CanonicalLibrary::open(&root).is_ok());
    }
}

#[cfg(test)]
mod track_deletion_tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    struct Fixture {
        _dir: TempDir,
        library: CanonicalLibrary,
        track: TrackId,
        a: ArtistId,
        b: ArtistId,
        old_audio: std::path::PathBuf,
        old_record: std::path::PathBuf,
        old_lyrics: std::path::PathBuf,
        playlist: PlaylistId,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = TempDir::new(name);
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source = dir.write("source/song.wav", b"track deletion payload");
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
        let old_audio = library.track_audio_path(track).expect("audio");
        let old_record = library.track_record_path(track).expect("record");
        let old_lyrics = library.track_lyrics_path(track).expect("lyrics");
        Fixture {
            _dir: dir,
            library,
            track,
            a,
            b,
            old_audio,
            old_record,
            old_lyrics,
            playlist,
        }
    }

    #[test]
    fn single_track_deletion_preserves_unrelated_entities_and_resources() {
        let mut fx = fixture("delete-one-track");
        let second_source = fx._dir.write("source/second.wav", b"second payload");
        let second = fx
            .library
            .import_track(CanonicalImportedTrack {
                source: second_source,
                title: "Second".to_owned(),
                artist_id: Some(fx.a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![fx.a, fx.b],
                album_title: Some("Source".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(2),
                disc_number: None,
                duration_ms: None,
            })
            .expect("second track");
        fx.library
            .set_lyrics(second, Some("[00:01.00]second lyrics"))
            .expect("second lyrics");
        fx.library
            .set_rating(second, Some(5), false)
            .expect("second rating");
        fx.library
            .set_playlist(Some(fx.playlist), "Keep", vec![fx.track, second])
            .expect("playlist both");
        let second_audio = fx.library.track_audio_path(second).expect("second audio");
        let second_record = fx.library.track_record_path(second).expect("second record");
        let second_lyrics = fx.library.track_lyrics_path(second).expect("second lyrics");
        let album_id = fx
            .library
            .track(fx.track)
            .expect("track")
            .album_id
            .expect("album");
        let cover = fx._dir.write("cover.png", b"album cover");
        fx.library
            .set_cover(album_id, &cover, "png")
            .expect("album cover");
        let managed_cover = fx
            .library
            .album_cover_path(album_id)
            .expect("managed cover");
        let next_track_id = fx.library.catalog.next_track_id;
        let generation = fx.library.catalog.generation;

        let plan = fx
            .library
            .plan_track_deletion(fx.track)
            .expect("delete plan");
        let report = fx.library.delete_track(&plan).expect("delete track");
        assert!(report.cleanup_errors.is_empty(), "{report:?}");
        assert_eq!(report.track_id, fx.track);
        assert!(fx.library.track(fx.track).is_none());
        assert!(fx.library.track(second).is_some());
        assert!(fx.library.album(album_id).is_some());
        assert!(fx.library.artist(fx.a).is_some());
        assert!(fx.library.artist(fx.b).is_some());
        assert_eq!(
            fx.library.album(album_id).expect("album").track_order,
            vec![second]
        );
        assert!(fx.library.rating(fx.track).is_none());
        assert!(fx.library.rating(second).is_some());
        assert_eq!(
            fx.library
                .playlist(fx.playlist)
                .expect("playlist")
                .track_ids,
            vec![second]
        );
        assert_eq!(fx.library.catalog.next_track_id, next_track_id);
        assert_eq!(fx.library.catalog.generation, generation + 1);
        assert!(fx.library.album(album_id).expect("album").cover.is_some());
        assert!(managed_cover.is_file(), "shared album artwork was removed");
        assert!(!fx.old_audio.exists());
        assert!(!fx.old_record.exists());
        assert!(!fx.old_lyrics.exists());
        assert!(second_audio.is_file());
        assert!(second_record.is_file());
        assert!(second_lyrics.is_file());
        assert_eq!(
            fs::read_to_string(fx.library.track_lyrics_path(second).expect("second lyrics"))
                .expect("second lyrics text"),
            "[00:01.00]second lyrics"
        );
    }

    #[test]
    fn artist_metadata_image_and_track_artwork_round_trip_and_delete_cleanup() {
        let mut fx = fixture("artist-track-artwork");
        let artist_image = fx._dir.write("artist.png", b"\x89PNG\r\n\x1a\nartist");
        fx.library
            .update_artist_metadata(
                fx.a,
                "Renamed A",
                Some("Warm synthesizer artist".to_owned()),
            )
            .expect("artist metadata");
        fx.library
            .set_artist_image(fx.a, &artist_image, "png")
            .expect("artist image");
        let managed_artist_image = fx
            .library
            .artist_image_path(fx.a)
            .expect("artist image path");
        assert!(managed_artist_image.is_file());

        let album_id = fx
            .library
            .track(fx.track)
            .expect("track")
            .album_id
            .expect("album");
        let album_cover = fx._dir.write("album.png", b"\x89PNG\r\n\x1a\nalbum");
        fx.library
            .set_cover(album_id, &album_cover, "png")
            .expect("album cover");
        let managed_album_cover = fx
            .library
            .album_cover_path(album_id)
            .expect("album cover path");
        assert!(managed_album_cover.is_file());

        let track_art = fx._dir.write("track.webp", b"RIFF....WEBPtrack");
        fx.library
            .set_track_artwork(fx.track, &track_art, "webp")
            .expect("track artwork");
        let managed_track_art = fx
            .library
            .track_artwork_path(fx.track)
            .expect("track artwork path");
        assert!(managed_track_art.is_file());
        assert_eq!(
            fx.library
                .track(fx.track)
                .and_then(|track| track.artwork.as_deref()),
            Some("track_1.webp")
        );
        fx.library
            .remove_track_artwork(fx.track)
            .expect("remove track artwork");
        assert!(!managed_track_art.exists());
        assert!(fx.library.track_artwork_path(fx.track).is_none());
        assert!(
            fx.library
                .album_cover_path(album_id)
                .is_some_and(|path| path.is_file()),
            "removing a Track override restores Album fallback"
        );
        let replacement_art = fx._dir.write("track2.png", b"\x89PNG\r\n\x1a\ntrack2");
        fx.library
            .set_track_artwork(fx.track, &replacement_art, "png")
            .expect("re-add track artwork");
        let managed_track_art = fx
            .library
            .track_artwork_path(fx.track)
            .expect("track artwork path");
        assert!(managed_track_art.is_file());
        let replacement_cover = fx
            ._dir
            .write("album-replacement.jpg", b"\xff\xd8\xffreplacement");
        fx.library
            .set_cover(album_id, &replacement_cover, "jpg")
            .expect("replace album cover");
        assert!(!managed_album_cover.exists());
        assert!(
            managed_track_art.is_file(),
            "Album cover replacement does not touch Track overrides"
        );
        assert_eq!(
            fx.library
                .track(fx.track)
                .and_then(|track| track.artwork.as_deref()),
            Some("track_1.png")
        );
        let managed_album_cover = fx
            .library
            .album_cover_path(album_id)
            .expect("replacement album cover path");
        assert!(managed_album_cover.is_file());

        let destination = fx
            .library
            .create_album(fx.a, "Artwork destination", ReleaseType::Album)
            .expect("destination album");
        let move_plan = fx
            .library
            .plan_track_move(fx.track, Some(destination), 0)
            .expect("move plan");
        fx.library.execute_track_move(&move_plan).expect("move");
        assert_eq!(
            fx.library.track(fx.track).map(|track| track.album_id),
            Some(Some(destination))
        );
        assert!(
            managed_track_art.is_file(),
            "track artwork survives album move"
        );
        assert_eq!(
            fx.library
                .artist(fx.a)
                .and_then(|artist| artist.image.as_deref()),
            Some("image.png")
        );
        assert_eq!(
            fx.library
                .artist(fx.a)
                .and_then(|artist| artist.description.as_deref()),
            Some("Warm synthesizer artist")
        );

        let root = fx.library.root.clone();
        drop(fx.library);
        let reopened = CanonicalLibrary::open(&root).expect("reopen");
        assert_eq!(
            reopened
                .artist(fx.a)
                .and_then(|artist| artist.description.as_deref()),
            Some("Warm synthesizer artist")
        );
        assert!(
            reopened
                .artist_image_path(fx.a)
                .is_some_and(|path| path.is_file())
        );
        assert_eq!(
            reopened.track(fx.track).map(|track| track.album_id),
            Some(Some(destination))
        );
        assert!(
            reopened
                .track_artwork_path(fx.track)
                .is_some_and(|path| path.is_file())
        );
        assert_eq!(
            reopened.album(album_id).expect("source album").track_order,
            Vec::<TrackId>::new(),
            "source album remains empty"
        );
        drop(reopened);

        let mut reopened = CanonicalLibrary::open(&root).expect("reopen for delete");
        let deletion = reopened
            .plan_track_deletion(fx.track)
            .expect("deletion plan");
        let report = reopened.delete_track(&deletion).expect("delete");
        assert!(report.cleanup_errors.is_empty(), "{report:?}");
        assert!(reopened.artist(fx.a).is_some(), "artist retained");
        assert!(
            reopened
                .artist_image_path(fx.a)
                .is_some_and(|path| path.is_file()),
            "artist image is not owned by the deleted track"
        );
        assert!(
            !managed_track_art.exists(),
            "track artwork cleaned after deletion"
        );
        assert!(
            reopened
                .album_cover_path(album_id)
                .is_some_and(|path| path.is_file()),
            "Track deletion never deletes unrelated Album artwork"
        );
    }

    #[test]
    fn import_batch_enriches_track_album_artwork_and_lyrics_atomically() {
        use super::{
            CanonicalImportAlbumRef, CanonicalImportArtistRef, CanonicalImportAsset,
            CanonicalImportTrackRequest,
        };

        let mut fx = fixture("import-enrichment");
        let source = fx._dir.write("incoming/enriched.wav", b"enriched audio");
        let track_art = fx
            ._dir
            .write("incoming/track.png", b"\x89PNG\r\n\x1a\ntrack");
        let album_art = fx
            ._dir
            .write("incoming/album.png", b"\x89PNG\r\n\x1a\nalbum");
        let request = CanonicalImportTrackRequest {
            source: source.clone(),
            title: "Enriched".to_owned(),
            credited_artists: vec![CanonicalImportArtistRef::Existing(fx.a)],
            owner_index: 0,
            album: CanonicalImportAlbumRef::ByName {
                title: "Enriched Album".to_owned(),
                release_type: ReleaseType::Album,
            },
            track_number: Some(1),
            disc_number: None,
            duration_ms: None,
            artwork: Some(CanonicalImportAsset {
                source: track_art.clone(),
                fingerprint: ContentFingerprint::of_file(&track_art)
                    .expect("track art fingerprint"),
            }),
            album_artwork: Some(CanonicalImportAsset {
                source: album_art.clone(),
                fingerprint: ContentFingerprint::of_file(&album_art)
                    .expect("album art fingerprint"),
            }),
            lyrics: Some("[00:01.00]enriched lyrics".to_owned()),
            allow_duplicate_content: false,
            expected_source_fingerprint: Some(
                ContentFingerprint::of_file(&source).expect("audio fingerprint"),
            ),
        };
        let old_album = fx.library.track(fx.track).expect("old track").album_id;
        let generation = fx.library.catalog.generation;
        let ids = fx
            .library
            .import_review_batch(generation, vec![request], Vec::new())
            .expect("atomic enriched import");
        assert_eq!(ids.len(), 1);
        let id = ids[0];
        let track = fx.library.track(id).expect("imported track");
        let album = fx
            .library
            .album(track.album_id.expect("album"))
            .expect("album");
        let imported_album_id = album.id;
        assert_eq!(album.title, "Enriched Album");
        let cover = fx.library.album_cover_path(album.id).expect("album cover");
        assert_eq!(
            fs::read(&cover).expect("cover bytes"),
            b"\x89PNG\r\n\x1a\nalbum"
        );
        let artwork = fx.library.track_artwork_path(id).expect("track artwork");
        assert_eq!(
            fs::read(&artwork).expect("art bytes"),
            b"\x89PNG\r\n\x1a\ntrack"
        );
        assert_eq!(
            fs::read_to_string(fx.library.track_lyrics_path(id).expect("lyrics path"))
                .expect("lyrics"),
            "[00:01.00]enriched lyrics"
        );
        assert_eq!(album.track_order, vec![id]);
        assert_eq!(
            fx.library.track(fx.track).expect("old track").album_id,
            old_album
        );

        let updated_art = fx
            ._dir
            .write("incoming/track-updated.png", b"\x89PNG\r\n\x1a\nupdated");
        let updated_album_art = fx
            ._dir
            .write("incoming/album-updated.jpg", b"\xff\xd8\xffupdated");
        let track = fx.library.track(id).expect("track for update");
        let update = super::CanonicalTrackMetadataUpdate {
            track_id: id,
            expected_fingerprint: track.fingerprint,
            expected_album_id: track.album_id,
            expected_title: track.title.clone(),
            expected_credited_artist_ids: track.credited_artist_ids.clone(),
            expected_track_number: track.track_number,
            expected_disc_number: track.disc_number,
            expected_duration_ms: track.duration_ms,
            title: track.title.clone(),
            credited_artists: track
                .credited_artist_ids
                .iter()
                .map(|artist| CanonicalImportArtistRef::Existing(*artist))
                .collect(),
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            artwork: Some(CanonicalImportAsset {
                source: updated_art.clone(),
                fingerprint: ContentFingerprint::of_file(&updated_art).expect("updated art"),
            }),
            album_artwork: Some(CanonicalImportAsset {
                source: updated_album_art.clone(),
                fingerprint: ContentFingerprint::of_file(&updated_album_art)
                    .expect("updated album art"),
            }),
            lyrics: Some("[00:02.00]updated lyrics".to_owned()),
        };
        fx.library
            .import_review_batch(fx.library.catalog.generation, Vec::new(), vec![update])
            .expect("enrich existing track atomically");
        assert_eq!(
            fs::read(fx.library.track_artwork_path(id).expect("updated artwork"))
                .expect("updated artwork bytes"),
            b"\x89PNG\r\n\x1a\nupdated"
        );
        assert_eq!(
            fs::read(
                fx.library
                    .album_cover_path(imported_album_id)
                    .expect("updated album cover")
            )
            .expect("updated cover bytes"),
            b"\xff\xd8\xffupdated"
        );
        assert_eq!(
            fs::read_to_string(fx.library.track_lyrics_path(id).expect("updated lyrics"))
                .expect("updated lyrics text"),
            "[00:02.00]updated lyrics"
        );
        assert_eq!(
            fx.library.track(id).expect("track").album_id,
            Some(imported_album_id)
        );
    }

    #[test]
    fn standalone_track_moves_into_and_out_of_an_album_without_losing_identity_or_resources() {
        let mut fx = fixture("standalone-membership");
        let source = fx._dir.write("standalone/single.wav", b"standalone audio");
        let track = fx
            .library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Standalone Single".to_owned(),
                artist_id: Some(fx.a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![fx.a],
                album_title: None,
                release_type: ReleaseType::Single,
                track_number: None,
                disc_number: None,
                duration_ms: None,
            })
            .expect("standalone import");
        assert_eq!(fx.library.track(track).expect("track").album_id, None);
        assert!(
            fx.library
                .track_audio_path(track)
                .expect("standalone audio")
                .to_string_lossy()
                .contains("/standalone/")
        );
        fx.library
            .set_lyrics(track, Some("[00:01.00]standalone lyrics"))
            .expect("lyrics");
        let artwork_source = fx
            ._dir
            .write("standalone/art.png", b"\x89PNG\r\n\x1a\nsingle");
        fx.library
            .set_track_artwork(track, &artwork_source, "png")
            .expect("track artwork");
        let managed_art = fx.library.track_artwork_path(track).expect("managed art");

        let album = fx
            .library
            .create_album(fx.a, "Holding Album", ReleaseType::Album)
            .expect("album");
        let cover_source = fx
            ._dir
            .write("standalone/cover.png", b"\x89PNG\r\n\x1a\ncover");
        fx.library
            .set_cover(album, &cover_source, "png")
            .expect("album cover");
        let attach = fx
            .library
            .plan_track_move(track, Some(album), 0)
            .expect("attach plan");
        assert_eq!(attach.from_album_id, None);
        assert_eq!(attach.to_album_id, Some(album));
        let report = fx.library.execute_track_move(&attach).expect("attach");
        assert_eq!(report.from_album_id, None);
        assert_eq!(report.to_album_id, Some(album));
        assert_eq!(fx.library.album_tracks(album), vec![track]);
        assert_eq!(
            fx.library.track(track).expect("track").album_id,
            Some(album)
        );
        assert_eq!(
            fx.library
                .track_lyrics_path(track)
                .and_then(|path| fs::read_to_string(path).ok())
                .as_deref(),
            Some("[00:01.00]standalone lyrics")
        );
        assert!(managed_art.is_file(), "track artwork follows the TrackId");
        assert!(
            fx.library
                .album_cover_path(album)
                .is_some_and(|path| path.is_file())
        );

        let detach = fx
            .library
            .plan_track_move(track, None, 0)
            .expect("detach plan");
        assert_eq!(detach.from_album_id, Some(album));
        assert_eq!(detach.to_album_id, None);
        let report = fx.library.execute_track_move(&detach).expect("detach");
        assert_eq!(report.to_album_id, None);
        assert_eq!(fx.library.track(track).expect("track").album_id, None);
        assert!(
            fx.library
                .track_audio_path(track)
                .expect("standalone audio")
                .to_string_lossy()
                .contains("/standalone/")
        );
        assert_eq!(
            fx.library
                .track_lyrics_path(track)
                .and_then(|path| fs::read_to_string(path).ok())
                .as_deref(),
            Some("[00:01.00]standalone lyrics")
        );
        assert!(managed_art.is_file());
        assert!(
            fx.library
                .album(album)
                .is_some_and(|album| album.track_order.is_empty()),
            "source Album is retained empty"
        );
        assert!(
            fx.library
                .albums()
                .all(|album| !album.track_order.contains(&track)),
            "no stale Album order reference"
        );

        let metadata_plan = fx
            .library
            .plan_track_metadata_update(track)
            .expect("standalone metadata plan");
        fx.library
            .execute_track_metadata_update(
                &metadata_plan,
                "Standalone Renamed",
                vec![fx.a],
                None,
                None,
                None,
            )
            .expect("standalone metadata update");
        assert_eq!(
            fx.library.track(track).expect("track").title,
            "Standalone Renamed"
        );
        assert_eq!(fx.library.track(track).expect("track").album_id, None);
        assert!(managed_art.is_file());

        let replacement_source = fx
            ._dir
            .write("standalone/replacement.wav", b"replacement audio");
        let replacement = fx
            .library
            .plan_track_audio_replacement(track, &replacement_source)
            .expect("standalone replacement plan");
        fx.library
            .execute_track_audio_replacement(&replacement)
            .expect("standalone replacement");
        assert_eq!(fx.library.track(track).expect("track").album_id, None);
        assert_eq!(
            fx.library
                .track_lyrics_path(track)
                .and_then(|path| fs::read_to_string(path).ok())
                .as_deref(),
            Some("[00:01.00]standalone lyrics")
        );
        assert!(
            fx.library
                .track_artwork_path(track)
                .is_some_and(|path| path.is_file())
        );

        let root = fx.library.root.clone();
        drop(fx.library);
        let reopened = CanonicalLibrary::open(&root).expect("reopen standalone");
        assert_eq!(reopened.track(track).expect("track").album_id, None);
        assert_eq!(
            reopened
                .track_lyrics_path(track)
                .and_then(|path| fs::read_to_string(path).ok())
                .as_deref(),
            Some("[00:01.00]standalone lyrics")
        );
        assert!(
            reopened
                .track_artwork_path(track)
                .is_some_and(|path| path.is_file())
        );
        drop(reopened);

        let mut reopened = CanonicalLibrary::open(&root).expect("reopen for standalone delete");
        let deletion = reopened
            .plan_track_deletion(track)
            .expect("standalone deletion plan");
        assert_eq!(deletion.album_id, None);
        let report = reopened.delete_track(&deletion).expect("standalone delete");
        assert!(report.cleanup_errors.is_empty(), "{report:?}");
        assert_eq!(report.album_id, None);
        assert!(reopened.track(track).is_none());
        assert!(reopened.artist(fx.a).is_some(), "Artist retained");
        assert!(reopened.album(album).is_some(), "unrelated Album retained");
    }

    #[test]
    fn import_batch_can_publish_standalone_tracks_and_rejects_album_artwork_without_membership() {
        use super::{
            CanonicalImportAlbumRef, CanonicalImportArtistRef, CanonicalImportAsset,
            CanonicalImportTrackRequest,
        };

        let mut fx = fixture("import-standalone");
        let source = fx
            ._dir
            .write("incoming/standalone.wav", b"standalone batch audio");
        let request = CanonicalImportTrackRequest {
            source: source.clone(),
            title: "Batch Standalone".to_owned(),
            credited_artists: vec![CanonicalImportArtistRef::Existing(fx.a)],
            owner_index: 0,
            album: CanonicalImportAlbumRef::Standalone,
            track_number: None,
            disc_number: None,
            duration_ms: None,
            artwork: None,
            album_artwork: None,
            lyrics: Some("[00:03.00]batch standalone".to_owned()),
            allow_duplicate_content: false,
            expected_source_fingerprint: Some(
                ContentFingerprint::of_file(&source).expect("source fingerprint"),
            ),
        };
        let id = fx
            .library
            .import_review_batch(fx.library.catalog.generation, vec![request], Vec::new())
            .expect("standalone batch import")[0];
        assert_eq!(fx.library.track(id).expect("track").album_id, None);
        assert_eq!(
            fx.library
                .track_lyrics_path(id)
                .and_then(|path| fs::read_to_string(path).ok())
                .as_deref(),
            Some("[00:03.00]batch standalone")
        );

        let art = fx
            ._dir
            .write("incoming/standalone.png", b"\x89PNG\r\n\x1a\nstandalone");
        let invalid_source = fx
            ._dir
            .write("incoming/invalid.wav", b"invalid standalone audio");
        let invalid = CanonicalImportTrackRequest {
            source: invalid_source.clone(),
            title: "Invalid Standalone Artwork".to_owned(),
            credited_artists: vec![CanonicalImportArtistRef::Existing(fx.a)],
            owner_index: 0,
            album: CanonicalImportAlbumRef::Standalone,
            track_number: None,
            disc_number: None,
            duration_ms: None,
            artwork: None,
            album_artwork: Some(CanonicalImportAsset {
                source: art.clone(),
                fingerprint: ContentFingerprint::of_file(&art).expect("art fingerprint"),
            }),
            lyrics: None,
            allow_duplicate_content: false,
            expected_source_fingerprint: None,
        };
        let tracks_before = fx.library.tracks().count();
        let error = fx
            .library
            .import_review_batch(fx.library.catalog.generation, vec![invalid], Vec::new())
            .expect_err("Album artwork requires Album membership");
        assert!(
            error.to_string().contains("Album artwork requires"),
            "{error}"
        );
        assert_eq!(fx.library.tracks().count(), tracks_before);
    }

    #[test]
    fn import_conflicting_album_artwork_or_changed_asset_leaves_no_partial_state() {
        use super::{
            CanonicalImportAlbumRef, CanonicalImportArtistRef, CanonicalImportAsset,
            CanonicalImportTrackRequest,
        };

        let mut fx = fixture("import-artwork-failure");
        let source_one = fx._dir.write("incoming/one.wav", b"one audio");
        let source_two = fx._dir.write("incoming/two.wav", b"two audio");
        let art_one = fx._dir.write("incoming/one.png", b"\x89PNG\r\n\x1a\none");
        let art_two = fx._dir.write("incoming/two.png", b"\x89PNG\r\n\x1a\ntwo");
        let request = |source: &Path, art: &Path| CanonicalImportTrackRequest {
            source: source.to_path_buf(),
            title: source
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("track")
                .to_owned(),
            credited_artists: vec![CanonicalImportArtistRef::Existing(fx.a)],
            owner_index: 0,
            album: CanonicalImportAlbumRef::ByName {
                title: "Conflict Album".to_owned(),
                release_type: ReleaseType::Album,
            },
            track_number: None,
            disc_number: None,
            duration_ms: None,
            artwork: Some(CanonicalImportAsset {
                source: art.to_path_buf(),
                fingerprint: ContentFingerprint::of_file(art).expect("art fingerprint"),
            }),
            album_artwork: Some(CanonicalImportAsset {
                source: art.to_path_buf(),
                fingerprint: ContentFingerprint::of_file(art).expect("album art fingerprint"),
            }),
            lyrics: Some("[00:01.00]failure test".to_owned()),
            allow_duplicate_content: false,
            expected_source_fingerprint: Some(
                ContentFingerprint::of_file(source).expect("source fingerprint"),
            ),
        };
        let tracks_before = fx.library.tracks().count();
        let albums_before = fx.library.albums().count();
        let generation = fx.library.catalog.generation;
        let error = fx
            .library
            .import_review_batch(
                generation,
                vec![
                    request(&source_one, &art_one),
                    request(&source_two, &art_two),
                ],
                Vec::new(),
            )
            .expect_err("conflicting album artwork must fail the batch");
        assert!(error.to_string().contains("conflicting artwork"), "{error}");
        assert_eq!(fx.library.tracks().count(), tracks_before);
        assert_eq!(fx.library.albums().count(), albums_before);
        assert!(!fx.library.root.join(TRACK_ARTWORK_DIR_NAME).exists());

        let mut stale = request(&source_one, &art_one);
        stale.album = CanonicalImportAlbumRef::ByName {
            title: "Stale Album".to_owned(),
            release_type: ReleaseType::Album,
        };
        fs::write(&art_one, b"\x89PNG\r\n\x1a\nchanged").expect("change reviewed artwork");
        let error = fx
            .library
            .import_review_batch(fx.library.catalog.generation, vec![stale], Vec::new())
            .expect_err("changed reviewed artwork must fail");
        assert!(
            error.to_string().contains("changed after review"),
            "{error}"
        );
        assert_eq!(fx.library.tracks().count(), tracks_before);
        assert_eq!(fx.library.albums().count(), albums_before);
        assert!(
            fx.library
                .albums()
                .all(|album| album.title != "Stale Album")
        );
    }

    #[test]
    fn deleting_the_only_track_keeps_empty_album_and_artists() {
        let mut fx = fixture("delete-only-track");
        let album_id = fx
            .library
            .track(fx.track)
            .expect("track")
            .album_id
            .expect("album");
        let next_track_id = fx.library.catalog.next_track_id;
        let plan = fx
            .library
            .plan_track_deletion(fx.track)
            .expect("delete plan");
        let report = fx.library.delete_track(&plan).expect("delete track");
        assert!(report.cleanup_errors.is_empty(), "{report:?}");
        assert!(fx.library.track(fx.track).is_none());
        let album = fx.library.album(album_id).expect("empty album retained");
        assert!(album.track_order.is_empty());
        assert!(fx.library.artist(fx.a).is_some());
        assert!(fx.library.artist(fx.b).is_some());
        assert!(fx.library.rating(fx.track).is_none());
        assert!(
            fx.library
                .playlist(fx.playlist)
                .expect("playlist retained")
                .track_ids
                .is_empty()
        );
        assert_eq!(fx.library.catalog.next_track_id, next_track_id);
        assert!(!fx.old_audio.exists());
        assert!(!fx.old_record.exists());
        assert!(!fx.old_lyrics.exists());
    }

    #[test]
    fn missing_audio_replacement_repairs_managed_resource_and_preserves_references() {
        let mut fx = fixture("repair-missing-audio");
        let old_audio = fx.old_audio.clone();
        let old_record = fx.old_record.clone();
        let old_lyrics = fx.old_lyrics.clone();
        fs::remove_file(&old_audio).expect("make audio missing");
        let replacement = fx._dir.write("replacement/new.wav", b"replacement payload");
        let plan = fx
            .library
            .plan_track_audio_replacement(fx.track, &replacement)
            .expect("repair plan");
        assert_eq!(plan.current_fingerprint, None);
        assert_eq!(
            fx.library
                .rating(fx.track)
                .map(|rating| (rating.rating, rating.favorite)),
            Some((Some(4), true))
        );
        let report = fx
            .library
            .execute_track_audio_replacement(&plan)
            .expect("repair audio");
        assert!(report.restored_missing_audio);
        assert!(fx.library.track_available(fx.track));
        assert_eq!(
            fs::read(fx.library.track_audio_path(fx.track).expect("audio")).expect("new audio"),
            b"replacement payload"
        );
        assert!(!old_audio.exists());
        assert!(old_record.is_file());
        assert!(old_lyrics.is_file());
        assert_eq!(
            fs::read_to_string(fx.library.track_lyrics_path(fx.track).expect("lyrics path"))
                .expect("lyrics text"),
            "[00:00.00]lyrics"
        );
        assert_eq!(
            fx.library
                .playlist(fx.playlist)
                .expect("playlist")
                .track_ids,
            vec![fx.track]
        );
        assert_eq!(
            fx.library.track(fx.track).expect("track").title,
            "Song".to_owned()
        );
    }

    #[test]
    fn missing_audio_repair_is_rejected_if_the_old_file_reappears() {
        let mut fx = fixture("repair-reappeared");
        fs::remove_file(&fx.old_audio).expect("make audio missing");
        let replacement = fx._dir.write("replacement/new.wav", b"replacement payload");
        let plan = fx
            .library
            .plan_track_audio_replacement(fx.track, &replacement)
            .expect("repair plan");
        fs::write(&fx.old_audio, b"reappeared payload").expect("reappear");
        assert!(
            fx.library.execute_track_audio_replacement(&plan).is_err(),
            "a reappeared old file must invalidate the repair plan"
        );
        assert_eq!(
            fs::read(fx.library.track_audio_path(fx.track).expect("audio")).expect("audio"),
            b"reappeared payload"
        );
    }

    #[test]
    fn deletion_transaction_failure_before_commit_preserves_the_track_and_references() {
        let mut fx = fixture("delete-before-commit");
        let album_id = fx
            .library
            .track(fx.track)
            .expect("track")
            .album_id
            .expect("album");
        let plan = fx
            .library
            .plan_track_deletion(fx.track)
            .expect("delete plan");
        fx.library.set_transaction_failure(Some(TransactionFailure {
            point: TransactionFailPoint::BeforeCatalogPublish,
            interrupt: true,
            abort: false,
        }));
        let error = fx
            .library
            .delete_track(&plan)
            .expect_err("injected failure");
        assert!(error.to_string().contains("injected"), "{error}");
        let root = fx.library.root.clone();
        drop(fx.library);
        let reopened = CanonicalLibrary::open(&root).expect("recovery");
        assert!(reopened.track(fx.track).is_some());
        assert!(reopened.album(album_id).is_some());
        assert!(reopened.rating(fx.track).is_some());
        assert_eq!(
            reopened.playlist(fx.playlist).expect("playlist").track_ids,
            vec![fx.track]
        );
        assert!(fs::read_to_string(reopened.track_lyrics_path(fx.track).expect("lyrics")).is_ok());
        assert!(fx.old_audio.is_file());
        assert!(fx.old_record.is_file());
    }

    #[test]
    fn deletion_cleanup_failure_is_reported_after_catalogue_commit() {
        let mut fx = fixture("delete-cleanup-failure");
        fs::remove_file(&fx.old_audio).expect("remove old audio");
        fs::create_dir_all(&fx.old_audio).expect("replace audio with directory");
        fx._dir.write("keep.txt", b"not touched");
        let album_id = fx
            .library
            .track(fx.track)
            .expect("track")
            .album_id
            .expect("album");
        let plan = fx
            .library
            .plan_track_deletion(fx.track)
            .expect("delete plan");
        assert!(!plan.audio_present);
        let report = fx.library.delete_track(&plan).expect("catalogue delete");
        assert!(
            report
                .cleanup_errors
                .iter()
                .any(|error| error.contains("could not remove")),
            "{report:?}"
        );
        assert!(fx.library.track(fx.track).is_none());
        assert!(fx.library.album(album_id).is_some());
        assert!(fx.old_audio.is_dir(), "ambiguous resource was not deleted");
        assert!(!fx.old_record.exists());
    }
}
#[cfg(test)]
mod filesystem_edge_tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    struct EdgeFixture {
        _dir: TempDir,
        root: std::path::PathBuf,
        library: CanonicalLibrary,
        track: TrackId,
        b: ArtistId,
        destination: AlbumId,
        plan: OwnershipTransferPlan,
    }

    fn fixture(name: &str) -> EdgeFixture {
        let dir = TempDir::new(name);
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source = dir.write("source/song.wav", b"edge payload");
        let track = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Source".to_owned()),
                release_type: ReleaseType::Album,
                track_number: None,
                disc_number: None,
                duration_ms: None,
            })
            .expect("track");
        let destination = library
            .create_album(b, "Destination", ReleaseType::Album)
            .expect("album");
        let plan = library
            .plan_ownership_transfer(track, b, OwnershipDestination::Existing(destination))
            .expect("plan");
        EdgeFixture {
            _dir: dir,
            root,
            library,
            track,
            b,
            destination,
            plan,
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_only_destination_fails_without_committing_and_can_retry() {
        use std::os::unix::fs::PermissionsExt;

        let mut fx = fixture("readonly-destination");
        let album_dir = fx.library.album_dir(fx.destination).expect("album dir");
        let original = fs::metadata(&album_dir).expect("metadata").permissions();
        let mut readonly = original.clone();
        readonly.set_mode(0o555);
        fs::set_permissions(&album_dir, readonly).expect("chmod");
        // Root-style environments ignore permission bits; do not claim a test
        // that cannot fail.
        let writable_even_when_readonly = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(album_dir.join(".permission-probe"))
            .is_ok();
        if writable_even_when_readonly {
            let _ = fs::remove_file(album_dir.join(".permission-probe"));
            fs::set_permissions(&album_dir, original).expect("restore");
            return;
        }
        assert!(fx.library.execute_ownership_transfer(&fx.plan).is_err());
        assert_eq!(
            fx.library
                .track(fx.track)
                .map(|track| track.owner_artist_id),
            Some(ArtistId::new(1))
        );
        // The failed rollback is still pending; a second write must not start
        // until recovery completes.
        assert!(fx.library.execute_ownership_transfer(&fx.plan).is_err());
        fs::set_permissions(&album_dir, original).expect("restore");
        drop(fx.library);
        let mut recovered = CanonicalLibrary::open(&fx.root).expect("recover blocked rollback");
        assert_eq!(
            recovered.track(fx.track).map(|track| track.owner_artist_id),
            Some(ArtistId::new(1))
        );
        recovered
            .execute_ownership_transfer(&fx.plan)
            .expect("retry after permission repair");
        fx.library = recovered;
        assert_eq!(
            fx.library
                .track(fx.track)
                .map(|track| track.owner_artist_id),
            Some(fx.b)
        );
        drop(fx.library);
        assert!(CanonicalLibrary::open(&fx.root).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn destination_symlink_replacement_is_rejected_before_publication() {
        use std::os::unix::fs::symlink;

        let mut fx = fixture("symlink-target");
        let target = fx
            .root
            .join(fx.plan.new_audio_path().to_os_path().expect("path"));
        fs::create_dir_all(target.parent().expect("parent")).expect("dirs");
        let outside = fx.root.parent().expect("parent").join("outside.wav");
        fs::write(&outside, b"outside").expect("outside");
        symlink(&outside, &target).expect("symlink");
        assert!(fx.library.execute_ownership_transfer(&fx.plan).is_err());
        assert_eq!(
            fx.library
                .track(fx.track)
                .map(|track| track.owner_artist_id),
            Some(ArtistId::new(1))
        );
        assert!(outside.is_file());
        assert_eq!(fs::read(&outside).expect("outside"), b"outside");
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_permission_failure_preserves_journal_and_retries() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("cleanup-permission");
        let root = dir.path().join("FluxLibrary");
        CanonicalLibrary::create(&root).expect("create");
        let locked = root.join("locked");
        fs::create_dir_all(&locked).expect("locked");
        let obsolete = locked.join("obsolete.wav");
        fs::write(&obsolete, b"old").expect("obsolete");
        let txn = "txn-permission-cleanup";
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
                "transaction_id": "txn-permission-cleanup",
                "operation": "ownership-transfer",
                "state": "cleaning",
                "intended_generation": 2,
                "actions": [],
                "cleanup": ["locked/obsolete.wav"]
            }"#,
        )
        .expect("journal");
        let original = fs::metadata(&locked).expect("metadata").permissions();
        let mut readonly = original.clone();
        readonly.set_mode(0o555);
        fs::set_permissions(&locked, readonly).expect("chmod");
        let writable = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(locked.join(".probe"))
            .is_ok();
        if writable {
            let _ = fs::remove_file(locked.join(".probe"));
            fs::set_permissions(&locked, original).expect("restore");
            return;
        }
        assert!(CanonicalLibrary::open(&root).is_err());
        assert!(obsolete.is_file());
        assert!(txn_dir.is_dir());
        fs::set_permissions(&locked, original).expect("restore");
        let reopened = CanonicalLibrary::open(&root).expect("retry cleanup");
        assert!(!obsolete.exists());
        assert!(!txn_dir.exists());
        drop(reopened);
    }

    #[test]
    fn missing_source_after_plan_fails_safely_without_metadata_loss() {
        let mut fx = fixture("missing-source");
        let old_audio = fx.library.track_audio_path(fx.track).expect("old audio");
        fs::remove_file(&old_audio).expect("remove source");
        assert!(fx.library.execute_ownership_transfer(&fx.plan).is_err());
        assert!(fx.library.track(fx.track).is_some());
        assert_eq!(
            fx.library.track_credits(fx.track),
            Some([ArtistId::new(1), ArtistId::new(2)].as_slice())
        );
        assert!(!fx.library.track_available(fx.track));
        drop(fx.library);
        let reopened = CanonicalLibrary::open(&fx.root).expect("reopen missing source");
        assert!(reopened.track(fx.track).is_some());
        assert!(!reopened.track_available(fx.track));
    }
}

#[cfg(test)]
mod artist_deletion_tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    fn lyrics(library: &CanonicalLibrary, id: TrackId) -> Option<String> {
        library
            .track_lyrics_path(id)
            .and_then(|path| fs::read_to_string(path).ok())
    }

    struct Fixture {
        _dir: TempDir,
        root: std::path::PathBuf,
        library: CanonicalLibrary,
        deleted: ArtistId,
        survivor: ArtistId,
        collaborative: TrackId,
        non_collaborative: TrackId,
        credited_only: TrackId,
        source_album: AlbumId,
        destination_album: AlbumId,
        playlist: PlaylistId,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = TempDir::new(name);
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let deleted = library
            .create_artist("Deleted", Vec::new())
            .expect("deleted");
        let survivor = library
            .create_artist("Survivor", Vec::new())
            .expect("survivor");
        let source = dir.write("source/collab.wav", b"collaborative bytes");
        let collaborative = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Collaboration".to_owned(),
                artist_id: Some(deleted),
                artist_name: "Deleted".to_owned(),
                credited_artist_ids: vec![deleted, survivor],
                album_title: Some("Source Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("collaborative");
        let source = dir.write("source/solo.wav", b"solo bytes");
        let non_collaborative = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Solo".to_owned(),
                artist_id: Some(deleted),
                artist_name: "Deleted".to_owned(),
                credited_artist_ids: vec![deleted],
                album_title: Some("Source Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(2),
                disc_number: None,
                duration_ms: None,
            })
            .expect("non-collaborative");
        let source = dir.write("source/guest.wav", b"guest bytes");
        let credited_only = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Guest".to_owned(),
                artist_id: Some(survivor),
                artist_name: "Survivor".to_owned(),
                credited_artist_ids: vec![survivor, deleted],
                album_title: Some("Survivor Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("credited-only");
        let source_album = library
            .track(collaborative)
            .expect("collab")
            .album_id
            .expect("album");
        let destination_album = library
            .track(credited_only)
            .expect("guest")
            .album_id
            .expect("album");
        library
            .set_lyrics(collaborative, Some("collab lyrics"))
            .expect("collab lyrics");
        library
            .set_lyrics(non_collaborative, Some("solo lyrics"))
            .expect("solo lyrics");
        library
            .set_lyrics(credited_only, Some("guest lyrics"))
            .expect("guest lyrics");
        library
            .set_rating(collaborative, Some(5), true)
            .expect("collab rating");
        library
            .set_rating(non_collaborative, Some(2), false)
            .expect("solo rating");
        library
            .set_rating(credited_only, Some(3), true)
            .expect("guest rating");
        let playlist = library
            .set_playlist(
                None,
                "Mixed",
                vec![collaborative, non_collaborative, credited_only],
            )
            .expect("playlist");
        Fixture {
            _dir: dir,
            root,
            library,
            deleted,
            survivor,
            collaborative,
            non_collaborative,
            credited_only,
            source_album,
            destination_album,
            playlist,
        }
    }

    fn prepared_plan(fx: &mut Fixture) -> ArtistDeletionPlan {
        let mut plan = fx.library.plan_artist_deletion(fx.deleted).expect("plan");
        plan.set_transfer(
            fx.collaborative,
            fx.survivor,
            ArtistDeletionDestination::Existing(fx.destination_album),
        )
        .expect("transfer");
        plan.set_delete(fx.non_collaborative, true).expect("delete");
        plan.set_remove_credit(fx.credited_only, None)
            .expect("credit");
        assert!(plan.is_ready(), "{:?}", plan.issues);
        plan
    }

    fn deletion_committed(point: TransactionFailPoint) -> bool {
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
    fn multi_item_deletion_failure_matrix_is_all_or_nothing() {
        let points = [
            TransactionFailPoint::BeforeJournal,
            TransactionFailPoint::AfterJournal,
            TransactionFailPoint::BeforeStaging,
            TransactionFailPoint::DuringCopy,
            TransactionFailPoint::DuringAudioCopy,
            TransactionFailPoint::AfterAudioCopyBeforeVerify,
            TransactionFailPoint::AfterCopyBeforeVerify,
            TransactionFailPoint::AfterVerify,
            TransactionFailPoint::BeforeMetadataPublish,
            TransactionFailPoint::DuringMetadataPublish,
            TransactionFailPoint::BeforeCatalogPublish,
            TransactionFailPoint::AfterCatalogPublish,
            TransactionFailPoint::BeforeCleanup,
            TransactionFailPoint::AfterCleanup,
            TransactionFailPoint::BeforeJournalFinalize,
        ];
        for point in points {
            let mut fx = fixture(&format!("delete-matrix-{point:?}"));
            let plan = prepared_plan(&mut fx);
            fx.library.set_transaction_failure(Some(TransactionFailure {
                point,
                interrupt: true,
                abort: false,
            }));
            assert!(
                fx.library.execute_artist_deletion(&plan).is_err(),
                "{point:?} must return the injected error"
            );
            drop(fx.library);
            let reopened = CanonicalLibrary::open(&fx.root)
                .unwrap_or_else(|err| panic!("{point:?} recovery failed: {err}"));
            let committed = deletion_committed(point);
            if committed {
                assert!(
                    reopened.artist(fx.deleted).is_none(),
                    "{point:?} artist removed"
                );
                assert!(
                    reopened.track(fx.non_collaborative).is_none(),
                    "{point:?} deleted track"
                );
                let collab = reopened.track(fx.collaborative).expect("collab");
                assert_eq!(collab.owner_artist_id, fx.survivor, "{point:?} owner");
                assert_eq!(
                    collab.credited_artist_ids,
                    vec![fx.survivor],
                    "{point:?} credits"
                );
                assert_eq!(
                    reopened
                        .track(fx.credited_only)
                        .map(|track| track.credited_artist_ids.clone()),
                    Some(vec![fx.survivor]),
                    "{point:?} credited-only"
                );
            } else {
                assert!(
                    reopened.artist(fx.deleted).is_some(),
                    "{point:?} artist restored"
                );
                assert!(
                    reopened.track(fx.non_collaborative).is_some(),
                    "{point:?} track restored"
                );
                assert_eq!(
                    reopened
                        .track(fx.collaborative)
                        .map(|track| track.owner_artist_id),
                    Some(fx.deleted),
                    "{point:?} old owner"
                );
                assert_eq!(
                    reopened
                        .track(fx.credited_only)
                        .map(|track| track.credited_artist_ids.clone()),
                    Some(vec![fx.survivor, fx.deleted]),
                    "{point:?} old credits"
                );
            }
            assert_eq!(
                reopened
                    .playlist(fx.playlist)
                    .map(|playlist| playlist.track_ids.clone()),
                if committed {
                    Some(vec![fx.collaborative, fx.credited_only])
                } else {
                    Some(vec![
                        fx.collaborative,
                        fx.non_collaborative,
                        fx.credited_only,
                    ])
                },
                "{point:?} playlist"
            );
            assert_eq!(
                reopened.rating(fx.collaborative),
                Some(TrackRating {
                    rating: Some(5),
                    favorite: true
                }),
                "{point:?} collaborative rating"
            );
            assert_eq!(
                reopened.rating(fx.non_collaborative),
                if committed {
                    None
                } else {
                    Some(TrackRating {
                        rating: Some(2),
                        favorite: false,
                    })
                },
                "{point:?} deleted rating"
            );
            drop(reopened);
            assert!(
                CanonicalLibrary::open(&fx.root).is_ok(),
                "{point:?} idempotent"
            );
        }
    }

    #[test]
    fn artist_deletion_transfers_retains_and_deletes_in_one_commit() {
        let mut fx = fixture("artist-delete");
        let artist_image_source = fx._dir.write("artist.png", b"\x89PNG\r\n\x1a\nartist");
        fx.library
            .set_artist_image(fx.deleted, &artist_image_source, "png")
            .expect("artist image");
        let managed_artist_image = fx
            .library
            .artist_image_path(fx.deleted)
            .expect("managed artist image");
        let track_art_source = fx._dir.write("track.png", b"\x89PNG\r\n\x1a\ntrack");
        fx.library
            .set_track_artwork(fx.non_collaborative, &track_art_source, "png")
            .expect("track artwork");
        let managed_track_art = fx
            .library
            .track_artwork_path(fx.non_collaborative)
            .expect("managed track artwork");
        let plan = fx.library.plan_artist_deletion(fx.deleted).expect("plan");
        assert!(!plan.is_ready());
        let mut plan = plan;
        plan.set_transfer(
            fx.collaborative,
            fx.survivor,
            ArtistDeletionDestination::Existing(fx.destination_album),
        )
        .expect("transfer decision");
        plan.set_delete(fx.non_collaborative, true)
            .expect("delete decision");
        plan.set_remove_credit(fx.credited_only, None)
            .expect("credit decision");
        assert!(plan.is_ready(), "issues: {:?}", plan.issues);
        let old_collab_audio = fx
            .library
            .track_audio_path(fx.collaborative)
            .expect("old audio");
        let old_solo_audio = fx
            .library
            .track_audio_path(fx.non_collaborative)
            .expect("solo audio");
        let old_guest_audio = fx
            .library
            .track_audio_path(fx.credited_only)
            .expect("guest audio");
        let report = fx
            .library
            .execute_artist_deletion(&plan)
            .expect("delete artist");
        assert_eq!(report.artist_id, fx.deleted);
        assert_eq!(report.transferred, 1);
        assert_eq!(report.credits_removed, 1);
        assert_eq!(report.deleted_tracks, vec![fx.non_collaborative]);
        assert_eq!(report.deleted_albums, vec![fx.source_album]);
        assert!(fx.library.artist(fx.deleted).is_none());
        assert!(!old_collab_audio.exists());
        assert!(!old_solo_audio.exists());
        assert!(!managed_artist_image.exists(), "artist image cleanup");
        assert!(!managed_track_art.exists(), "deleted track artwork cleanup");
        assert!(
            old_guest_audio.is_file(),
            "credited-only audio is not moved"
        );
        let collab = fx.library.track(fx.collaborative).expect("collab retained");
        assert_eq!(collab.owner_artist_id, fx.survivor);
        assert_eq!(collab.credited_artist_ids, vec![fx.survivor]);
        assert_eq!(collab.album_id, Some(fx.destination_album));
        assert_eq!(
            fx.library.album_tracks(fx.destination_album),
            vec![fx.credited_only, fx.collaborative]
        );
        assert!(fx.library.track(fx.non_collaborative).is_none());
        let guest = fx.library.track(fx.credited_only).expect("guest retained");
        assert_eq!(guest.credited_artist_ids, vec![fx.survivor]);
        assert_eq!(
            lyrics(&fx.library, fx.collaborative).as_deref(),
            Some("collab lyrics")
        );
        assert!(lyrics(&fx.library, fx.non_collaborative).is_none());
        assert_eq!(
            lyrics(&fx.library, fx.credited_only).as_deref(),
            Some("guest lyrics")
        );
        assert_eq!(
            fx.library.rating(fx.collaborative),
            Some(TrackRating {
                rating: Some(5),
                favorite: true
            })
        );
        assert_eq!(fx.library.rating(fx.non_collaborative), None);
        assert_eq!(
            fx.library
                .playlist(fx.playlist)
                .map(|playlist| playlist.track_ids.clone()),
            Some(vec![fx.collaborative, fx.credited_only])
        );
        assert!(fx.library.album(fx.source_album).is_none());
        drop(fx.library);
        let reopened = CanonicalLibrary::open(&fx.root).expect("reopen after deletion");
        assert!(reopened.artist(fx.deleted).is_none());
        assert_eq!(
            reopened
                .playlist(fx.playlist)
                .map(|playlist| playlist.track_ids.clone()),
            Some(vec![fx.collaborative, fx.credited_only])
        );
    }

    #[test]
    fn art_less_empty_artist_and_owned_cover_are_removed_without_track_loss() {
        let dir = TempDir::new("delete-empty");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("Empty", Vec::new()).expect("artist");
        let album = library
            .create_album(a, "Empty Album", ReleaseType::Album)
            .expect("album");
        let cover = dir.write("cover.png", b"fake png");
        library.set_cover(album, &cover, "png").expect("cover");
        let plan = library.plan_artist_deletion(a).expect("plan");
        assert!(plan.is_ready(), "{:?}", plan.issues);
        let report = library
            .execute_artist_deletion(&plan)
            .expect("delete empty");
        assert_eq!(report.deleted_albums, vec![album]);
        assert!(library.artist(a).is_none());
        assert!(library.album(album).is_none());
        assert!(
            !root
                .join("artists/artist_000001/albums/album_000001/album.flux")
                .exists()
        );
        assert!(
            !root
                .join("artists/artist_000001/albums/album_000001/cover.png")
                .exists()
        );
    }

    #[test]
    fn artist_deletion_can_create_explicit_new_destination_album() {
        let mut fx = fixture("delete-new-album");
        let mut plan = fx.library.plan_artist_deletion(fx.deleted).expect("plan");
        plan.set_transfer(
            fx.collaborative,
            fx.survivor,
            ArtistDeletionDestination::New {
                title: "Fresh Release".to_owned(),
                release_type: ReleaseType::Single,
            },
        )
        .expect("transfer");
        plan.set_delete(fx.non_collaborative, true).expect("delete");
        plan.set_remove_credit(fx.credited_only, None)
            .expect("credit");
        assert!(plan.is_ready(), "{:?}", plan.issues);
        let report = fx.library.execute_artist_deletion(&plan).expect("delete");
        assert_eq!(report.transferred, 1);
        let collab = fx.library.track(fx.collaborative).expect("collab");
        let album = fx
            .library
            .album(collab.album_id.expect("album"))
            .expect("new album");
        assert_eq!(album.title, "Fresh Release");
        assert_eq!(album.release_type, ReleaseType::Single);
        assert_eq!(album.track_order, vec![fx.collaborative]);
        assert_eq!(album.owner_artist_id, fx.survivor);
    }

    #[test]
    fn unresolved_and_stale_plans_are_rejected_without_changes() {
        let mut fx = fixture("artist-delete-guards");
        let mut plan = fx.library.plan_artist_deletion(fx.deleted).expect("plan");
        assert!(!plan.is_ready());
        assert!(fx.library.execute_artist_deletion(&plan).is_err());
        assert!(fx.library.artist(fx.deleted).is_some());
        plan.set_transfer(
            fx.collaborative,
            fx.survivor,
            ArtistDeletionDestination::Existing(fx.destination_album),
        )
        .expect("transfer");
        plan.set_delete(fx.non_collaborative, true).expect("delete");
        plan.set_remove_credit(fx.credited_only, None)
            .expect("credit");
        assert!(plan.is_ready());
        // Force a relevant catalogue change after planning.
        fx.library
            .set_rating(fx.collaborative, Some(1), false)
            .expect("rating");
        assert!(fx.library.execute_artist_deletion(&plan).is_err());
        assert!(fx.library.artist(fx.deleted).is_some());
        assert!(fx.library.track(fx.collaborative).is_some());
    }
}

#[cfg(test)]
mod deletion_process_tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use super::*;
    use crate::test_support::TempDir;

    const ROOT: &str = "FLUX_TEST_DELETE_ROOT";
    const POINT: &str = "FLUX_TEST_DELETE_POINT";

    fn point(name: &str) -> TransactionFailPoint {
        match name {
            "after-journal" => TransactionFailPoint::AfterJournal,
            "during-metadata-publish" => TransactionFailPoint::DuringMetadataPublish,
            "after-catalog-publish" => TransactionFailPoint::AfterCatalogPublish,
            "during-cleanup" => TransactionFailPoint::DuringCleanup,
            other => panic!("unknown deletion boundary {other}"),
        }
    }

    #[test]
    fn child_delete_helper() {
        let Ok(root) = std::env::var(ROOT) else {
            return;
        };
        let boundary = point(&std::env::var(POINT).expect("deletion boundary"));
        let root = PathBuf::from(root);
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source_dir = root.parent().expect("parent").join("delete-source");
        fs::create_dir_all(&source_dir).expect("source");
        let mut import =
            |name: &str, bytes: &[u8], credits: Vec<ArtistId>, owner: ArtistId, album: &str| {
                let path = source_dir.join(name);
                fs::write(&path, bytes).expect("audio");
                library
                    .import_track(CanonicalImportedTrack {
                        source: path,
                        title: name.to_owned(),
                        artist_id: Some(owner),
                        artist_name: if owner == a { "A" } else { "B" }.to_owned(),
                        credited_artist_ids: credits,
                        album_title: Some(album.to_owned()),
                        release_type: ReleaseType::Album,
                        track_number: None,
                        disc_number: None,
                        duration_ms: None,
                    })
                    .expect("import")
            };
        let collab = import(
            "collab.wav",
            b"delete child collab",
            vec![a, b],
            a,
            "A Album",
        );
        let solo = import("solo.wav", b"delete child solo", vec![a], a, "A Album");
        let guest = import("guest.wav", b"delete child guest", vec![b, a], b, "B Album");
        let destination = library.track(guest).expect("guest").album_id;
        library
            .set_playlist(None, "P", vec![collab, solo, guest])
            .expect("playlist");
        library.set_rating(collab, Some(4), true).expect("rating");
        let mut plan = library.plan_artist_deletion(a).expect("plan");
        plan.set_transfer(
            collab,
            b,
            ArtistDeletionDestination::Existing(destination.expect("album")),
        )
        .expect("transfer");
        plan.set_delete(solo, true).expect("delete");
        plan.set_remove_credit(guest, None).expect("credit");
        library.set_transaction_failure(Some(TransactionFailure {
            point: boundary,
            interrupt: true,
            abort: true,
        }));
        let _ = library.execute_artist_deletion(&plan);
        panic!("delete child did not terminate");
    }

    fn spawn(boundary: &str, root: &Path) -> std::process::ExitStatus {
        Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "canonical::deletion_process_tests::child_delete_helper",
                "--nocapture",
            ])
            .env(ROOT, root)
            .env(POINT, boundary)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn deletion child")
    }

    #[test]
    fn real_child_termination_during_artist_deletion_recovers() {
        for (boundary, committed) in [
            ("after-journal", false),
            ("during-metadata-publish", false),
            ("after-catalog-publish", true),
            ("during-cleanup", true),
        ] {
            let dir = TempDir::new(&format!("delete-child-{boundary}"));
            let root = dir.path().join("FluxLibrary");
            let status = spawn(boundary, &root);
            assert!(
                !status.success(),
                "{boundary} child must abort, got {status:?}"
            );
            let reopened = CanonicalLibrary::open(&root).expect("recover deletion");
            if committed {
                assert!(reopened.artist(ArtistId::new(1)).is_none());
                assert!(reopened.track(TrackId::new(2)).is_none());
                assert_eq!(
                    reopened
                        .track(TrackId::new(1))
                        .map(|track| track.owner_artist_id),
                    Some(ArtistId::new(2))
                );
                assert_eq!(
                    reopened
                        .track(TrackId::new(3))
                        .map(|track| track.credited_artist_ids.clone()),
                    Some(vec![ArtistId::new(2)])
                );
                assert_eq!(
                    fs::read(reopened.track_audio_path(TrackId::new(1)).expect("audio"))
                        .expect("audio"),
                    b"delete child collab"
                );
            } else {
                assert!(reopened.artist(ArtistId::new(1)).is_some());
                assert!(reopened.track(TrackId::new(2)).is_some());
                assert_eq!(
                    reopened
                        .track(TrackId::new(1))
                        .map(|track| track.owner_artist_id),
                    Some(ArtistId::new(1))
                );
                assert_eq!(
                    reopened
                        .track(TrackId::new(3))
                        .map(|track| track.credited_artist_ids.clone()),
                    Some(vec![ArtistId::new(2), ArtistId::new(1)])
                );
            }
            assert_eq!(
                reopened.rating(TrackId::new(1)),
                Some(TrackRating {
                    rating: Some(4),
                    favorite: true
                })
            );
            assert_eq!(
                reopened
                    .playlists()
                    .next()
                    .map(|playlist| playlist.track_ids.clone()),
                if committed {
                    Some(vec![TrackId::new(1), TrackId::new(3)])
                } else {
                    Some(vec![TrackId::new(1), TrackId::new(2), TrackId::new(3)])
                }
            );
            drop(reopened);
            assert!(CanonicalLibrary::open(&root).is_ok());
        }
    }
}

// ------------------------------------------------ album ownership ---

/// One track in an immutable whole-album transfer plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTransferTrack {
    /// Stable track identity.
    pub track_id: TrackId,
    /// Track title.
    pub title: String,
    /// Current physical owner (source album owner).
    pub owner_artist_id: ArtistId,
    /// Ordered credits at plan time.
    pub credited_artist_ids: Vec<ArtistId>,
    /// Canonical audio path at plan time.
    pub audio_path: RelPath,
    /// Managed lyrics path, if any.
    pub lyrics_path: Option<RelPath>,
    /// Lyrics content fingerprint at plan time.
    pub lyrics_fingerprint: Option<ContentFingerprint>,
    /// Rating/favorite at plan time.
    pub rating: Option<TrackRating>,
    /// Playlists referencing this track.
    pub playlist_ids: Vec<PlaylistId>,
    /// Whether the destination artist is absent from the credits.
    pub requires_credit: bool,
    /// Explicit review decision allowing the destination credit to be appended.
    pub credit_allowed: bool,
}

/// Immutable, generation-aware whole-album transfer plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTransferPlan {
    /// Album whose ownership changes.
    pub album_id: AlbumId,
    /// Current owner.
    pub from_owner: ArtistId,
    /// Destination owner.
    pub to_owner: ArtistId,
    /// Album title.
    pub title: String,
    /// Release type.
    pub release_type: ReleaseType,
    /// Managed cover path at plan time.
    pub cover: Option<RelPath>,
    /// Cover content fingerprint.
    pub cover_fingerprint: Option<ContentFingerprint>,
    /// Ordered track list.
    pub track_order: Vec<TrackId>,
    /// Per-track snapshots and credit decisions.
    pub tracks: Vec<AlbumTransferTrack>,
    /// Catalogue generation captured by the plan.
    pub generation: u64,
    /// Blocking issues.
    pub issues: Vec<String>,
    /// Resource/ownership issues independent of credit decisions.
    snapshot_issues: Vec<String>,
}

impl AlbumTransferPlan {
    /// Whether the plan contains no blockers and every required credit decision
    /// is resolved.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.issues.is_empty()
    }

    /// First blocking issue.
    #[must_use]
    pub fn blocking_reason(&self) -> Option<&str> {
        self.issues.first().map(String::as_str)
    }

    /// Track IDs that need an explicit destination-credit decision.
    #[must_use]
    pub fn credit_decision_tracks(&self) -> Vec<TrackId> {
        self.tracks
            .iter()
            .filter(|track| track.requires_credit)
            .map(|track| track.track_id)
            .collect()
    }

    /// Allows or rejects appending the destination artist to one track's
    /// credits. The destination artist becomes physical owner regardless; a
    /// rejected track blocks the whole transfer.
    pub fn set_credit_allowed(&mut self, track_id: TrackId, allowed: bool) -> Result<(), String> {
        let track = self
            .tracks
            .iter_mut()
            .find(|track| track.track_id == track_id)
            .ok_or_else(|| format!("track {track_id} is not in this album"))?;
        if !track.requires_credit {
            return Err("the destination artist is already credited".to_owned());
        }
        track.credit_allowed = allowed;
        self.refresh_issues();
        Ok(())
    }

    fn refresh_issues(&mut self) {
        let snapshot_issues = self.snapshot_issues.clone();
        self.issues.clear();
        self.issues.extend(snapshot_issues);
        for track in &self.tracks {
            if track.requires_credit && !track.credit_allowed {
                self.issues.push(format!(
                    "track {} requires explicit destination-credit approval",
                    track.track_id
                ));
            }
        }
    }
}

/// Result of a committed whole-album transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTransferReport {
    /// Stable AlbumId.
    pub album_id: AlbumId,
    /// Previous owner.
    pub from_owner: ArtistId,
    /// New owner.
    pub to_owner: ArtistId,
    /// Number of tracks transferred.
    pub tracks: usize,
    /// New relative album directory.
    pub album_path: RelPath,
}

impl CanonicalLibrary {
    /// Albums physically owned by an artist.
    pub fn albums_owned_by(&self, artist: ArtistId) -> impl Iterator<Item = &CanonicalAlbum> + '_ {
        self.albums
            .values()
            .filter(move |album| album.owner_artist_id == artist)
    }

    /// Albums containing at least one track credited to an artist but owned by
    /// another artist.
    #[must_use]
    pub fn albums_credited_for(&self, artist: ArtistId) -> Vec<AlbumId> {
        self.tracks
            .values()
            .filter(|track| track.credited_artist_ids.contains(&artist))
            .filter_map(|track| track.album_id)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|album_id| {
                self.albums
                    .get(album_id)
                    .is_some_and(|album| album.owner_artist_id != artist)
            })
            .collect()
    }

    /// Builds a whole-album transfer plan. The destination is a new location
    /// under the destination artist with the same AlbumId; merging into a
    /// different existing AlbumId is intentionally rejected by design.
    pub fn plan_album_transfer(
        &self,
        album_id: AlbumId,
        to_owner: ArtistId,
    ) -> Result<AlbumTransferPlan, CanonicalError> {
        let album = self.albums.get(&album_id).ok_or_else(|| {
            CanonicalError::Invalid(format!("album {album_id} is not in the library"))
        })?;
        if !self.artists.contains_key(&to_owner) {
            return Err(CanonicalError::Invalid(format!(
                "destination artist {to_owner} is not in the library"
            )));
        }
        if album.owner_artist_id == to_owner {
            return Err(CanonicalError::Invalid(
                "album already belongs to the destination artist".to_owned(),
            ));
        }
        let from_owner = album.owner_artist_id;
        let old_album_rel = album_rel(from_owner, album_id);
        let new_album_rel = album_rel(to_owner, album_id);
        let cover = album.cover.as_ref().map(|cover| {
            RelPath::from_utf8(&format!("{}/{}", old_album_rel.display_lossy(), cover))
                .expect("cover path is valid")
        });
        let mut issues = Vec::new();
        let cover_fingerprint = match cover.as_ref() {
            Some(path) => match safe_join(&self.root, path)
                .and_then(|path| ContentFingerprint::of_file(&path))
            {
                Ok(fingerprint) => Some(fingerprint),
                Err(_) => {
                    issues.push("managed cover is missing or unreadable".to_owned());
                    None
                }
            },
            None => None,
        };
        if safe_join(&self.root, &new_album_rel)?.exists() {
            issues.push(format!(
                "destination album directory already exists: {}",
                new_album_rel.display_lossy()
            ));
        }
        let mut tracks = Vec::new();
        for track_id in &album.track_order {
            let track = self.tracks.get(track_id).ok_or_else(|| {
                CanonicalError::Invalid(format!("album references missing track {track_id}"))
            })?;
            if track.album_id != Some(album_id) || track.owner_artist_id != from_owner {
                issues.push(format!(
                    "track {track_id} ownership/album is inconsistent with the source album"
                ));
            }
            let audio_path = self.track_paths[track_id].clone();
            let record_path = self.track_records[track_id].clone();
            let lyrics_path = track
                .lyrics
                .as_ref()
                .and_then(|lyrics| plan_track_lyrics_rel(&record_path, Some(lyrics)));
            let lyrics_fingerprint = lyrics_path.as_ref().and_then(|path| {
                safe_join(&self.root, path)
                    .ok()
                    .and_then(|path| ContentFingerprint::of_file(&path).ok())
            });
            if !self.track_available(*track_id) {
                issues.push(format!("track {track_id} audio is missing"));
            }
            if track.lyrics.is_some() && lyrics_fingerprint.is_none() {
                issues.push(format!("track {track_id} lyrics are missing"));
            }
            let extension = audio_path
                .display_lossy()
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_owned())
                .unwrap_or_default();
            let new_audio = track_audio_rel(to_owner, Some(album_id), *track_id, &extension);
            let new_record = track_record_rel(to_owner, Some(album_id), *track_id);
            if safe_join(&self.root, &new_audio)?.exists()
                || safe_join(&self.root, &new_record)?.exists()
                || lyrics_path.as_ref().is_some_and(|old| {
                    let new_lyrics = plan_track_lyrics_rel(
                        &new_record,
                        Some(
                            old.display_lossy()
                                .rsplit_once('/')
                                .map_or("", |(_, name)| name),
                        ),
                    );
                    new_lyrics
                        .is_some_and(|new| safe_join(&self.root, &new).is_ok_and(|p| p.exists()))
                })
            {
                issues.push(format!("track {track_id} destination path collision"));
            }
            let requires_credit = !track.credited_artist_ids.contains(&to_owner);
            tracks.push(AlbumTransferTrack {
                track_id: *track_id,
                title: track.title.clone(),
                owner_artist_id: track.owner_artist_id,
                credited_artist_ids: track.credited_artist_ids.clone(),
                audio_path,
                lyrics_path,
                lyrics_fingerprint,
                rating: self.rating(*track_id),
                playlist_ids: self
                    .playlists
                    .values()
                    .filter(|playlist| playlist.track_ids.contains(track_id))
                    .map(|playlist| playlist.id)
                    .collect(),
                requires_credit,
                credit_allowed: false,
            });
        }
        let mut plan = AlbumTransferPlan {
            album_id,
            from_owner,
            to_owner,
            title: album.title.clone(),
            release_type: album.release_type,
            cover,
            cover_fingerprint,
            track_order: album.track_order.clone(),
            tracks,
            generation: self.catalog.generation,
            issues: Vec::new(),
            snapshot_issues: issues,
        };
        plan.refresh_issues();
        Ok(plan)
    }

    /// Executes an album transfer as one catalogue transaction. The plan must
    /// still match the catalogue generation and all track/cover/lyrics
    /// snapshots. The destination album keeps the same AlbumId and every track
    /// keeps its TrackId and order.
    pub fn execute_album_transfer(
        &mut self,
        plan: &AlbumTransferPlan,
    ) -> Result<AlbumTransferReport, CanonicalError> {
        self.ensure_writable()?;
        self.ensure_no_pending_transactions()?;
        if plan.generation != self.catalog.generation {
            return Err(CanonicalError::Invalid(
                "album transfer plan is stale: catalogue generation changed; re-plan".to_owned(),
            ));
        }
        if !plan.is_ready() {
            return Err(CanonicalError::Invalid(format!(
                "album transfer is blocked: {}",
                plan.blocking_reason().unwrap_or("unknown issue")
            )));
        }
        let album = self.albums.get(&plan.album_id).cloned().ok_or_else(|| {
            CanonicalError::Invalid("album disappeared since transfer preview".to_owned())
        })?;
        if album.owner_artist_id != plan.from_owner
            || album.title != plan.title
            || album.release_type != plan.release_type
            || album.track_order != plan.track_order
            || album_cover_rel(&album)? != plan.cover
        {
            return Err(CanonicalError::Invalid(
                "album metadata changed since transfer preview; re-plan".to_owned(),
            ));
        }
        if !self.artists.contains_key(&plan.to_owner) {
            return Err(CanonicalError::Invalid(
                "destination artist disappeared; re-plan".to_owned(),
            ));
        }
        if plan.tracks.len() != plan.track_order.len() {
            return Err(CanonicalError::Invalid(
                "album transfer plan is missing tracks".to_owned(),
            ));
        }
        let new_album_rel = album_rel(plan.to_owner, plan.album_id);
        if safe_join(&self.root, &new_album_rel)?.exists() {
            return Err(CanonicalError::Invalid(
                "destination album directory already exists; re-plan".to_owned(),
            ));
        }
        let old_album_rel = album_rel(plan.from_owner, plan.album_id);
        let cover = plan.cover.as_ref().map(|old| {
            let name = old
                .display_lossy()
                .rsplit_once('/')
                .map_or("", |(_, name)| name)
                .to_owned();
            RelPath::from_utf8(&format!("{}/{}", new_album_rel.display_lossy(), name))
                .expect("new cover path is valid")
        });
        let mut writes = Vec::new();
        let mut cleanup = vec![RelPath::from_utf8(&format!(
            "{}/album.flux",
            old_album_rel.display_lossy()
        ))?];
        if let Some(old_cover) = &plan.cover {
            cleanup.push(old_cover.clone());
        }
        let mut catalog = self.catalog.clone();
        let album_dto = AlbumEntityDto {
            schema_version: CANONICAL_SCHEMA_VERSION,
            kind: ALBUM_KIND.to_owned(),
            id: album.id.get(),
            owner_artist_id: plan.to_owner.get(),
            title: album.title,
            release_type: album.release_type,
            cover: album.cover.clone(),
            track_order: album.track_order.iter().map(|id| id.get()).collect(),
        };
        push_entity_write(&mut writes, &new_album_rel, "album.flux", &album_dto)?;
        if let (Some(old), Some(new), Some(expected)) =
            (&plan.cover, &cover, plan.cover_fingerprint)
        {
            writes.push(PendingWrite {
                target: new.clone(),
                bytes: None,
                copy_from: Some(safe_join(&self.root, old)?),
                expected: Some(expected),
            });
        }
        let mut updated_tracks = Vec::new();
        for planned in &plan.tracks {
            let track = self.tracks.get(&planned.track_id).cloned().ok_or_else(|| {
                CanonicalError::Invalid(format!("track {} disappeared", planned.track_id))
            })?;
            if track.owner_artist_id != plan.from_owner
                || track.album_id != Some(plan.album_id)
                || track.credited_artist_ids != planned.credited_artist_ids
                || track.title != planned.title
                || self.track_paths[&track.id] != planned.audio_path
                || self.rating(track.id) != planned.rating
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {} changed since transfer preview; re-plan",
                    planned.track_id
                )));
            }
            let current_lyrics = track.lyrics.as_deref().and_then(|lyrics| {
                plan_track_lyrics_rel(&self.track_records[&track.id], Some(lyrics))
            });
            let current_lyrics_fingerprint = current_lyrics.as_ref().and_then(|path| {
                safe_join(&self.root, path)
                    .ok()
                    .and_then(|path| ContentFingerprint::of_file(&path).ok())
            });
            if current_lyrics != planned.lyrics_path
                || current_lyrics_fingerprint != planned.lyrics_fingerprint
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {} lyrics changed since transfer preview; re-plan",
                    planned.track_id
                )));
            }
            if planned.requires_credit && !planned.credit_allowed {
                return Err(CanonicalError::Invalid(format!(
                    "track {} still requires a destination-credit decision",
                    planned.track_id
                )));
            }
            let mut credits = track.credited_artist_ids.clone();
            if !credits.contains(&plan.to_owner) {
                credits.push(plan.to_owner);
            }
            let old_audio = self.track_paths[&track.id].clone();
            let old_record = self.track_records[&track.id].clone();
            let extension = old_audio
                .display_lossy()
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_owned())
                .ok_or_else(|| CanonicalError::Invalid("audio has no extension".to_owned()))?;
            let new_audio =
                track_audio_rel(plan.to_owner, Some(plan.album_id), track.id, &extension);
            let new_record = track_record_rel(plan.to_owner, Some(plan.album_id), track.id);
            if safe_join(&self.root, &new_audio)?.exists()
                || safe_join(&self.root, &new_record)?.exists()
            {
                return Err(CanonicalError::Invalid(format!(
                    "track {} destination collision",
                    track.id
                )));
            }
            let lyrics = track.lyrics.clone();
            let new_lyrics = lyrics.as_ref().map(|name| {
                RelPath::from_utf8(&format!(
                    "{}/{}",
                    new_record
                        .display_lossy()
                        .rsplit_once('/')
                        .map_or("", |(dir, _)| dir),
                    name
                ))
                .expect("lyrics path is valid")
            });
            let track_dto = TrackEntityDto {
                schema_version: CANONICAL_SCHEMA_VERSION,
                kind: TRACK_KIND.to_owned(),
                id: track.id.get(),
                title: track.title.clone(),
                credited_artist_ids: credits.iter().map(|artist| artist.get()).collect(),
                owner_artist_id: plan.to_owner.get(),
                album_id: Some(plan.album_id.get()),
                track_number: track.track_number,
                disc_number: track.disc_number,
                duration_ms: track.duration_ms,
                lyrics,
                artwork: track.artwork.clone(),
                fingerprint: track.fingerprint,
                created_unix_ms: track.created_unix_ms,
            };
            push_file_write(&mut writes, &new_record, &track_dto)?;
            writes.push(PendingWrite {
                target: new_audio.clone(),
                bytes: None,
                copy_from: Some(safe_join(&self.root, &old_audio)?),
                expected: track
                    .fingerprint
                    .or(Some(ContentFingerprint::of_file(&safe_join(
                        &self.root, &old_audio,
                    )?)?)),
            });
            if let (Some(old), Some(new)) = (current_lyrics, new_lyrics) {
                writes.push(PendingWrite {
                    target: new,
                    bytes: Some(fs::read(safe_join(&self.root, &old)?)?),
                    copy_from: None,
                    expected: None,
                });
            }
            cleanup.extend([old_audio, old_record]);
            if let Some(lyrics) = planned.lyrics_path.clone() {
                cleanup.push(lyrics);
            }
            let descriptor = catalog
                .tracks
                .iter_mut()
                .find(|entry| entry.id == track.id.get())
                .ok_or_else(|| CanonicalError::Invalid("catalogue track missing".to_owned()))?;
            descriptor.owner_artist_id = plan.to_owner.get();
            descriptor.audio_path = new_audio.display_lossy();
            descriptor.record_path = new_record.display_lossy();
            updated_tracks.push(track.id);
        }
        if updated_tracks != plan.track_order {
            return Err(CanonicalError::Invalid(
                "album transfer plan track order is inconsistent".to_owned(),
            ));
        }
        let album_entry = catalog
            .albums
            .iter_mut()
            .find(|entry| entry.id == plan.album_id.get())
            .ok_or_else(|| CanonicalError::Invalid("catalogue album missing".to_owned()))?;
        album_entry.artist_id = plan.to_owner.get();
        album_entry.path = new_album_rel.display_lossy();
        self.commit_writes_with_cleanup(Some(catalog), writes, cleanup)?;
        self.reopen_after()?;
        Ok(AlbumTransferReport {
            album_id: plan.album_id,
            from_owner: plan.from_owner,
            to_owner: plan.to_owner,
            tracks: plan.tracks.len(),
            album_path: new_album_rel,
        })
    }
}

#[cfg(test)]
mod album_transfer_tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    struct Fixture {
        _dir: TempDir,
        root: std::path::PathBuf,
        library: CanonicalLibrary,
        a: ArtistId,
        b: ArtistId,
        album: AlbumId,
        first: TrackId,
        second: TrackId,
        playlist: PlaylistId,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = TempDir::new(name);
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let source = dir.write("source/first.wav", b"album first");
        let first = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "First".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a, b],
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: Some(1),
                duration_ms: None,
            })
            .expect("first");
        let source = dir.write("source/second.wav", b"album second");
        let second = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Second".to_owned(),
                artist_id: Some(a),
                artist_name: "A".to_owned(),
                credited_artist_ids: vec![a],
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(2),
                disc_number: Some(1),
                duration_ms: None,
            })
            .expect("second");
        let album = library
            .track(first)
            .expect("first")
            .album_id
            .expect("album");
        library
            .set_lyrics(first, Some("first lyrics"))
            .expect("lyrics");
        library
            .set_lyrics(second, Some("second lyrics"))
            .expect("lyrics");
        library.set_rating(first, Some(5), true).expect("rating");
        let playlist = library
            .set_playlist(None, "Album list", vec![first, second])
            .expect("playlist");
        let cover = dir.write("art/cover.png", b"cover bytes");
        library.set_cover(album, &cover, "png").expect("cover");
        Fixture {
            _dir: dir,
            root,
            library,
            a,
            b,
            album,
            first,
            second,
            playlist,
        }
    }

    #[test]
    fn album_transfer_preserves_ids_order_cover_lyrics_ratings_and_playlist() {
        let mut fx = fixture("album-transfer");
        let mut plan = fx
            .library
            .plan_album_transfer(fx.album, fx.b)
            .expect("plan");
        assert!(!plan.is_ready());
        assert_eq!(plan.credit_decision_tracks(), vec![fx.second]);
        plan.set_credit_allowed(fx.second, true)
            .expect("credit decision");
        assert!(plan.is_ready(), "{:?}", plan.issues);
        let old_album_dir = fx.library.album_dir(fx.album).expect("old album");
        let old_cover = old_album_dir.join("cover.png");
        let old_audio = fx.library.track_audio_path(fx.first).expect("old audio");
        let report = fx.library.execute_album_transfer(&plan).expect("transfer");
        assert_eq!(report.album_id, fx.album);
        assert_eq!(report.from_owner, fx.a);
        assert_eq!(report.to_owner, fx.b);
        assert_eq!(report.tracks, 2);
        let album = fx.library.album(fx.album).expect("album retained");
        assert_eq!(album.owner_artist_id, fx.b);
        assert_eq!(album.track_order, vec![fx.first, fx.second]);
        assert_eq!(album.title, "Album");
        assert_eq!(album.release_type, ReleaseType::Album);
        let new_album_dir = fx.library.album_dir(fx.album).expect("new album");
        assert!(new_album_dir.starts_with(fx.library.artist_dir(fx.b).expect("B dir")));
        assert_eq!(
            fs::read(new_album_dir.join("cover.png")).expect("cover"),
            b"cover bytes"
        );
        assert!(!old_cover.exists());
        assert!(!old_album_dir.join("album.flux").exists());
        assert!(!old_audio.exists());
        let first = fx.library.track(fx.first).expect("first");
        let second = fx.library.track(fx.second).expect("second");
        assert_eq!(first.owner_artist_id, fx.b);
        assert_eq!(second.owner_artist_id, fx.b);
        assert_eq!(first.credited_artist_ids, vec![fx.a, fx.b]);
        assert_eq!(second.credited_artist_ids, vec![fx.a, fx.b]);
        assert_eq!(first.track_number, Some(1));
        assert_eq!(second.track_number, Some(2));
        assert_eq!(
            fs::read(fx.library.track_audio_path(fx.first).expect("first audio")).expect("audio"),
            b"album first"
        );
        assert_eq!(
            fs::read(
                fx.library
                    .track_audio_path(fx.second)
                    .expect("second audio")
            )
            .expect("audio"),
            b"album second"
        );
        assert_eq!(
            fs::read_to_string(fx.library.track_lyrics_path(fx.first).expect("lyrics"))
                .expect("lyrics"),
            "first lyrics"
        );
        assert_eq!(
            fs::read_to_string(fx.library.track_lyrics_path(fx.second).expect("lyrics"))
                .expect("lyrics"),
            "second lyrics"
        );
        assert_eq!(
            fx.library.rating(fx.first),
            Some(TrackRating {
                rating: Some(5),
                favorite: true
            })
        );
        assert_eq!(
            fx.library
                .playlist(fx.playlist)
                .map(|playlist| playlist.track_ids.clone()),
            Some(vec![fx.first, fx.second])
        );
        assert!(fx.library.albums_owned_by(fx.a).next().is_none());
        assert_eq!(
            fx.library
                .albums_owned_by(fx.b)
                .map(|album| album.id)
                .collect::<Vec<_>>(),
            vec![fx.album]
        );
        drop(fx.library);
        let reopened = CanonicalLibrary::open(&fx.root).expect("reopen");
        assert_eq!(
            reopened.album(fx.album).map(|album| album.owner_artist_id),
            Some(fx.b)
        );
        assert_eq!(
            reopened
                .album(fx.album)
                .map(|album| album.track_order.clone()),
            Some(vec![fx.first, fx.second])
        );
    }

    fn album_committed(point: TransactionFailPoint) -> bool {
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
    fn album_transfer_failure_matrix_is_all_or_nothing() {
        let points = [
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
        for point in points {
            let mut fx = fixture(&format!("album-matrix-{point:?}"));
            let old_album_dir = fx.library.album_dir(fx.album).expect("old dir");
            let old_cover = old_album_dir.join("cover.png");
            let old_audio = fx.library.track_audio_path(fx.first).expect("old audio");
            let mut plan = fx
                .library
                .plan_album_transfer(fx.album, fx.b)
                .expect("plan");
            plan.set_credit_allowed(fx.second, true).expect("credit");
            assert!(plan.is_ready());
            fx.library.set_transaction_failure(Some(TransactionFailure {
                point,
                interrupt: true,
                abort: false,
            }));
            assert!(
                fx.library.execute_album_transfer(&plan).is_err(),
                "{point:?}"
            );
            drop(fx.library);
            let reopened = CanonicalLibrary::open(&fx.root)
                .unwrap_or_else(|err| panic!("{point:?} recovery failed: {err}"));
            let committed = album_committed(point);
            assert_eq!(
                reopened.album(fx.album).map(|album| album.owner_artist_id),
                Some(if committed { fx.b } else { fx.a }),
                "{point:?} album owner"
            );
            assert_eq!(
                reopened
                    .album(fx.album)
                    .map(|album| album.track_order.clone()),
                Some(vec![fx.first, fx.second]),
                "{point:?} order"
            );
            let new_album_dir = fx.root.join(
                album_rel(fx.b, fx.album)
                    .to_os_path()
                    .expect("new album path"),
            );
            if committed {
                assert!(!old_cover.exists());
                assert!(!old_audio.exists());
                assert!(new_album_dir.join("cover.png").is_file());
                assert_eq!(
                    fs::read(new_album_dir.join("cover.png")).expect("cover"),
                    b"cover bytes"
                );
                assert_eq!(
                    reopened
                        .track(fx.second)
                        .map(|track| track.credited_artist_ids.clone()),
                    Some(vec![fx.a, fx.b])
                );
                assert_eq!(
                    fs::read_to_string(reopened.track_lyrics_path(fx.first).expect("lyrics"))
                        .expect("lyrics"),
                    "first lyrics"
                );
            } else {
                assert!(old_cover.is_file());
                assert!(old_audio.is_file());
                assert!(!new_album_dir.join("album.flux").exists());
                assert!(!new_album_dir.join("cover.png").exists());
                assert_eq!(
                    reopened
                        .track(fx.second)
                        .map(|track| track.credited_artist_ids.clone()),
                    Some(vec![fx.a])
                );
            }
            assert_eq!(
                reopened.rating(fx.first),
                Some(TrackRating {
                    rating: Some(5),
                    favorite: true
                })
            );
            assert_eq!(
                reopened
                    .playlist(fx.playlist)
                    .map(|playlist| playlist.track_ids.clone()),
                Some(vec![fx.first, fx.second])
            );
            drop(reopened);
            assert!(
                CanonicalLibrary::open(&fx.root).is_ok(),
                "{point:?} idempotent"
            );
        }
    }

    #[test]
    fn unapproved_credit_blocks_album_transfer() {
        let mut fx = fixture("album-blocked-credit");
        let plan = fx
            .library
            .plan_album_transfer(fx.album, fx.b)
            .expect("plan");
        assert!(!plan.is_ready());
        assert!(fx.library.execute_album_transfer(&plan).is_err());
        assert_eq!(
            fx.library
                .album(fx.album)
                .map(|album| album.owner_artist_id),
            Some(fx.a)
        );
        assert_eq!(
            fx.library
                .track(fx.second)
                .map(|track| track.owner_artist_id),
            Some(fx.a)
        );
    }

    #[test]
    fn album_transfer_detects_destination_collision_and_stale_rating() {
        let mut fx = fixture("album-collision");
        let target = album_rel(fx.b, fx.album);
        fs::create_dir_all(safe_join(&fx.root, &target).expect("target")).expect("target dir");
        let plan = fx
            .library
            .plan_album_transfer(fx.album, fx.b)
            .expect("plan");
        assert!(!plan.is_ready(), "existing album directory blocks");
        assert!(fx.library.execute_album_transfer(&plan).is_err());
        let mut plan = {
            fs::remove_dir_all(safe_join(&fx.root, &target).expect("target"))
                .expect("remove target");
            fx.library
                .plan_album_transfer(fx.album, fx.b)
                .expect("plan")
        };
        plan.set_credit_allowed(fx.second, true).expect("credit");
        fx.library
            .set_rating(fx.first, Some(1), false)
            .expect("stale rating");
        assert!(fx.library.execute_album_transfer(&plan).is_err());
        assert_eq!(
            fx.library
                .album(fx.album)
                .map(|album| album.owner_artist_id),
            Some(fx.a)
        );
    }

    #[test]
    fn missing_managed_cover_blocks_album_transfer() {
        let fx = fixture("album-missing-cover");
        let cover = fx
            .library
            .album_dir(fx.album)
            .expect("album dir")
            .join("cover.png");
        fs::remove_file(&cover).expect("remove cover");
        let plan = fx
            .library
            .plan_album_transfer(fx.album, fx.b)
            .expect("plan");
        assert!(!plan.is_ready());
        assert!(
            plan.issues.iter().any(|issue| issue.contains("cover")),
            "{:?}",
            plan.issues
        );
    }

    #[test]
    fn empty_album_transfer_moves_album_and_cover() {
        let dir = TempDir::new("album-empty");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let a = library.create_artist("A", Vec::new()).expect("A");
        let b = library.create_artist("B", Vec::new()).expect("B");
        let album = library
            .create_album(a, "Empty", ReleaseType::Ep)
            .expect("album");
        let cover = dir.write("cover.png", b"empty cover");
        library.set_cover(album, &cover, "png").expect("cover");
        let plan = library.plan_album_transfer(album, b).expect("plan");
        assert!(plan.is_ready(), "{:?}", plan.issues);
        let report = library.execute_album_transfer(&plan).expect("transfer");
        assert_eq!(report.tracks, 0);
        assert_eq!(
            library.album(album).map(|album| album.owner_artist_id),
            Some(b)
        );
        assert_eq!(
            library.album(album).map(|album| album.track_order.clone()),
            Some(Vec::new())
        );
        assert!(
            library
                .album_dir(album)
                .expect("dir")
                .join("cover.png")
                .is_file()
        );
    }

    #[cfg(test)]
    mod album_process_tests {
        use std::fs;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Stdio};

        use super::*;
        use crate::test_support::TempDir;

        const ROOT: &str = "FLUX_TEST_ALBUM_ROOT";
        const POINT: &str = "FLUX_TEST_ALBUM_POINT";

        fn point(name: &str) -> TransactionFailPoint {
            match name {
                "after-journal" => TransactionFailPoint::AfterJournal,
                "after-catalog-publish" => TransactionFailPoint::AfterCatalogPublish,
                "during-cleanup" => TransactionFailPoint::DuringCleanup,
                other => panic!("unknown album boundary {other}"),
            }
        }

        #[test]
        fn child_album_transfer_helper() {
            let Ok(root) = std::env::var(ROOT) else {
                return;
            };
            let boundary = point(&std::env::var(POINT).expect("album boundary"));
            let root = PathBuf::from(root);
            let mut library = CanonicalLibrary::create(&root).expect("create");
            let a = library.create_artist("A", Vec::new()).expect("A");
            let b = library.create_artist("B", Vec::new()).expect("B");
            let source_dir = root.parent().expect("parent").join("album-source");
            fs::create_dir_all(&source_dir).expect("source");
            let mut import = |name: &str, credits: Vec<ArtistId>, number: u32| {
                let path = source_dir.join(name);
                fs::write(&path, format!("album {name}")).expect("audio");
                library
                    .import_track(CanonicalImportedTrack {
                        source: path,
                        title: name.to_owned(),
                        artist_id: Some(a),
                        artist_name: "A".to_owned(),
                        credited_artist_ids: credits,
                        album_title: Some("Album".to_owned()),
                        release_type: ReleaseType::Album,
                        track_number: Some(number),
                        disc_number: Some(1),
                        duration_ms: None,
                    })
                    .expect("import")
            };
            let first = import("first.wav", vec![a, b], 1);
            let second = import("second.wav", vec![a], 2);
            let album = library
                .track(first)
                .expect("track")
                .album_id
                .expect("album");
            library.set_lyrics(first, Some("lyrics")).expect("lyrics");
            library.set_rating(first, Some(5), true).expect("rating");
            let cover = source_dir.join("cover.png");
            fs::write(&cover, b"cover").expect("cover");
            library.set_cover(album, &cover, "png").expect("set cover");
            let mut plan = library.plan_album_transfer(album, b).expect("plan");
            plan.set_credit_allowed(second, true).expect("credit");
            library.set_transaction_failure(Some(TransactionFailure {
                point: boundary,
                interrupt: true,
                abort: true,
            }));
            let _ = library.execute_album_transfer(&plan);
            panic!("album child did not terminate");
        }

        fn spawn(boundary: &str, root: &Path) -> std::process::ExitStatus {
            Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "canonical::album_transfer_tests::album_process_tests::child_album_transfer_helper",
                    "--nocapture",
                ])
                .env(ROOT, root)
                .env(POINT, boundary)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .expect("spawn album child")
        }

        #[test]
        fn real_child_termination_during_album_transfer_recovers() {
            for (boundary, committed) in [
                ("after-journal", false),
                ("after-catalog-publish", true),
                ("during-cleanup", true),
            ] {
                let dir = TempDir::new(&format!("album-child-{boundary}"));
                let root = dir.path().join("FluxLibrary");
                let status = spawn(boundary, &root);
                assert!(
                    !status.success(),
                    "{boundary} child must abort, got {status:?}"
                );
                let reopened = CanonicalLibrary::open(&root).expect("recover album transfer");
                let album = AlbumId::new(1);
                assert_eq!(
                    reopened.album(album).map(|album| album.owner_artist_id),
                    Some(if committed {
                        ArtistId::new(2)
                    } else {
                        ArtistId::new(1)
                    })
                );
                assert_eq!(
                    reopened.album(album).map(|album| album.track_order.clone()),
                    Some(vec![TrackId::new(1), TrackId::new(2)])
                );
                let target_dir = root.join(
                    album_rel(ArtistId::new(2), album)
                        .to_os_path()
                        .expect("path"),
                );
                if committed {
                    assert!(target_dir.join("cover.png").is_file());
                    assert_eq!(
                        reopened
                            .track(TrackId::new(2))
                            .map(|track| track.credited_artist_ids.clone()),
                        Some(vec![ArtistId::new(1), ArtistId::new(2)])
                    );
                } else {
                    assert!(!target_dir.join("cover.png").exists());
                    assert_eq!(
                        reopened
                            .track(TrackId::new(2))
                            .map(|track| track.credited_artist_ids.clone()),
                        Some(vec![ArtistId::new(1)])
                    );
                }
                assert_eq!(
                    reopened.rating(TrackId::new(1)),
                    Some(TrackRating {
                        rating: Some(5),
                        favorite: true
                    })
                );
                drop(reopened);
                assert!(CanonicalLibrary::open(&root).is_ok());
            }
        }
    }
}

#[cfg(test)]
mod album_lifecycle_tests {
    use std::fs;

    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn empty_album_lifecycle_metadata_cover_rename_delete_and_reopen() {
        let dir = TempDir::new("album-lifecycle");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let artist = library.create_artist("Artist", Vec::new()).expect("artist");
        let album = library
            .create_album(artist, "Empty", ReleaseType::Album)
            .expect("album");
        assert_eq!(library.albums_owned_by(artist).count(), 1);
        assert!(library.album_tracks(album).is_empty());
        assert!(
            library
                .create_album(artist, "Empty", ReleaseType::Single)
                .is_err(),
            "duplicate owner/title rejected"
        );
        library
            .update_album_metadata(album, "Renamed", ReleaseType::Ep)
            .expect("metadata");
        assert_eq!(
            library.album(album).map(|album| album.title.as_str()),
            Some("Renamed")
        );
        assert_eq!(
            library.album(album).map(|album| album.release_type),
            Some(ReleaseType::Ep)
        );
        let cover = dir.write("cover.png", b"first cover");
        library.set_cover(album, &cover, "png").expect("cover");
        let old_cover = library.album_dir(album).expect("dir").join("cover.png");
        let replacement = dir.write("replacement.webp", b"replacement cover");
        library
            .set_cover(album, &replacement, "webp")
            .expect("replace");
        assert!(!old_cover.exists(), "old cover cleaned after commit");
        assert_eq!(
            fs::read(library.album_dir(album).expect("dir").join("cover.webp")).expect("cover"),
            b"replacement cover"
        );
        library.remove_cover(album).expect("remove cover");
        assert!(
            library
                .album(album)
                .and_then(|album| album.cover.clone())
                .is_none()
        );
        drop(library);
        let mut reopened = CanonicalLibrary::open(&root).expect("reopen");
        assert_eq!(
            reopened.album(album).map(|album| album.title.as_str()),
            Some("Renamed")
        );
        assert!(reopened.album_tracks(album).is_empty());
        reopened.delete_empty_album(album).expect("delete empty");
        assert!(reopened.album(album).is_none());
        assert!(reopened.artist(artist).is_some());
        drop(reopened);
        assert!(CanonicalLibrary::open(&root).is_ok());
    }

    fn import_into(
        dir: &TempDir,
        owner: ArtistId,
        album: &str,
        name: &str,
        bytes: &[u8],
    ) -> CanonicalImportedTrack {
        let source = dir.write(name, bytes);
        CanonicalImportedTrack {
            source,
            title: name.to_owned(),
            artist_id: Some(owner),
            artist_name: "Artist".to_owned(),
            credited_artist_ids: vec![owner],
            album_title: Some(album.to_owned()),
            release_type: ReleaseType::Album,
            track_number: None,
            disc_number: None,
            duration_ms: None,
        }
    }

    #[test]
    fn batch_import_into_existing_album_preserves_metadata_cover_and_order() {
        let dir = TempDir::new("album-batch-import");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let artist = library.create_artist("Artist", Vec::new()).expect("artist");
        let album = library
            .create_album(artist, "Target", ReleaseType::Album)
            .expect("album");
        let cover = dir.write("cover.png", b"target cover");
        library.set_cover(album, &cover, "png").expect("cover");
        let first = import_into(&dir, artist, "Target", "one.wav", b"one");
        let second = import_into(&dir, artist, "Target", "two.wav", b"two");
        let generation = library.generation();
        let order = library.album_tracks(album);
        let ids = library
            .import_tracks_into_album(
                &CanonicalAlbumImportTarget {
                    album_id: album,
                    generation,
                    owner_artist_id: artist,
                    title: "Target".to_owned(),
                    release_type: ReleaseType::Album,
                    cover: Some("cover.png".to_owned()),
                    track_order: order.clone(),
                },
                vec![first, second],
            )
            .expect("batch import");
        assert_eq!(ids.len(), 2);
        assert_eq!(library.album_tracks(album), ids);
        assert_eq!(
            library.album(album).map(|album| album.title.clone()),
            Some("Target".to_owned())
        );
        assert_eq!(
            library.album(album).and_then(|album| album.cover.clone()),
            Some("cover.png".to_owned())
        );
        assert!(
            library
                .album_dir(album)
                .expect("dir")
                .join("cover.png")
                .is_file()
        );
        assert!(library.track(ids[0]).is_some());
        assert!(library.track(ids[1]).is_some());
        drop(library);
        let reopened = CanonicalLibrary::open(&root).expect("reopen");
        assert_eq!(reopened.album_tracks(album), ids);
        assert_eq!(
            reopened.album(album).and_then(|album| album.cover.clone()),
            Some("cover.png".to_owned())
        );
    }

    #[test]
    fn batch_album_import_rejects_stale_order_and_generation() {
        let dir = TempDir::new("album-batch-stale");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let artist = library.create_artist("Artist", Vec::new()).expect("artist");
        let album = library
            .create_album(artist, "Target", ReleaseType::Album)
            .expect("album");
        let old_generation = library.generation();
        let old_order = library.album_tracks(album);
        let source = import_into(&dir, artist, "Target", "one.wav", b"one");
        assert!(
            library
                .import_tracks_into_album(
                    &CanonicalAlbumImportTarget {
                        album_id: album,
                        generation: old_generation + 1,
                        owner_artist_id: artist,
                        title: "Target".to_owned(),
                        release_type: ReleaseType::Album,
                        cover: None,
                        track_order: old_order.clone(),
                    },
                    vec![source.clone()],
                )
                .is_err()
        );
        assert!(
            library
                .import_tracks_into_album(
                    &CanonicalAlbumImportTarget {
                        album_id: album,
                        generation: old_generation,
                        owner_artist_id: artist,
                        title: "Other Title".to_owned(),
                        release_type: ReleaseType::Album,
                        cover: None,
                        track_order: old_order.clone(),
                    },
                    vec![source],
                )
                .is_err()
        );
    }

    #[test]
    fn batch_album_import_failure_recovers_old_or_new_complete_state() {
        for (point, committed) in [
            (TransactionFailPoint::BeforeCatalogPublish, false),
            (TransactionFailPoint::AfterCatalogPublish, true),
        ] {
            let dir = TempDir::new(&format!("album-import-failure-{point:?}"));
            let root = dir.path().join("FluxLibrary");
            let mut library = CanonicalLibrary::create(&root).expect("create");
            let artist = library.create_artist("Artist", Vec::new()).expect("artist");
            let album = library
                .create_album(artist, "Target", ReleaseType::Album)
                .expect("album");
            let generation = library.generation();
            let order = library.album_tracks(album);
            let first = import_into(&dir, artist, "Target", "one.wav", b"one");
            let second = import_into(&dir, artist, "Target", "two.wav", b"two");
            library.set_transaction_failure(Some(TransactionFailure {
                point,
                interrupt: true,
                abort: false,
            }));
            assert!(
                library
                    .import_tracks_into_album(
                        &CanonicalAlbumImportTarget {
                            album_id: album,
                            generation,
                            owner_artist_id: artist,
                            title: "Target".to_owned(),
                            release_type: ReleaseType::Album,
                            cover: None,
                            track_order: order.clone(),
                        },
                        vec![first, second]
                    )
                    .is_err()
            );
            drop(library);
            let reopened = CanonicalLibrary::open(&root).expect("recover batch import");
            assert_eq!(
                reopened.album_tracks(album).len(),
                if committed { 2 } else { 0 },
                "{point:?}"
            );
            assert_eq!(
                reopened.album(album).map(|album| album.track_order.len()),
                Some(if committed { 2 } else { 0 }),
                "{point:?}"
            );
            drop(reopened);
            assert!(CanonicalLibrary::open(&root).is_ok());
        }
    }

    #[test]
    fn populated_album_cannot_be_deleted_without_track_decisions() {
        let dir = TempDir::new("album-populated-delete");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let artist = library.create_artist("Artist", Vec::new()).expect("artist");
        let source = dir.write("song.wav", b"audio");
        let track = library
            .import_track(CanonicalImportedTrack {
                source,
                title: "Song".to_owned(),
                artist_id: Some(artist),
                artist_name: "Artist".to_owned(),
                credited_artist_ids: Vec::new(),
                album_title: Some("Album".to_owned()),
                release_type: ReleaseType::Album,
                track_number: Some(1),
                disc_number: None,
                duration_ms: None,
            })
            .expect("track");
        let album = library
            .track(track)
            .expect("track")
            .album_id
            .expect("album");
        assert!(library.delete_empty_album(album).is_err());
        assert!(library.album(album).is_some());
        assert!(library.track(track).is_some());
    }

    #[test]
    fn album_reorder_preserves_track_ids_and_persists_order() {
        let dir = TempDir::new("album-reorder");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let artist = library.create_artist("Artist", Vec::new()).expect("artist");
        let mut tracks = Vec::new();
        for (index, title) in ["One", "Two", "Three"].into_iter().enumerate() {
            let source = dir.write(&format!("{index}.wav"), title.as_bytes());
            tracks.push(
                library
                    .import_track(CanonicalImportedTrack {
                        source,
                        title: title.to_owned(),
                        artist_id: Some(artist),
                        artist_name: "Artist".to_owned(),
                        credited_artist_ids: Vec::new(),
                        album_title: Some("Album".to_owned()),
                        release_type: ReleaseType::Album,
                        track_number: Some(index as u32 + 1),
                        disc_number: Some(1),
                        duration_ms: None,
                    })
                    .expect("track"),
            );
        }
        let album = library
            .track(tracks[0])
            .expect("track")
            .album_id
            .expect("album");
        assert_eq!(library.album_tracks(album), tracks);
        library
            .reorder_album_track(album, tracks[2], -2)
            .expect("reorder");
        assert_eq!(
            library.album_tracks(album),
            vec![tracks[2], tracks[0], tracks[1]]
        );
        assert!(
            library
                .set_album_track_order(album, vec![tracks[0], tracks[2]])
                .is_err()
        );
        drop(library);
        let reopened = CanonicalLibrary::open(&root).expect("reopen");
        assert_eq!(
            reopened.album_tracks(album),
            vec![tracks[2], tracks[0], tracks[1]]
        );
        assert_eq!(
            reopened.track(tracks[2]).map(|track| track.id),
            Some(tracks[2])
        );
    }
}

#[cfg(test)]
mod atomic_import_tests {
    use super::*;
    use crate::test_support::TempDir;

    fn request(
        dir: &TempDir,
        name: &str,
        title: &str,
        artist: CanonicalImportArtistRef,
        album: CanonicalImportAlbumRef,
    ) -> CanonicalImportTrackRequest {
        let source = dir.write(name, title.as_bytes());
        CanonicalImportTrackRequest {
            source,
            title: title.to_owned(),
            credited_artists: vec![artist],
            owner_index: 0,
            album,
            track_number: None,
            disc_number: None,
            duration_ms: None,
            artwork: None,
            album_artwork: None,
            lyrics: None,
            allow_duplicate_content: false,
            expected_source_fingerprint: None,
        }
    }

    #[test]
    fn batch_import_creates_artist_album_and_tracks_together() {
        let dir = TempDir::new("atomic-import");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let generation = library.generation();
        let first = request(
            &dir,
            "one.wav",
            "One",
            CanonicalImportArtistRef::Proposed("New Artist".to_owned()),
            CanonicalImportAlbumRef::ByName {
                title: "New Album".to_owned(),
                release_type: ReleaseType::Album,
            },
        );
        let second = request(
            &dir,
            "two.wav",
            "Two",
            CanonicalImportArtistRef::Proposed("New Artist".to_owned()),
            CanonicalImportAlbumRef::ByName {
                title: "New Album".to_owned(),
                release_type: ReleaseType::Album,
            },
        );
        let sources = [first.source.clone(), second.source.clone()];
        let ids = library
            .import_tracks_batch(generation, vec![first, second])
            .expect("atomic import");
        assert_eq!(ids.len(), 2);
        assert_eq!(library.artists().count(), 1);
        assert_eq!(library.albums().count(), 1);
        assert_eq!(library.tracks().count(), 2);
        let artist = library.artists().next().expect("artist");
        assert_eq!(artist.name, "New Artist");
        let album = library.albums().next().expect("album");
        assert_eq!(album.title, "New Album");
        assert_eq!(album.track_order, ids);
        for id in ids {
            let track = library.track(id).expect("track");
            assert_eq!(track.owner_artist_id, artist.id);
            assert_eq!(track.credited_artist_ids, vec![artist.id]);
            assert_eq!(track.album_id, Some(album.id));
        }
        drop(library);
        let reopened = CanonicalLibrary::open(&root).expect("reopen");
        assert_eq!(reopened.artists().count(), 1);
        assert_eq!(reopened.albums().count(), 1);
        assert_eq!(reopened.tracks().count(), 2);
        for source in sources {
            assert!(source.is_file(), "source is never deleted");
        }
    }

    #[test]
    fn failed_batch_import_never_leaves_proposed_artist_or_album() {
        let points = [
            TransactionFailPoint::BeforeJournal,
            TransactionFailPoint::AfterJournal,
            TransactionFailPoint::BeforeStaging,
            TransactionFailPoint::DuringCopy,
            TransactionFailPoint::DuringAudioCopy,
            TransactionFailPoint::AfterAudioCopyBeforeVerify,
            TransactionFailPoint::AfterCopyBeforeVerify,
            TransactionFailPoint::AfterVerify,
            TransactionFailPoint::BeforeMetadataPublish,
            TransactionFailPoint::DuringMetadataPublish,
            TransactionFailPoint::BeforeCatalogPublish,
            TransactionFailPoint::AfterCatalogPublish,
            TransactionFailPoint::BeforeCleanup,
            TransactionFailPoint::AfterCleanup,
            TransactionFailPoint::BeforeJournalFinalize,
        ];
        for point in points {
            let committed = matches!(
                point,
                TransactionFailPoint::AfterCatalogPublish
                    | TransactionFailPoint::BeforeCleanup
                    | TransactionFailPoint::AfterCleanup
                    | TransactionFailPoint::BeforeJournalFinalize
            );
            let dir = TempDir::new(&format!("atomic-import-failure-{point:?}"));
            let root = dir.path().join("FluxLibrary");
            let mut library = CanonicalLibrary::create(&root).expect("create");
            let generation = library.generation();
            let request = request(
                &dir,
                "one.wav",
                "One",
                CanonicalImportArtistRef::Proposed("Proposed".to_owned()),
                CanonicalImportAlbumRef::ByName {
                    title: "Proposed Album".to_owned(),
                    release_type: ReleaseType::Album,
                },
            );
            let source = request.source.clone();
            library.set_transaction_failure(Some(TransactionFailure {
                point,
                interrupt: true,
                abort: false,
            }));
            assert!(
                library
                    .import_tracks_batch(generation, vec![request])
                    .is_err(),
                "{point:?} did not trigger"
            );
            drop(library);
            let reopened = CanonicalLibrary::open(&root).expect("recover");
            assert_eq!(reopened.artists().count(), if committed { 1 } else { 0 });
            assert_eq!(reopened.albums().count(), if committed { 1 } else { 0 });
            assert_eq!(reopened.tracks().count(), if committed { 1 } else { 0 });
            assert!(source.is_file());
            drop(reopened);
            assert!(CanonicalLibrary::open(&root).is_ok());
        }
    }

    #[test]
    fn fixed_album_batch_can_stage_a_proposed_featured_artist_atomically() {
        let dir = TempDir::new("atomic-feature");
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let owner = library.create_artist("Owner", Vec::new()).expect("owner");
        let album = library
            .create_album(owner, "Target", ReleaseType::Album)
            .expect("album");
        let generation = library.generation();
        let source = dir.write("song.wav", b"audio");
        let request = CanonicalImportTrackRequest {
            source,
            title: "Song".to_owned(),
            credited_artists: vec![
                CanonicalImportArtistRef::Existing(owner),
                CanonicalImportArtistRef::Proposed("Featured".to_owned()),
            ],
            owner_index: 0,
            album: CanonicalImportAlbumRef::Existing(album),
            track_number: Some(1),
            disc_number: None,
            duration_ms: None,
            artwork: None,
            album_artwork: None,
            lyrics: None,
            allow_duplicate_content: false,
            expected_source_fingerprint: None,
        };
        let ids = library
            .import_tracks_batch(generation, vec![request])
            .expect("import");
        let track = library.track(ids[0]).expect("track");
        assert_eq!(track.owner_artist_id, owner);
        assert_eq!(track.credited_artist_ids.len(), 2);
        assert!(library.artists().any(|artist| artist.name == "Featured"));
        assert_eq!(library.album_tracks(album), ids);
    }

    fn base_track_fixture(
        dir: &TempDir,
        name: &str,
    ) -> (CanonicalLibrary, ArtistId, AlbumId, TrackId) {
        let root = dir.path().join("FluxLibrary");
        let mut library = CanonicalLibrary::create(&root).expect("create");
        let owner = library.create_artist("Owner", Vec::new()).expect("owner");
        let album = library
            .create_album(owner, "Album", ReleaseType::Album)
            .expect("album");
        let source = dir.write(name, b"original-audio");
        let generation = library.generation();
        let ids = library
            .import_tracks_batch(
                generation,
                vec![CanonicalImportTrackRequest {
                    source,
                    title: "Original".to_owned(),
                    credited_artists: vec![CanonicalImportArtistRef::Existing(owner)],
                    owner_index: 0,
                    album: CanonicalImportAlbumRef::Existing(album),
                    track_number: Some(1),
                    disc_number: None,
                    duration_ms: Some(1_000),
                    artwork: None,
                    album_artwork: None,
                    lyrics: None,
                    allow_duplicate_content: false,
                    expected_source_fingerprint: None,
                }],
            )
            .expect("base import");
        (library, owner, album, ids[0])
    }

    fn update_request(
        library: &CanonicalLibrary,
        id: TrackId,
        title: &str,
        credits: Vec<CanonicalImportArtistRef>,
    ) -> CanonicalTrackMetadataUpdate {
        let track = library.track(id).expect("track");
        CanonicalTrackMetadataUpdate {
            track_id: id,
            expected_fingerprint: track.fingerprint,
            expected_album_id: track.album_id,
            expected_title: track.title.clone(),
            expected_credited_artist_ids: track.credited_artist_ids.clone(),
            expected_track_number: track.track_number,
            expected_disc_number: track.disc_number,
            expected_duration_ms: track.duration_ms,
            title: title.to_owned(),
            credited_artists: credits,
            track_number: track.track_number,
            disc_number: track.disc_number,
            duration_ms: track.duration_ms,
            artwork: None,
            album_artwork: None,
            lyrics: None,
        }
    }

    #[test]
    fn approved_metadata_update_preserves_track_id_and_stages_new_artist() {
        let dir = TempDir::new("atomic-update");
        let (mut library, owner, album, id) = base_track_fixture(&dir, "base.wav");
        let generation = library.generation();
        let update = update_request(
            &library,
            id,
            "Corrected",
            vec![
                CanonicalImportArtistRef::Existing(owner),
                CanonicalImportArtistRef::Proposed("Guest".to_owned()),
            ],
        );
        library
            .import_review_batch(generation, Vec::new(), vec![update])
            .expect("metadata update");
        let track = library.track(id).expect("same track");
        assert_eq!(track.title, "Corrected");
        assert_eq!(track.album_id, Some(album));
        assert_eq!(track.owner_artist_id, owner);
        assert_eq!(track.credited_artist_ids.len(), 2);
        assert!(library.artists().any(|artist| artist.name == "Guest"));
    }

    #[test]
    fn failed_metadata_update_never_leaves_new_artist_or_partial_metadata() {
        let points = [
            TransactionFailPoint::BeforeJournal,
            TransactionFailPoint::AfterJournal,
            TransactionFailPoint::BeforeStaging,
            TransactionFailPoint::DuringCopy,
            TransactionFailPoint::AfterCopyBeforeVerify,
            TransactionFailPoint::AfterVerify,
            TransactionFailPoint::BeforeMetadataPublish,
            TransactionFailPoint::DuringMetadataPublish,
            TransactionFailPoint::BeforeCatalogPublish,
            TransactionFailPoint::AfterCatalogPublish,
            TransactionFailPoint::BeforeCleanup,
            TransactionFailPoint::AfterCleanup,
            TransactionFailPoint::BeforeJournalFinalize,
        ];
        for point in points {
            let committed = matches!(
                point,
                TransactionFailPoint::AfterCatalogPublish
                    | TransactionFailPoint::BeforeCleanup
                    | TransactionFailPoint::AfterCleanup
                    | TransactionFailPoint::BeforeJournalFinalize
            );
            let dir = TempDir::new(&format!("atomic-update-failure-{point:?}"));
            let (mut library, owner, _album, id) = base_track_fixture(&dir, "base.wav");
            let generation = library.generation();
            let update = update_request(
                &library,
                id,
                "Changed",
                vec![
                    CanonicalImportArtistRef::Existing(owner),
                    CanonicalImportArtistRef::Proposed("Guest".to_owned()),
                ],
            );
            library.set_transaction_failure(Some(TransactionFailure {
                point,
                interrupt: true,
                abort: false,
            }));
            assert!(
                library
                    .import_review_batch(generation, Vec::new(), vec![update])
                    .is_err(),
                "{point:?} did not trigger"
            );
            let root = library.root.clone();
            drop(library);
            let reopened = CanonicalLibrary::open(&root).expect("recover");
            let track = reopened.track(id).expect("track");
            assert_eq!(track.title, if committed { "Changed" } else { "Original" });
            assert_eq!(
                reopened.artists().any(|artist| artist.name == "Guest"),
                committed
            );
            assert_eq!(reopened.artists().count(), if committed { 2 } else { 1 });
        }
    }

    #[test]
    fn stale_metadata_update_is_rejected_without_writes() {
        let dir = TempDir::new("atomic-update-stale");
        let (mut library, owner, _album, id) = base_track_fixture(&dir, "base.wav");
        let generation = library.generation();
        let mut update = update_request(
            &library,
            id,
            "Changed",
            vec![
                CanonicalImportArtistRef::Existing(owner),
                CanonicalImportArtistRef::Proposed("Guest".to_owned()),
            ],
        );
        update.expected_title = "Something else".to_owned();
        assert!(
            library
                .import_review_batch(generation, Vec::new(), vec![update])
                .is_err()
        );
        assert_eq!(library.track(id).expect("track").title, "Original");
        assert_eq!(library.artists().count(), 1);
        assert!(!library.artists().any(|artist| artist.name == "Guest"));
    }

    #[cfg(test)]
    mod track_lifecycle_tests {
        use super::*;
        use crate::test_support::TempDir;

        fn lyrics(library: &CanonicalLibrary, id: TrackId) -> Option<String> {
            library
                .track_lyrics_path(id)
                .and_then(|path| fs::read_to_string(path).ok())
        }

        struct LifecycleFixture {
            _dir: TempDir,
            library: CanonicalLibrary,
            owner: ArtistId,
            source_album: AlbumId,
            destination_album: AlbumId,
            first: TrackId,
            middle: TrackId,
            last: TrackId,
            original_audio: PathBuf,
            original_bytes: Vec<u8>,
        }

        fn fixture(name: &str) -> LifecycleFixture {
            let dir = TempDir::new(name);
            let root = dir.path().join("FluxLibrary");
            let mut library = CanonicalLibrary::create(&root).expect("create");
            let owner = library.create_artist("Owner", Vec::new()).expect("owner");
            let source_album = library
                .create_album(owner, "Source", ReleaseType::Album)
                .expect("source album");
            let destination_album = library
                .create_album(owner, "Destination", ReleaseType::Album)
                .expect("destination album");
            let mut sources = Vec::new();
            for (index, name) in ["one.wav", "two.wav", "three.wav"].iter().enumerate() {
                let source = dir.write(name, format!("audio-{index}").as_bytes());
                sources.push(source);
            }
            let generation = library.generation();
            let ids = library
                .import_tracks_batch(
                    generation,
                    sources
                        .into_iter()
                        .enumerate()
                        .map(|(index, source)| CanonicalImportTrackRequest {
                            source,
                            title: format!("Track {}", index + 1),
                            credited_artists: vec![CanonicalImportArtistRef::Existing(owner)],
                            owner_index: 0,
                            album: CanonicalImportAlbumRef::Existing(source_album),
                            track_number: Some(index as u32 + 1),
                            disc_number: None,
                            duration_ms: Some(1_000),
                            artwork: None,
                            album_artwork: None,
                            lyrics: None,
                            allow_duplicate_content: false,
                            expected_source_fingerprint: None,
                        })
                        .collect(),
                )
                .expect("import tracks");
            let middle = ids[1];
            library
                .set_lyrics(middle, Some("line one"))
                .expect("lyrics");
            library.set_rating(middle, Some(5), true).expect("rating");
            library
                .set_playlist(None, "Keep", vec![middle])
                .expect("playlist");
            let original_audio = library.track_audio_path(middle).expect("audio path");
            let original_bytes = fs::read(&original_audio).expect("audio bytes");
            LifecycleFixture {
                _dir: dir,
                library,
                owner,
                source_album,
                destination_album,
                first: ids[0],
                middle,
                last: ids[2],
                original_audio,
                original_bytes,
            }
        }

        #[test]
        fn metadata_edit_updates_credits_and_album_title_atomically() {
            let mut fx = fixture("lifecycle-metadata");
            let guest = fx
                .library
                .create_artist("Guest", Vec::new())
                .expect("guest");
            let plan = fx
                .library
                .plan_track_metadata_update(fx.middle)
                .expect("plan");
            fx.library
                .execute_track_metadata_update(
                    &plan,
                    "Renamed",
                    vec![fx.owner, guest],
                    Some(9),
                    Some(2),
                    Some("Renamed Album".to_owned()),
                )
                .expect("edit");
            let track = fx.library.track(fx.middle).expect("track");
            assert_eq!(track.title, "Renamed");
            assert_eq!(track.credited_artist_ids, vec![fx.owner, guest]);
            assert_eq!(track.track_number, Some(9));
            assert_eq!(track.disc_number, Some(2));
            assert_eq!(track.album_id, Some(fx.source_album));
            assert_eq!(track.owner_artist_id, fx.owner);
            assert_eq!(lyrics(&fx.library, fx.middle), Some("line one".to_owned()));
            assert_eq!(
                fx.library.rating(fx.middle).map(|r| r.rating),
                Some(Some(5))
            );
            assert!(
                fx.library
                    .playlists()
                    .any(|playlist| playlist.track_ids.contains(&fx.middle))
            );
            assert_eq!(
                fs::read(fx.library.track_audio_path(fx.middle).expect("audio")).expect("bytes"),
                fx.original_bytes
            );
        }

        #[test]
        fn metadata_edit_rejects_missing_owner_and_stale_plan_without_writes() {
            let mut fx = fixture("lifecycle-metadata-stale");
            let guest = fx
                .library
                .create_artist("Guest", Vec::new())
                .expect("guest");
            let plan = fx
                .library
                .plan_track_metadata_update(fx.middle)
                .expect("plan");
            assert!(
                fx.library
                    .execute_track_metadata_update(
                        &plan,
                        "Bad",
                        vec![guest],
                        Some(1),
                        None,
                        Some("Source".to_owned()),
                    )
                    .is_err()
            );
            assert_eq!(fx.library.track(fx.middle).expect("track").title, "Track 2");

            let plan = fx
                .library
                .plan_track_metadata_update(fx.middle)
                .expect("plan");
            fx.library
                .execute_track_metadata_update(
                    &plan,
                    "Changed later",
                    vec![fx.owner],
                    Some(2),
                    None,
                    Some("Source".to_owned()),
                )
                .expect("first edit");
            assert!(
                fx.library
                    .execute_track_metadata_update(
                        &plan,
                        "Stale",
                        vec![fx.owner],
                        Some(2),
                        None,
                        Some("Source".to_owned()),
                    )
                    .is_err()
            );
            assert_eq!(
                fx.library.track(fx.middle).expect("track").title,
                "Changed later"
            );
        }

        #[test]
        fn batch_favorites_and_playlist_append_are_one_transaction() {
            for (point, committed) in [
                (TransactionFailPoint::BeforeCatalogPublish, false),
                (TransactionFailPoint::AfterCatalogPublish, true),
            ] {
                let mut fx = fixture(&format!("lifecycle-batch-favorites-{point:?}"));
                fx.library.set_transaction_failure(Some(TransactionFailure {
                    point,
                    interrupt: true,
                    abort: false,
                }));
                assert!(
                    fx.library
                        .set_ratings(&[(fx.first, None, true), (fx.last, Some(3), true),])
                        .is_err()
                );
                let root = fx.library.root.clone();
                drop(fx.library);
                let reopened = CanonicalLibrary::open(&root).expect("recover");
                assert_eq!(
                    reopened
                        .rating(fx.first)
                        .is_some_and(|rating| rating.favorite),
                    committed,
                    "{point:?}"
                );
                assert_eq!(
                    reopened
                        .rating(fx.last)
                        .map(|rating| (rating.rating, rating.favorite)),
                    if committed {
                        Some((Some(3), true))
                    } else {
                        None
                    },
                    "{point:?}"
                );

                let mut fx = fixture(&format!("lifecycle-batch-playlist-{point:?}"));
                let playlist = fx
                    .library
                    .set_playlist(None, "Batch", vec![fx.middle])
                    .expect("playlist");
                fx.library.set_transaction_failure(Some(TransactionFailure {
                    point,
                    interrupt: true,
                    abort: false,
                }));
                assert!(
                    fx.library
                        .set_playlist(Some(playlist), "Batch", vec![fx.middle, fx.first])
                        .is_err()
                );
                let root = fx.library.root.clone();
                drop(fx.library);
                let reopened = CanonicalLibrary::open(&root).expect("recover");
                assert_eq!(
                    reopened
                        .playlist(playlist)
                        .map(|playlist| playlist.track_ids.clone()),
                    if committed {
                        Some(vec![fx.middle, fx.first])
                    } else {
                        Some(vec![fx.middle])
                    },
                    "{point:?}"
                );
            }
        }

        #[test]
        fn lyrics_replace_and_remove_are_transactional() {
            for (point, committed) in [
                (TransactionFailPoint::BeforeCatalogPublish, false),
                (TransactionFailPoint::AfterCatalogPublish, true),
            ] {
                let mut fx = fixture(&format!("lifecycle-lyrics-{point:?}"));
                let old_path = fx.library.track_lyrics_path(fx.middle).expect("old lyrics");
                fx.library.set_transaction_failure(Some(TransactionFailure {
                    point,
                    interrupt: true,
                    abort: false,
                }));
                assert!(
                    fx.library
                        .set_lyrics(fx.middle, Some("[00:01.000]new lyrics"))
                        .is_err()
                );
                let root = fx.library.root.clone();
                drop(fx.library);
                let reopened = CanonicalLibrary::open(&root).expect("recover");
                assert_eq!(
                    lyrics(&reopened, fx.middle),
                    Some(if committed {
                        "[00:01.000]new lyrics".to_owned()
                    } else {
                        "line one".to_owned()
                    }),
                    "{point:?}"
                );
                assert!(old_path.exists(), "the managed lyrics resource remains");
                drop(reopened);
                assert!(CanonicalLibrary::open(&root).is_ok());
            }

            let mut fx = fixture("lifecycle-lyrics-remove");
            let old_path = fx.library.track_lyrics_path(fx.middle).expect("lyrics");
            fx.library.set_lyrics(fx.middle, None).expect("remove");
            assert!(!old_path.exists());
            assert!(fx.library.track_lyrics_path(fx.middle).is_none());
            assert!(fx.library.track(fx.middle).is_some());
        }

        #[test]
        fn playlist_delete_is_transactional_and_never_deletes_tracks() {
            for (point, committed) in [
                (TransactionFailPoint::BeforeCatalogPublish, false),
                (TransactionFailPoint::AfterCatalogPublish, true),
            ] {
                let mut fx = fixture(&format!("lifecycle-playlist-delete-{point:?}"));
                let playlist = fx
                    .library
                    .set_playlist(None, "Delete me", vec![fx.middle])
                    .expect("playlist");
                let track_before = fx.library.track(fx.middle).expect("track").clone();
                fx.library.set_transaction_failure(Some(TransactionFailure {
                    point,
                    interrupt: true,
                    abort: false,
                }));
                assert!(fx.library.delete_playlist(playlist).is_err());
                let root = fx.library.root.clone();
                drop(fx.library);
                let reopened = CanonicalLibrary::open(&root).expect("recover");
                assert_eq!(
                    reopened.playlist(playlist).is_some(),
                    !committed,
                    "{point:?}"
                );
                assert_eq!(
                    reopened.track(fx.middle).expect("track").title,
                    track_before.title
                );
                assert_eq!(
                    reopened.track_audio_path(fx.middle).expect("audio"),
                    fx.original_audio
                );
                assert!(fx.original_audio.exists());
            }
        }

        #[test]
        fn batch_favorites_reject_missing_track_without_writes() {
            let mut fx = fixture("lifecycle-batch-favorites-invalid");
            let before = fx.library.rating(fx.first);
            assert!(
                fx.library
                    .set_ratings(&[
                        (fx.first, Some(3), true),
                        (TrackId::new(99_999), None, true)
                    ])
                    .is_err()
            );
            assert_eq!(fx.library.rating(fx.first), before);
            assert!(
                !fx.library
                    .rating(fx.first)
                    .is_some_and(|rating| rating.favorite)
            );
        }

        #[test]
        fn move_preserves_identity_references_and_empty_source_album() {
            let mut fx = fixture("lifecycle-move");
            let plan = fx
                .library
                .plan_track_move(fx.middle, Some(fx.destination_album), 0)
                .expect("plan");
            let report = fx.library.execute_track_move(&plan).expect("move");
            assert_eq!(report.track_id, fx.middle);
            assert_eq!(
                fx.library.album_tracks(fx.destination_album),
                vec![fx.middle]
            );
            assert_eq!(
                fx.library.album_tracks(fx.source_album),
                vec![fx.first, fx.last]
            );
            let track = fx.library.track(fx.middle).expect("track");
            assert_eq!(track.album_id, Some(fx.destination_album));
            assert_eq!(track.owner_artist_id, fx.owner);
            assert!(fx.library.album(fx.source_album).is_some());
            assert_eq!(
                fs::read(fx.library.track_audio_path(fx.middle).expect("new audio"))
                    .expect("bytes"),
                fx.original_bytes
            );
            assert!(!fx.original_audio.exists());
            assert_eq!(lyrics(&fx.library, fx.middle), Some("line one".to_owned()));
            assert_eq!(fx.library.rating(fx.middle).map(|r| r.favorite), Some(true));
            assert!(
                fx.library
                    .playlists()
                    .any(|playlist| playlist.track_ids.contains(&fx.middle))
            );
            let root = fx.library.root.clone();
            drop(fx.library);
            let reopened = CanonicalLibrary::open(&root).expect("reopen");
            assert_eq!(reopened.album_tracks(fx.destination_album), vec![fx.middle]);
            assert_eq!(
                reopened.track(fx.middle).expect("track").album_id,
                Some(fx.destination_album)
            );
        }

        #[test]
        fn move_insertion_order_and_empty_source_album_are_deterministic() {
            let mut fx = fixture("lifecycle-move-order");
            let first_plan = fx
                .library
                .plan_track_move(fx.first, Some(fx.destination_album), 0)
                .expect("first plan");
            fx.library
                .execute_track_move(&first_plan)
                .expect("first move");
            let last_plan = fx
                .library
                .plan_track_move(fx.last, Some(fx.destination_album), 0)
                .expect("last plan");
            fx.library
                .execute_track_move(&last_plan)
                .expect("last move");
            assert_eq!(
                fx.library.album_tracks(fx.destination_album),
                vec![fx.last, fx.first]
            );
            let middle_plan = fx
                .library
                .plan_track_move(fx.middle, Some(fx.destination_album), 1)
                .expect("middle plan");
            fx.library
                .execute_track_move(&middle_plan)
                .expect("middle move");
            assert_eq!(
                fx.library.album_tracks(fx.destination_album),
                vec![fx.last, fx.middle, fx.first]
            );
            assert_eq!(
                fx.library.album_tracks(fx.source_album),
                Vec::<TrackId>::new()
            );
            assert!(fx.library.album(fx.source_album).is_some());
            let root = fx.library.root.clone();
            drop(fx.library);
            let reopened = CanonicalLibrary::open(&root).expect("reopen");
            assert_eq!(
                reopened.album_tracks(fx.destination_album),
                vec![fx.last, fx.middle, fx.first]
            );
            assert_eq!(
                reopened.album_tracks(fx.source_album),
                Vec::<TrackId>::new()
            );
        }

        #[test]
        fn move_rejects_foreign_owner_and_stale_order() {
            let mut fx = fixture("lifecycle-move-invalid");
            let other_owner = fx
                .library
                .create_artist("Other", Vec::new())
                .expect("other");
            let foreign = fx
                .library
                .create_album(other_owner, "Foreign", ReleaseType::Album)
                .expect("foreign");
            assert!(
                fx.library
                    .plan_track_move(fx.middle, Some(foreign), 0)
                    .is_err()
            );

            let collision_plan = fx
                .library
                .plan_track_move(fx.middle, Some(fx.destination_album), 0)
                .expect("collision plan");
            let collision_path = fx.library.root.join(
                collision_plan
                    .destination_audio_path
                    .to_os_path()
                    .expect("os"),
            );
            fs::create_dir_all(collision_path.parent().expect("parent")).expect("parent");
            fs::write(&collision_path, b"collision").expect("collision");
            assert!(fx.library.execute_track_move(&collision_plan).is_err());
            fs::remove_file(&collision_path).expect("remove collision");
            assert_eq!(
                fx.library.track(fx.middle).expect("track").album_id,
                Some(fx.source_album)
            );

            let plan = fx
                .library
                .plan_track_move(fx.middle, Some(fx.destination_album), 0)
                .expect("plan");
            let destination_track = fx
                .library
                .plan_track_move(fx.first, Some(fx.destination_album), 0)
                .expect("other plan");
            fx.library
                .execute_track_move(&destination_track)
                .expect("change destination order");
            assert!(fx.library.execute_track_move(&plan).is_err());
            assert_eq!(
                fx.library.track(fx.middle).expect("track").album_id,
                Some(fx.source_album)
            );
        }

        #[test]
        fn replacement_preserves_track_identity_references_and_source_file() {
            let dir = TempDir::new("lifecycle-replacement");
            let fx_dir = dir.path();
            let mut fx = fixture("lifecycle-replacement-inner");
            let replacement = fx_dir.join("replacement.wav");
            fs::write(&replacement, b"replacement-audio").expect("replacement");
            let replacement_before = fs::read(&replacement).expect("replacement bytes");
            let plan = fx
                .library
                .plan_track_audio_replacement(fx.middle, &replacement)
                .expect("plan");
            assert_ne!(plan.destination_audio_path, plan.current_audio_path);
            let report = fx
                .library
                .execute_track_audio_replacement(&plan)
                .expect("replace");
            assert_eq!(report.track_id, fx.middle);
            assert_eq!(fx.library.track(fx.middle).expect("track").title, "Track 2");
            assert_eq!(
                fx.library.track(fx.middle).expect("track").album_id,
                Some(fx.source_album)
            );
            assert_eq!(
                fx.library.track(fx.middle).expect("track").owner_artist_id,
                fx.owner
            );
            assert_eq!(
                fs::read(fx.library.track_audio_path(fx.middle).expect("new audio"))
                    .expect("audio"),
                replacement_before
            );
            assert!(!fx.original_audio.exists());
            assert_eq!(
                fs::read(&replacement).expect("source preserved"),
                replacement_before
            );
            assert_eq!(lyrics(&fx.library, fx.middle), Some("line one".to_owned()));
            assert_eq!(
                fx.library.rating(fx.middle).map(|r| r.rating),
                Some(Some(5))
            );
            assert!(
                fx.library
                    .playlists()
                    .any(|playlist| playlist.track_ids.contains(&fx.middle))
            );
            let root = fx.library.root.clone();
            drop(fx.library);
            let reopened = CanonicalLibrary::open(&root).expect("reopen");
            assert_eq!(
                reopened.track_audio_path(fx.middle).expect("audio"),
                root.join(report.audio_path.to_os_path().expect("os path"))
            );
            assert_eq!(
                reopened.track(fx.middle).expect("track").album_id,
                Some(fx.source_album)
            );
        }

        #[test]
        fn replacement_rejects_stale_source_current_audio_and_collisions() {
            let dir = TempDir::new("lifecycle-replacement-stale");
            let mut fx = fixture("lifecycle-replacement-stale-inner");
            let replacement = dir.path().join("replacement.wav");
            fs::write(&replacement, b"replacement-audio").expect("replacement");
            let plan = fx
                .library
                .plan_track_audio_replacement(fx.middle, &replacement)
                .expect("plan");
            fs::write(&replacement, b"changed-source").expect("change source");
            assert!(fx.library.execute_track_audio_replacement(&plan).is_err());
            assert!(fx.original_audio.exists());
            assert_eq!(
                fs::read(fx.library.track_audio_path(fx.middle).expect("audio")).expect("audio"),
                fx.original_bytes
            );

            fs::write(&replacement, b"replacement-audio").expect("restore source");
            let plan = fx
                .library
                .plan_track_audio_replacement(fx.middle, &replacement)
                .expect("plan");
            fs::write(
                fx.library
                    .root
                    .join(plan.destination_audio_path.to_os_path().expect("os path")),
                b"collision",
            )
            .expect("collision");
            assert!(fx.library.execute_track_audio_replacement(&plan).is_err());
            assert_eq!(
                fs::read(fx.library.track_audio_path(fx.middle).expect("audio")).expect("audio"),
                fx.original_bytes
            );

            let plan = fx
                .library
                .plan_track_audio_replacement(fx.middle, &replacement)
                .expect("plan");
            fs::write(&fx.original_audio, b"external change").expect("change managed audio");
            assert!(fx.library.execute_track_audio_replacement(&plan).is_err());
        }

        fn failure_points() -> [TransactionFailPoint; 15] {
            [
                TransactionFailPoint::BeforeJournal,
                TransactionFailPoint::AfterJournal,
                TransactionFailPoint::BeforeStaging,
                TransactionFailPoint::DuringCopy,
                TransactionFailPoint::DuringAudioCopy,
                TransactionFailPoint::AfterAudioCopyBeforeVerify,
                TransactionFailPoint::AfterCopyBeforeVerify,
                TransactionFailPoint::AfterVerify,
                TransactionFailPoint::BeforeMetadataPublish,
                TransactionFailPoint::DuringMetadataPublish,
                TransactionFailPoint::BeforeCatalogPublish,
                TransactionFailPoint::AfterCatalogPublish,
                TransactionFailPoint::BeforeCleanup,
                TransactionFailPoint::AfterCleanup,
                TransactionFailPoint::BeforeJournalFinalize,
            ]
        }

        fn committed_point(point: TransactionFailPoint) -> bool {
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
        fn move_failure_matrix_recovers_complete_old_or_new_state() {
            let mut points = failure_points().to_vec();
            points.push(TransactionFailPoint::DuringLyricsCopy);
            points.push(TransactionFailPoint::DuringLyricsCleanup);
            for point in points {
                let mut fx = fixture(&format!("lifecycle-move-{point:?}"));
                let plan = fx
                    .library
                    .plan_track_move(fx.middle, Some(fx.destination_album), 0)
                    .expect("plan");
                fx.library.set_transaction_failure(Some(TransactionFailure {
                    point,
                    interrupt: true,
                    abort: false,
                }));
                assert!(
                    fx.library.execute_track_move(&plan).is_err(),
                    "{point:?} did not trigger"
                );
                let root = fx.library.root.clone();
                drop(fx.library);
                let reopened = CanonicalLibrary::open(&root).expect("recover");
                let committed = committed_point(point);
                assert_eq!(
                    reopened.track(fx.middle).expect("track").album_id,
                    Some(if committed {
                        fx.destination_album
                    } else {
                        fx.source_album
                    }),
                    "{point:?} committed={committed}"
                );
                assert_eq!(
                    reopened.album_tracks(fx.destination_album),
                    if committed {
                        vec![fx.middle]
                    } else {
                        Vec::new()
                    }
                );
                assert_eq!(
                    reopened.album_tracks(fx.source_album),
                    if committed {
                        vec![fx.first, fx.last]
                    } else {
                        vec![fx.first, fx.middle, fx.last]
                    }
                );
                assert_eq!(
                    fs::read(reopened.track_audio_path(fx.middle).expect("audio")).expect("audio"),
                    fx.original_bytes
                );
                let old_audio = root.join(plan.audio_path.to_os_path().expect("old"));
                let new_audio = root.join(plan.destination_audio_path.to_os_path().expect("new"));
                assert_eq!(old_audio.exists(), !committed, "{point:?} old audio");
                assert_eq!(new_audio.exists(), committed, "{point:?} new audio");
                assert_eq!(lyrics(&reopened, fx.middle), Some("line one".to_owned()));
                assert!(reopened.album(fx.source_album).is_some());
            }
        }

        #[test]
        fn replacement_failure_matrix_recovers_complete_old_or_new_state() {
            for point in failure_points() {
                let dir = TempDir::new(&format!("lifecycle-replacement-{point:?}"));
                let mut fx = fixture(&format!("lifecycle-replacement-{point:?}-inner"));
                let replacement = dir.path().join("replacement.wav");
                fs::write(&replacement, b"replacement-audio").expect("replacement");
                let plan = fx
                    .library
                    .plan_track_audio_replacement(fx.middle, &replacement)
                    .expect("plan");
                let old_path = plan.current_audio_path.clone();
                let new_path = plan.destination_audio_path.clone();
                fx.library.set_transaction_failure(Some(TransactionFailure {
                    point,
                    interrupt: true,
                    abort: false,
                }));
                assert!(
                    fx.library.execute_track_audio_replacement(&plan).is_err(),
                    "{point:?} did not trigger"
                );
                let root = fx.library.root.clone();
                drop(fx.library);
                let reopened = CanonicalLibrary::open(&root).expect("recover");
                let committed = committed_point(point);
                let audio_path = reopened.track_audio_path(fx.middle).expect("audio");
                assert_eq!(
                    audio_path,
                    root.join(
                        if committed { &new_path } else { &old_path }
                            .to_os_path()
                            .expect("os")
                    )
                );
                assert_eq!(
                    fs::read(&audio_path).expect("audio"),
                    if committed {
                        b"replacement-audio".to_vec()
                    } else {
                        fx.original_bytes.clone()
                    }
                );
                let old_audio = root.join(old_path.to_os_path().expect("old"));
                let new_audio = root.join(new_path.to_os_path().expect("new"));
                assert_eq!(old_audio.exists(), !committed, "{point:?} old audio");
                assert_eq!(new_audio.exists(), committed, "{point:?} new audio");
                assert_eq!(lyrics(&reopened, fx.middle), Some("line one".to_owned()));
                assert_eq!(
                    reopened.track(fx.middle).expect("track").album_id,
                    Some(fx.source_album)
                );
                assert_eq!(
                    fs::read(&replacement).expect("source preserved"),
                    b"replacement-audio"
                );
            }
        }
    }
}
