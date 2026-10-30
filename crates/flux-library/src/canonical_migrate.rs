//! Controlled migration into the canonical self-contained library format.
//!
//! Source libraries are only read. A migration is planned first, built in an
//! isolated sibling staging directory, reopened and validated, and only then
//! activated by a same-filesystem rename. No audio tag is read: metadata comes
//! from the source catalogue's effective Flux metadata, and import inference
//! (when used elsewhere) comes from filenames.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flux_core::{ArtistId, TrackId};

use crate::canonical::{
    AlbumId, CanonicalAlbum, CanonicalArtist, CanonicalError, CanonicalLibrary, CanonicalSeed,
    CanonicalSeedTrack, CanonicalTrack, ContentFingerprint, PlaylistId, ReleaseType, TrackRating,
};
use crate::store::{LegacyPathData, StoreError, load_legacy_index_data};

type AlbumOrderEntry = (Option<u32>, Option<u32>, u64, TrackId);
use crate::portable::{PortableStore, PortableStoreError};

/// Source format understood by the canonical migration planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalSourceFormat {
    /// Current portable schema-v3 catalogue (`.flux/catalog.flux`).
    PortableSchemaV3,
    /// Legacy application-data index, converted through the existing
    /// plan-first portable migration bridge and then into canonical form.
    LegacyIndex,
}

/// One blocker found while planning; migration must not partially activate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalMigrationIssue {
    /// Source track, when the issue is track-specific.
    pub track_id: Option<TrackId>,
    /// Human-readable reason.
    pub message: String,
}

/// Read-only migration preview and the exact canonical seed that would be
/// installed after all issues are resolved.
#[derive(Debug, Clone)]
pub struct CanonicalMigrationPlan {
    source: PathBuf,
    destination: PathBuf,
    format: CanonicalSourceFormat,
    seed: CanonicalSeed,
    issues: Vec<CanonicalMigrationIssue>,
    warnings: Vec<String>,
    estimated_bytes: u64,
}

impl CanonicalMigrationPlan {
    /// Source root/index.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }
    /// Destination canonical root (must not exist yet).
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }
    /// Detected source format.
    #[must_use]
    pub fn format(&self) -> CanonicalSourceFormat {
        self.format
    }
    /// Number of source tracks represented in the seed.
    #[must_use]
    pub fn track_count(&self) -> usize {
        self.seed.tracks.len()
    }
    /// Number of albums that will be created.
    #[must_use]
    pub fn album_count(&self) -> usize {
        self.seed.albums.len()
    }
    /// Number of artist identities preserved/created.
    #[must_use]
    pub fn artist_count(&self) -> usize {
        self.seed.artists.len()
    }
    /// Estimated source audio bytes to copy.
    #[must_use]
    pub fn estimated_bytes(&self) -> u64 {
        self.estimated_bytes
    }
    /// Blocking issues. A non-empty list makes [`execute`](Self::execute) fail.
    #[must_use]
    pub fn issues(&self) -> &[CanonicalMigrationIssue] {
        &self.issues
    }
    /// Non-blocking observations.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
    /// Whether every source track can be migrated without user decisions.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.issues.is_empty()
    }
    /// Executes this plan. The destination must still not exist.
    pub fn execute(&self) -> Result<CanonicalMigrationReport, CanonicalMigrationError> {
        if !self.issues.is_empty() {
            return Err(CanonicalMigrationError::Blocked(self.issues.clone()));
        }
        let parent = self
            .destination
            .parent()
            .ok_or_else(|| CanonicalMigrationError::InvalidDestination(self.destination.clone()))?;
        fs::create_dir_all(parent)?;
        if fs::symlink_metadata(&self.destination).is_ok() {
            return Err(CanonicalMigrationError::DestinationExists(
                self.destination.clone(),
            ));
        }
        let staging = unique_staging_path(&self.destination);
        let library = match CanonicalLibrary::create_from_seed(&staging, &self.seed) {
            Ok(library) => library,
            Err(err) => {
                return Err(CanonicalMigrationError::Build {
                    staging,
                    source: err,
                });
            }
        };
        if library.tracks().count() != self.seed.tracks.len() {
            return Err(CanonicalMigrationError::Verify(format!(
                "catalogue has {} tracks, expected {}",
                library.tracks().count(),
                self.seed.tracks.len()
            )));
        }
        for track in library.tracks() {
            let Some(path) = library.track_audio_path(track.id) else {
                return Err(CanonicalMigrationError::Verify(format!(
                    "track {} has no audio path",
                    track.id
                )));
            };
            if !path.starts_with(library.root()) || !path.is_file() {
                return Err(CanonicalMigrationError::Verify(format!(
                    "track {} does not resolve to a file inside the canonical root",
                    track.id
                )));
            }
            if let Some(fingerprint) = track.fingerprint
                && ContentFingerprint::of_file(&path)? != fingerprint
            {
                return Err(CanonicalMigrationError::Verify(format!(
                    "track {} content verification failed",
                    track.id
                )));
            }
        }
        drop(library);
        match fs::rename(&staging, &self.destination) {
            Ok(()) => {}
            Err(err) => {
                return Err(CanonicalMigrationError::Activate {
                    staging,
                    destination: self.destination.clone(),
                    source: err,
                });
            }
        }
        Ok(CanonicalMigrationReport {
            source: self.source.clone(),
            destination: self.destination.clone(),
            format: self.format,
            tracks: self.seed.tracks.len(),
            artists: self.seed.artists.len(),
            albums: self.seed.albums.len(),
            bytes_copied: self.estimated_bytes,
            warnings: self.warnings.clone(),
        })
    }
}

/// Successful canonical migration summary.
#[derive(Debug, Clone)]
pub struct CanonicalMigrationReport {
    source: PathBuf,
    destination: PathBuf,
    format: CanonicalSourceFormat,
    tracks: usize,
    artists: usize,
    albums: usize,
    bytes_copied: u64,
    warnings: Vec<String>,
}

impl CanonicalMigrationReport {
    /// Read-only source.
    #[must_use]
    pub fn source(&self) -> &Path {
        &self.source
    }
    /// Activated canonical root.
    #[must_use]
    pub fn destination(&self) -> &Path {
        &self.destination
    }
    /// Source format.
    #[must_use]
    pub fn format(&self) -> CanonicalSourceFormat {
        self.format
    }
    /// Tracks migrated.
    #[must_use]
    pub fn tracks(&self) -> usize {
        self.tracks
    }
    /// Artists preserved/created.
    #[must_use]
    pub fn artists(&self) -> usize {
        self.artists
    }
    /// Albums created.
    #[must_use]
    pub fn albums(&self) -> usize {
        self.albums
    }
    /// Source audio bytes copied.
    #[must_use]
    pub fn bytes_copied(&self) -> u64 {
        self.bytes_copied
    }
    /// Non-blocking observations.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Why planning or executing a canonical migration failed.
#[derive(Debug)]
pub enum CanonicalMigrationError {
    /// Source catalogue could not be opened.
    Portable(PortableStoreError),
    /// Canonical staging build/validation failed.
    Canonical(CanonicalError),
    /// Legacy index failed validation.
    Legacy(StoreError),
    /// One or more blocking issues.
    Blocked(Vec<CanonicalMigrationIssue>),
    /// Destination already exists.
    DestinationExists(PathBuf),
    /// Destination has no usable parent.
    InvalidDestination(PathBuf),
    /// The selected source is not a supported legacy/portable library.
    UnsupportedSource(String),
    /// Building staging failed.
    Build {
        /// Isolated staging directory retained for inspection/cleanup.
        staging: PathBuf,
        /// Underlying error.
        source: CanonicalError,
    },
    /// Post-build validation failed.
    Verify(String),
    /// Activation rename failed; staging is retained.
    Activate {
        /// Isolated, validated staging directory.
        staging: PathBuf,
        /// Intended destination.
        destination: PathBuf,
        /// Underlying rename error.
        source: std::io::Error,
    },
    /// Filesystem error.
    Io(std::io::Error),
}

impl fmt::Display for CanonicalMigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Portable(err) => write!(f, "cannot read portable source: {err}"),
            Self::Canonical(err) => write!(f, "canonical storage error: {err}"),
            Self::Legacy(err) => write!(f, "legacy index cannot be used: {err}"),
            Self::Blocked(issues) => {
                write!(f, "migration is blocked by {} issue(s)", issues.len())
            }
            Self::DestinationExists(path) => {
                write!(f, "destination already exists: {}", path.display())
            }
            Self::InvalidDestination(path) => {
                write!(f, "destination has no usable parent: {}", path.display())
            }
            Self::UnsupportedSource(message) => {
                write!(f, "unsupported migration source: {message}")
            }
            Self::Build { staging, source } => write!(
                f,
                "building canonical staging at {} failed: {source}",
                staging.display()
            ),
            Self::Verify(message) => {
                write!(f, "canonical migration verification failed: {message}")
            }
            Self::Activate {
                staging,
                destination,
                source,
            } => write!(
                f,
                "activating {} as {} failed: {source}; validated staging retained",
                staging.display(),
                destination.display()
            ),
            Self::Io(err) => write!(f, "filesystem error: {err}"),
        }
    }
}

impl std::error::Error for CanonicalMigrationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Portable(err) => Some(err),
            Self::Canonical(err) => Some(err),
            Self::Legacy(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}
impl From<std::io::Error> for CanonicalMigrationError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}
impl From<CanonicalError> for CanonicalMigrationError {
    fn from(err: CanonicalError) -> Self {
        Self::Canonical(err)
    }
}
impl From<PortableStoreError> for CanonicalMigrationError {
    fn from(err: PortableStoreError) -> Self {
        Self::Portable(err)
    }
}

/// Plans a migration from a current portable schema-v3 root.
pub fn plan_portable_v3(
    source_root: &Path,
    destination: &Path,
) -> Result<CanonicalMigrationPlan, CanonicalMigrationError> {
    let opened = PortableStore::open(source_root)?;
    let store = opened.store;
    let source_root = store.root().to_path_buf();
    let mut issues = Vec::new();
    let mut warnings = Vec::new();
    let mut seed = CanonicalSeed::default();
    let mut artist_names = BTreeMap::new();
    let mut next_artist = 1u64;
    for artist in store.artists() {
        next_artist = next_artist.max(artist.id().get() + 1);
        seed.artists.push(CanonicalArtist {
            id: artist.id(),
            name: artist.name().to_owned(),
            aliases: artist.aliases().to_vec(),
        });
        artist_names
            .entry(artist.name().to_owned())
            .or_insert(artist.id());
    }
    if opened.recovery.is_some() {
        warnings.push(
            "source portable catalogue was recovered from its backup; migration used that recovered view"
                .to_owned(),
        );
    }

    let mut albums: BTreeMap<(ArtistId, String), AlbumId> = BTreeMap::new();
    let mut album_orders: HashMap<AlbumId, Vec<AlbumOrderEntry>> = HashMap::new();
    let mut next_album = 1u64;
    let mut estimated_bytes = 0u64;

    for stored in store.iter() {
        let effective = store.effective_metadata(stored);
        let owner = match stored.artist_id() {
            Some(id) => id,
            None => {
                let name = effective
                    .artist
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or("Unknown Artist")
                    .to_owned();
                if let Some(id) = artist_names.get(&name) {
                    *id
                } else {
                    let id = ArtistId::new(next_artist);
                    next_artist = next_artist.checked_add(1).ok_or_else(|| {
                        CanonicalMigrationError::Canonical(CanonicalError::Invalid(
                            "artist ID counter overflow".to_owned(),
                        ))
                    })?;
                    seed.artists.push(CanonicalArtist {
                        id,
                        name: name.clone(),
                        aliases: Vec::new(),
                    });
                    artist_names.insert(name, id);
                    id
                }
            }
        };
        let album_title = effective
            .album
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("Unknown Album")
            .to_owned();
        let album_id = *albums
            .entry((owner, album_title.clone()))
            .or_insert_with(|| {
                let id = AlbumId::new(next_album);
                next_album += 1;
                id
            });
        let rel = stored.path();
        let Some(os_rel) = rel.to_os_path() else {
            issues.push(CanonicalMigrationIssue {
                track_id: Some(stored.id()),
                message: "source path is not representable on this platform".to_owned(),
            });
            continue;
        };
        let source_audio = source_root.join(os_rel);
        let extension = source_audio
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_default();
        if extension.is_empty() || !extension.bytes().all(|b| b.is_ascii_alphanumeric()) {
            issues.push(CanonicalMigrationIssue {
                track_id: Some(stored.id()),
                message: "source audio has no safe extension".to_owned(),
            });
            continue;
        }
        match fs::symlink_metadata(&source_audio) {
            Ok(metadata) if metadata.is_file() => {
                if let (Ok(canonical_source), Ok(canonical_root)) = (
                    fs::canonicalize(&source_audio),
                    fs::canonicalize(&source_root),
                ) && !canonical_source.starts_with(&canonical_root)
                {
                    issues.push(CanonicalMigrationIssue {
                        track_id: Some(stored.id()),
                        message: "source audio resolves outside the portable root".to_owned(),
                    });
                    continue;
                }
                estimated_bytes = estimated_bytes.saturating_add(metadata.len());
            }
            Ok(_) => {
                issues.push(CanonicalMigrationIssue {
                    track_id: Some(stored.id()),
                    message: format!(
                        "source audio is not a regular file: {}",
                        source_audio.display()
                    ),
                });
                continue;
            }
            Err(err) => {
                issues.push(CanonicalMigrationIssue {
                    track_id: Some(stored.id()),
                    message: format!(
                        "source audio is missing/unreadable ({}): {err}",
                        source_audio.display()
                    ),
                });
                continue;
            }
        }
        let album_order = album_orders.entry(album_id).or_default();
        album_order.push((
            effective.disc_number,
            effective.track_number,
            stored.id().get(),
            stored.id(),
        ));
        seed.tracks.push(CanonicalSeedTrack {
            track: CanonicalTrack {
                id: stored.id(),
                title: effective.title,
                credited_artist_ids: vec![owner],
                owner_artist_id: owner,
                album_id,
                track_number: effective.track_number,
                disc_number: effective.disc_number,
                duration_ms: effective.duration_ms,
                lyrics: None,
                fingerprint: None,
            },
            source_audio,
            extension,
            lyrics_text: None,
        });
    }

    for ((owner, title), id) in albums {
        let mut order = album_orders.remove(&id).unwrap_or_default();
        order.sort_by_key(|(disc, number, id, _)| (*disc, *number, *id));
        let track_order = order
            .into_iter()
            .map(|(_, _, _, id)| id)
            .collect::<Vec<_>>();
        let release_type = if track_order.len() == 1
            && seed
                .tracks
                .iter()
                .find(|item| item.track.id == track_order[0])
                .is_some_and(|item| item.track.title == title)
        {
            ReleaseType::Single
        } else {
            ReleaseType::Album
        };
        seed.albums.push(CanonicalAlbum {
            id,
            owner_artist_id: owner,
            title,
            release_type,
            cover: None,
            track_order,
        });
    }
    seed.albums.sort_by_key(|album| album.id);
    seed.artists.sort_by_key(|artist| artist.id);
    seed.next_track_id = Some(store.next_track_id());
    seed.next_artist_id = Some(next_artist);
    seed.next_album_id = Some(next_album);
    seed.next_playlist_id = Some(1);

    Ok(CanonicalMigrationPlan {
        source: source_root,
        destination: destination.to_path_buf(),
        format: CanonicalSourceFormat::PortableSchemaV3,
        seed,
        issues,
        warnings,
        estimated_bytes,
    })
}

/// Plans and executes a portable schema-v3 migration.
pub fn migrate_portable_v3(
    source_root: &Path,
    destination: &Path,
) -> Result<CanonicalMigrationReport, CanonicalMigrationError> {
    plan_portable_v3(source_root, destination)?.execute()
}

/// Plans a legacy application-data index directly into the canonical model.
/// Legacy records have no stable artist identity, so exact artist names are
/// materialized as deterministic new stable artists (the same documented
/// mapping used by the existing legacy migration). No audio tag is read.
pub fn plan_legacy_index(
    legacy_index: &Path,
    destination: &Path,
) -> Result<CanonicalMigrationPlan, CanonicalMigrationError> {
    let data = load_legacy_index_data(legacy_index).map_err(CanonicalMigrationError::Legacy)?;
    let mut seed = CanonicalSeed::default();
    let mut issues = Vec::new();
    let warnings = vec![
        "legacy records have no stable artist or album IDs; exact artist names are materialized as new stable artists, and album identities are created per owner/album string"
            .to_owned(),
    ];
    let mut artist_names: BTreeMap<String, ArtistId> = BTreeMap::new();
    let mut next_artist = 1u64;
    let mut albums: BTreeMap<(ArtistId, String), AlbumId> = BTreeMap::new();
    let mut album_orders: HashMap<AlbumId, Vec<AlbumOrderEntry>> = HashMap::new();
    let mut next_album = 1u64;
    let mut seen_paths = std::collections::HashSet::new();
    let mut estimated_bytes = 0u64;

    for record in data.tracks {
        let source_audio = match legacy_path_to_path(&record.path) {
            Some(path) => path,
            None => {
                issues.push(CanonicalMigrationIssue {
                    track_id: Some(TrackId::new(record.id)),
                    message: "legacy path is not representable on this platform".to_owned(),
                });
                continue;
            }
        };
        if !source_audio.is_absolute() {
            issues.push(CanonicalMigrationIssue {
                track_id: Some(TrackId::new(record.id)),
                message: "legacy relative paths are ambiguous; the directory the index was relative to is unknown"
                    .to_owned(),
            });
            continue;
        }
        if !seen_paths.insert(source_audio.clone()) {
            issues.push(CanonicalMigrationIssue {
                track_id: Some(TrackId::new(record.id)),
                message: format!("duplicate legacy path: {}", source_audio.display()),
            });
            continue;
        }
        let metadata = match fs::metadata(&source_audio) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                issues.push(CanonicalMigrationIssue {
                    track_id: Some(TrackId::new(record.id)),
                    message: format!(
                        "legacy path is not a regular file: {}",
                        source_audio.display()
                    ),
                });
                continue;
            }
            Err(err) => {
                issues.push(CanonicalMigrationIssue {
                    track_id: Some(TrackId::new(record.id)),
                    message: format!(
                        "legacy audio is missing/unreadable ({}): {err}",
                        source_audio.display()
                    ),
                });
                continue;
            }
        };
        let extension = source_audio
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .unwrap_or_default();
        if extension.is_empty() || !extension.bytes().all(|b| b.is_ascii_alphanumeric()) {
            issues.push(CanonicalMigrationIssue {
                track_id: Some(TrackId::new(record.id)),
                message: "legacy audio has no safe extension".to_owned(),
            });
            continue;
        }
        estimated_bytes = estimated_bytes.saturating_add(metadata.len());

        let artist_name = record
            .artist
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("Unknown Artist")
            .to_owned();
        let owner = *artist_names.entry(artist_name.clone()).or_insert_with(|| {
            let id = ArtistId::new(next_artist);
            next_artist += 1;
            seed.artists.push(CanonicalArtist {
                id,
                name: artist_name,
                aliases: Vec::new(),
            });
            id
        });
        let album_title = record
            .album
            .as_deref()
            .filter(|title| !title.trim().is_empty())
            .unwrap_or("Unknown Album")
            .to_owned();
        let album_id = *albums.entry((owner, album_title)).or_insert_with(|| {
            let id = AlbumId::new(next_album);
            next_album += 1;
            id
        });
        let track_id = TrackId::new(record.id);
        album_orders.entry(album_id).or_default().push((
            record.disc_number,
            record.track_number,
            record.id,
            track_id,
        ));
        seed.tracks.push(CanonicalSeedTrack {
            track: CanonicalTrack {
                id: track_id,
                title: record.title,
                credited_artist_ids: vec![owner],
                owner_artist_id: owner,
                album_id,
                track_number: record.track_number,
                disc_number: record.disc_number,
                duration_ms: record.duration_ms,
                lyrics: None,
                fingerprint: None,
            },
            source_audio,
            extension,
            lyrics_text: None,
        });
    }

    for ((owner, title), id) in albums {
        let mut order = album_orders.remove(&id).unwrap_or_default();
        order.sort_by_key(|(disc, number, id, _)| (*disc, *number, *id));
        let track_order = order
            .into_iter()
            .map(|(_, _, _, id)| id)
            .collect::<Vec<_>>();
        let release_type = if track_order.len() == 1
            && seed
                .tracks
                .iter()
                .find(|item| item.track.id == track_order[0])
                .is_some_and(|item| item.track.title == title)
        {
            ReleaseType::Single
        } else {
            ReleaseType::Album
        };
        seed.albums.push(CanonicalAlbum {
            id,
            owner_artist_id: owner,
            title,
            release_type,
            cover: None,
            track_order,
        });
    }
    seed.artists.sort_by_key(|artist| artist.id);
    seed.albums.sort_by_key(|album| album.id);
    seed.next_track_id = Some(data.next_id);
    seed.next_artist_id = Some(next_artist);
    seed.next_album_id = Some(next_album);
    seed.next_playlist_id = Some(1);

    Ok(CanonicalMigrationPlan {
        source: legacy_index.to_path_buf(),
        destination: destination.to_path_buf(),
        format: CanonicalSourceFormat::LegacyIndex,
        seed,
        issues,
        warnings,
        estimated_bytes,
    })
}

/// Plans and executes a legacy-index migration.
pub fn migrate_legacy_index_to_canonical(
    legacy_index: &Path,
    destination: &Path,
) -> Result<CanonicalMigrationReport, CanonicalMigrationError> {
    plan_legacy_index(legacy_index, destination)?.execute()
}

fn legacy_path_to_path(path: &LegacyPathData) -> Option<PathBuf> {
    match path {
        LegacyPathData::Utf8(text) => Some(PathBuf::from(text)),
        LegacyPathData::UnixBytes(bytes) => {
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

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos}", std::process::id())
}

fn unique_staging_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "library".to_owned());
    destination.with_file_name(format!(".{name}.flux-migration-{}", unique_suffix()))
}

// Keep these public types referenced in rustdoc and future GUI integration.
const _: fn() = || {
    let _ = PlaylistId::new(1);
    let _ = TrackRating {
        rating: None,
        favorite: false,
    };
};

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use flux_core::{ArtistId, TrackId};

    use super::{
        CanonicalMigrationError, CanonicalSourceFormat, migrate_portable_v3, plan_portable_v3,
    };
    use crate::canonical::{CanonicalLibrary, ReleaseType};
    use crate::portable::{
        Availability, ImportedTrack, MetadataOverrides, PortableStore, RelPath, TagSet,
    };

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-canonical-migrate-{}-{name}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp");
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

    fn portable_fixture(dir: &TempDir) -> (PathBuf, Vec<u8>) {
        let root = dir.path().join("old-portable");
        let mut store = PortableStore::create(&root).expect("create portable");
        let artist = store.create_artist("Legacy Artist").expect("artist");
        let audio = root.join("Legacy Artist/Legacy Album/01 - Song.wav");
        fs::create_dir_all(audio.parent().expect("parent")).expect("dirs");
        fs::write(&audio, b"opaque audio bytes").expect("audio");
        store
            .import_tracks(
                vec![ImportedTrack {
                    id: TrackId::new(1),
                    path: RelPath::from_utf8("Legacy Artist/Legacy Album/01 - Song.wav")
                        .expect("path"),
                    availability: Availability::Present,
                    file: None,
                    tags: TagSet {
                        title: Some("Stored Title".to_owned()),
                        artist: Some("Tag Artist".to_owned()),
                        album: Some("Stored Album".to_owned()),
                        album_artist: Some("Stored Album Artist".to_owned()),
                        track_number: Some(3),
                        disc_number: Some(1),
                        duration_ms: Some(201_000),
                    },
                    overrides: {
                        let mut overrides = MetadataOverrides::new();
                        overrides.set_title("User Title".to_owned());
                        overrides
                    },
                    artist_id: Some(artist),
                    added_unix_ms: 10,
                    updated_unix_ms: 20,
                }],
                2,
            )
            .expect("import");
        store.commit().expect("commit");
        let catalog = root.join(".flux/catalog.flux");
        (root, fs::read(catalog).expect("catalog bytes"))
    }

    #[test]
    fn portable_v3_migrates_into_canonical_layout_and_reopens() {
        let dir = TempDir::new("v3");
        let (source, source_catalog) = portable_fixture(&dir);
        let source_audio = source.join("Legacy Artist/Legacy Album/01 - Song.wav");
        let audio_before = fs::read(&source_audio).expect("audio");
        let destination = dir.path().join("FluxLibrary");
        let plan = plan_portable_v3(&source, &destination).expect("plan");
        assert_eq!(plan.format(), CanonicalSourceFormat::PortableSchemaV3);
        assert!(plan.is_ready(), "issues: {:?}", plan.issues());
        assert_eq!(plan.track_count(), 1);
        assert_eq!(plan.artist_count(), 1);
        assert_eq!(plan.album_count(), 1);
        assert_eq!(plan.estimated_bytes(), audio_before.len() as u64);
        let report = plan.execute().expect("migrate");
        assert_eq!(report.tracks(), 1);
        assert_eq!(report.artists(), 1);
        assert_eq!(report.albums(), 1);
        assert!(!destination.with_extension("missing").exists());

        let library = CanonicalLibrary::open(&destination).expect("open canonical");
        let track = library.track(TrackId::new(1)).expect("track ID preserved");
        assert_eq!(track.title, "User Title");
        assert_eq!(track.owner_artist_id, ArtistId::new(1));
        assert_eq!(track.album_id, crate::canonical::AlbumId::new(1));
        assert_eq!(track.track_number, Some(3));
        assert_eq!(track.disc_number, Some(1));
        assert_eq!(track.duration_ms, Some(201_000));
        let album = library.album(track.album_id).expect("album");
        assert_eq!(album.title, "Stored Album");
        assert_eq!(album.release_type, ReleaseType::Album);
        assert_eq!(album.track_order, vec![TrackId::new(1)]);
        assert_eq!(
            library.artist(ArtistId::new(1)).map(|a| a.name.as_str()),
            Some("Legacy Artist"),
            "stable artist relationship wins over the tag artist"
        );
        let copied = library.track_audio_path(TrackId::new(1)).expect("copied");
        assert!(copied.starts_with(&destination));
        assert_eq!(fs::read(&copied).expect("copied audio"), audio_before);
        let _ = track;

        assert_eq!(
            fs::read(source.join(".flux/catalog.flux")).expect("source catalog"),
            source_catalog,
            "the source catalogue is never modified"
        );
        assert_eq!(
            fs::read(&source_audio).expect("source audio"),
            audio_before,
            "the source audio is never modified"
        );
        drop(library);
        assert!(CanonicalLibrary::open(&destination).is_ok());
    }

    #[test]
    fn missing_source_audio_blocks_activation_and_preserves_source() {
        let dir = TempDir::new("missing");
        let (source, source_catalog) = portable_fixture(&dir);
        let source_audio = source.join("Legacy Artist/Legacy Album/01 - Song.wav");
        fs::remove_file(&source_audio).expect("remove source");
        let destination = dir.path().join("FluxLibrary");
        let plan = plan_portable_v3(&source, &destination).expect("plan");
        assert!(!plan.is_ready());
        assert!(
            plan.issues()
                .iter()
                .any(|issue| issue.track_id == Some(TrackId::new(1)))
        );
        assert!(matches!(
            plan.execute(),
            Err(CanonicalMigrationError::Blocked(_))
        ));
        assert!(!destination.exists());
        assert_eq!(
            fs::read(source.join(".flux/catalog.flux")).expect("source catalog"),
            source_catalog
        );
    }

    #[test]
    fn existing_destination_is_never_overwritten() {
        let dir = TempDir::new("collision");
        let (source, _) = portable_fixture(&dir);
        let destination = dir.path().join("FluxLibrary");
        fs::create_dir_all(&destination).expect("dest");
        fs::write(destination.join("keep.txt"), b"keep").expect("keep");
        let plan = plan_portable_v3(&source, &destination).expect("plan");
        assert!(plan.is_ready());
        assert!(matches!(
            plan.execute(),
            Err(CanonicalMigrationError::DestinationExists(_))
        ));
        assert_eq!(
            fs::read(destination.join("keep.txt")).expect("keep"),
            b"keep"
        );
    }

    #[test]
    fn migration_is_repeatable_only_into_a_fresh_destination() {
        let dir = TempDir::new("repeat");
        let (source, _) = portable_fixture(&dir);
        let destination = dir.path().join("FluxLibrary");
        migrate_portable_v3(&source, &destination).expect("first");
        let second = dir.path().join("FluxLibrary2");
        migrate_portable_v3(&source, &second).expect("repeat into fresh root");
        assert!(matches!(
            migrate_portable_v3(&source, &destination),
            Err(CanonicalMigrationError::DestinationExists(_))
        ));
    }
}

#[cfg(test)]
mod legacy_tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use flux_core::TrackId;

    use super::{CanonicalSourceFormat, migrate_legacy_index_to_canonical};
    use crate::canonical::CanonicalLibrary;
    use crate::store::INDEX_VERSION;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "flux-canonical-legacy-{}-{name}",
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

    #[test]
    fn legacy_index_migrates_to_canonical_without_modifying_source() {
        let dir = TempDir::new("bridge");
        let audio = dir.write("music/Artist/Album/song.wav", b"legacy opaque audio");
        let index = dir.write(
            "library-index.json",
            serde_json::json!({
                "version": INDEX_VERSION,
                "next_id": 2,
                "tracks": [{
                    "id": 1,
                    "title": "Legacy Song",
                    "artist": "Legacy Artist",
                    "album": "Legacy Album",
                    "track_number": 1,
                    "disc_number": 1,
                    "duration_ms": 1000,
                    "path": {"utf8": audio.to_str().expect("utf8")}
                }]
            })
            .to_string()
            .as_bytes(),
        );
        let index_before = fs::read(&index).expect("index");
        let audio_before = fs::read(&audio).expect("audio");
        let destination = dir.path().join("FluxLibrary");

        let report = migrate_legacy_index_to_canonical(&index, &destination).expect("migrate");
        assert_eq!(report.format(), CanonicalSourceFormat::LegacyIndex);
        assert_eq!(report.tracks(), 1);
        let library = CanonicalLibrary::open(&destination).expect("open");
        let track = library.track(TrackId::new(1)).expect("track id preserved");
        assert_eq!(track.title, "Legacy Song");
        assert_eq!(
            library
                .artist(track.owner_artist_id)
                .map(|a| a.name.as_str()),
            Some("Legacy Artist")
        );
        assert_eq!(
            library.album(track.album_id).map(|a| a.title.as_str()),
            Some("Legacy Album")
        );
        assert_eq!(fs::read(&index).expect("index after"), index_before);
        assert_eq!(fs::read(&audio).expect("audio after"), audio_before);
    }
}
