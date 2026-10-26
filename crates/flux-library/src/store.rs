//! Versioned JSON persistence for [`Library`].
//!
//! The on-disk index is a single JSON document holding every track (with
//! its stable ID) and the next-ID counter. Serialization uses DTOs owned
//! by this module; `flux-core` stays serialization-free.
//!
//! # Guarantees and limitations
//!
//! - IDs and the counter round-trip exactly, so identities survive
//!   restarts and removed IDs are never reused afterwards.
//! - Loading validates before accepting anything: unsupported schema
//!   versions, duplicate IDs, duplicate paths, and impossible counters
//!   are typed errors ([`StoreError`]), never panics.
//! - Saving writes to a sibling `<name>.tmp` file, flushes it, then
//!   renames over the target. `fs::rename` replaces an existing file on
//!   Unix and Windows, so a failed save leaves the previous index in
//!   place; the old index is never deleted to make room. The parent
//!   directory is not synced, so durability of the rename across a
//!   power loss is not guaranteed (a rescan can rebuild the index, but
//!   the ID counter's history would be lost). A crash before the rename
//!   can leave a harmless `.tmp` file behind, which the next save
//!   overwrites.
//! - The caller supplies the path (typically under a `flux-platform`
//!   data directory) and is responsible for the parent directory
//!   existing; this module never picks locations itself.
//! - Paths are stored as UTF-8 text. On Unix, non-UTF-8 file names are
//!   preserved losslessly as raw bytes; on other platforms such (already
//!   pathological) names degrade to lossy text.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use flux_core::{Track, TrackId, TrackSource};
use serde::{Deserialize, Serialize};

use crate::Library;
use crate::fs_ops::replace_file;

/// The only index schema version this build can read and write.
pub const INDEX_VERSION: u32 = 1;

/// Errors produced while saving or loading a library index.
#[derive(Debug)]
pub enum StoreError {
    /// The index file could not be read or written.
    Io(std::io::Error),
    /// The index file is not valid JSON of the expected shape.
    Malformed(serde_json::Error),
    /// The index was written by an incompatible schema version.
    UnsupportedVersion {
        /// The version found in the file.
        found: u32,
        /// The version this build supports.
        supported: u32,
    },
    /// The index parsed but violates library invariants (duplicate IDs
    /// or paths, an impossible ID counter, …). The message says which.
    InvalidIndex(String),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "could not access the library index file: {err}"),
            Self::Malformed(err) => write!(f, "the library index is malformed: {err}"),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "unsupported library index version {found} (this build supports version {supported})"
            ),
            Self::InvalidIndex(reason) => write!(f, "invalid library index: {reason}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::Malformed(err) => Some(err),
            Self::UnsupportedVersion { .. } | Self::InvalidIndex(_) => None,
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        Self::Malformed(err)
    }
}

/// The serialized index document (schema [`INDEX_VERSION`]).
#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    version: u32,
    next_id: u64,
    tracks: Vec<TrackRecord>,
}

/// One serialized track. Field names are the on-disk schema; do not
/// rename without bumping [`INDEX_VERSION`].
#[derive(Debug, Serialize, Deserialize)]
struct TrackRecord {
    id: u64,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    duration_ms: Option<u64>,
    path: StoredPath,
}

/// A filesystem path in the index: UTF-8 text when possible, raw bytes
/// for non-UTF-8 Unix file names.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredPath {
    Utf8(String),
    UnixBytes(Vec<u8>),
}

impl StoredPath {
    fn from_path(path: &Path) -> Self {
        if let Some(text) = path.to_str() {
            return Self::Utf8(text.to_owned());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            Self::UnixBytes(path.as_os_str().as_bytes().to_vec())
        }
        #[cfg(not(unix))]
        {
            Self::Utf8(path.to_string_lossy().into_owned())
        }
    }

    fn into_path(self) -> PathBuf {
        match self {
            Self::Utf8(text) => PathBuf::from(text),
            Self::UnixBytes(bytes) => {
                #[cfg(unix)]
                {
                    use std::ffi::OsString;
                    use std::os::unix::ffi::OsStringExt;
                    PathBuf::from(OsString::from_vec(bytes))
                }
                #[cfg(not(unix))]
                {
                    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
                }
            }
        }
    }
}

impl Library {
    /// Writes the whole library (tracks, stable IDs, ID counter) as a
    /// versioned JSON index at `path`, replacing any existing file.
    ///
    /// The write goes through a sibling temporary file plus rename; see
    /// the module docs for the exact durability guarantees. A failed
    /// replacement leaves any existing index untouched. The parent
    /// directory must already exist.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] if the file cannot be written or replaced.
    pub fn save_to_path(&self, path: &Path) -> Result<(), StoreError> {
        let (tracks, next_id) = self.parts();
        let index = IndexFile {
            version: INDEX_VERSION,
            next_id,
            tracks: tracks
                .iter()
                .map(|(id, track)| TrackRecord {
                    id: id.get(),
                    title: track.title().to_owned(),
                    artist: track.artist().map(ToOwned::to_owned),
                    album: track.album().map(ToOwned::to_owned),
                    track_number: track.track_number(),
                    disc_number: track.disc_number(),
                    duration_ms: track
                        .duration()
                        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
                    path: StoredPath::from_path(track.source().path()),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&index)?;

        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp_path = PathBuf::from(tmp_name);

        let write_result = (|| -> Result<(), StoreError> {
            let mut file = fs::File::create(&tmp_path)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            replace_file(&tmp_path, path)?;
            Ok(())
        })();
        if write_result.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
        write_result
    }

    /// Loads a library index written by [`save_to_path`](Self::save_to_path),
    /// restoring tracks, their stable IDs (in stored order), and the
    /// next-ID counter.
    ///
    /// # Errors
    ///
    /// - [`StoreError::Io`] — the file cannot be read (including missing).
    /// - [`StoreError::Malformed`] — not JSON of the expected shape.
    /// - [`StoreError::UnsupportedVersion`] — schema version mismatch.
    /// - [`StoreError::InvalidIndex`] — duplicate IDs/paths or an
    ///   impossible ID counter. Nothing is accepted partially: either a
    ///   complete valid `Library` is returned or an error is.
    pub fn load_from_path(path: &Path) -> Result<Library, StoreError> {
        let bytes = fs::read(path)?;
        let index: IndexFile = serde_json::from_slice(&bytes)?;
        validate_and_build(index)
    }
}

/// Checks every invariant a persisted index must satisfy, then builds
/// the in-memory library from it.
fn validate_and_build(index: IndexFile) -> Result<Library, StoreError> {
    if index.version != INDEX_VERSION {
        return Err(StoreError::UnsupportedVersion {
            found: index.version,
            supported: INDEX_VERSION,
        });
    }
    if index.next_id == 0 {
        return Err(StoreError::InvalidIndex(
            "next_id counter is 0 (IDs start at 1)".to_owned(),
        ));
    }

    let mut seen_ids: HashSet<u64> = HashSet::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut max_id = 0u64;
    let mut tracks: Vec<(TrackId, Track)> = Vec::with_capacity(index.tracks.len());

    for record in index.tracks {
        if record.id == 0 {
            return Err(StoreError::InvalidIndex(
                "a track has ID 0 (IDs start at 1)".to_owned(),
            ));
        }
        if !seen_ids.insert(record.id) {
            return Err(StoreError::InvalidIndex(format!(
                "duplicate track ID {}",
                record.id
            )));
        }
        max_id = max_id.max(record.id);

        let path = record.path.into_path();
        if !seen_paths.insert(path.clone()) {
            return Err(StoreError::InvalidIndex(format!(
                "duplicate track path {}",
                path.display()
            )));
        }

        let mut track = Track::new(record.title, TrackSource::local(path));
        if let Some(artist) = record.artist {
            track = track.with_artist(artist);
        }
        if let Some(album) = record.album {
            track = track.with_album(album);
        }
        if let Some(number) = record.track_number {
            track = track.with_track_number(number);
        }
        if let Some(disc) = record.disc_number {
            track = track.with_disc_number(disc);
        }
        if let Some(ms) = record.duration_ms {
            track = track.with_duration(Duration::from_millis(ms));
        }
        tracks.push((TrackId::new(record.id), track));
    }

    if index.next_id <= max_id {
        return Err(StoreError::InvalidIndex(format!(
            "next_id counter {} would reuse an existing ID (max in use: {max_id})",
            index.next_id
        )));
    }

    Ok(Library::from_parts(tracks, index.next_id))
}

// ------------------------------------------------ migration decoding ---
//
// S3 migration ([`crate::migrate`]) must see legacy paths in their
// original stored representation. It therefore cannot use
// [`Library::load_from_path`]: that loader converts every
// [`StoredPath::UnixBytes`] through `StoredPath::into_path`, which on
// non-Unix platforms decodes with `String::from_utf8_lossy` and would
// silently rewrite non-UTF-8 names into different (replacement-
// character) paths. The decoder below is migration-only: it applies
// the same schema, version, ID, and counter validation as the public
// loader, but returns paths unconverted so the migration layer can
// classify and report them byte-exactly. Duplicate-path detection is
// deliberately *not* repeated here — the public loader detects it via
// converted `PathBuf` equality (the lossy conversion migration must
// avoid), so migration performs its own lossless, component-based
// duplicate check on the values returned here. The legacy schema and
// the public loader's behavior are unchanged.

/// A legacy track path in its original stored representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LegacyPathData {
    /// UTF-8 path text, exactly as stored (native spelling of the
    /// platform that wrote the index).
    Utf8(String),
    /// Raw Unix path bytes, exactly as stored.
    UnixBytes(Vec<u8>),
}

/// One legacy track record, decoded losslessly for migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyTrackData {
    pub(crate) id: u64,
    pub(crate) title: String,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) track_number: Option<u32>,
    pub(crate) disc_number: Option<u32>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) path: LegacyPathData,
}

/// A whole legacy index, decoded losslessly for migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LegacyIndexData {
    pub(crate) next_id: u64,
    pub(crate) tracks: Vec<LegacyTrackData>,
}

/// Reads and validates a legacy index for migration, preserving the
/// original path representation of every record. Read-only: the index
/// file is never modified.
///
/// Validation mirrors the public loader (supported version, non-zero
/// IDs, no duplicate IDs, a counter that reuses nothing); see the
/// section comment above for why duplicate *paths* are checked by the
/// caller instead.
pub(crate) fn load_legacy_index_data(path: &Path) -> Result<LegacyIndexData, StoreError> {
    let bytes = fs::read(path)?;
    let index: IndexFile = serde_json::from_slice(&bytes)?;

    if index.version != INDEX_VERSION {
        return Err(StoreError::UnsupportedVersion {
            found: index.version,
            supported: INDEX_VERSION,
        });
    }
    if index.next_id == 0 {
        return Err(StoreError::InvalidIndex(
            "next_id counter is 0 (IDs start at 1)".to_owned(),
        ));
    }

    let mut seen_ids: HashSet<u64> = HashSet::new();
    let mut max_id = 0u64;
    let mut tracks: Vec<LegacyTrackData> = Vec::with_capacity(index.tracks.len());
    for record in index.tracks {
        if record.id == 0 {
            return Err(StoreError::InvalidIndex(
                "a track has ID 0 (IDs start at 1)".to_owned(),
            ));
        }
        if !seen_ids.insert(record.id) {
            return Err(StoreError::InvalidIndex(format!(
                "duplicate track ID {}",
                record.id
            )));
        }
        max_id = max_id.max(record.id);
        tracks.push(LegacyTrackData {
            id: record.id,
            title: record.title,
            artist: record.artist,
            album: record.album,
            track_number: record.track_number,
            disc_number: record.disc_number,
            duration_ms: record.duration_ms,
            path: match record.path {
                StoredPath::Utf8(text) => LegacyPathData::Utf8(text),
                StoredPath::UnixBytes(bytes) => LegacyPathData::UnixBytes(bytes),
            },
        });
    }

    if index.next_id <= max_id {
        return Err(StoreError::InvalidIndex(format!(
            "next_id counter {} would reuse an existing ID (max in use: {max_id})",
            index.next_id
        )));
    }

    Ok(LegacyIndexData {
        next_id: index.next_id,
        tracks,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

    use flux_core::{Track, TrackId, TrackSource};

    use super::{INDEX_VERSION, StoreError};
    use crate::Library;
    use crate::test_support::{TempDir, flac_bytes};

    fn sample_library() -> Library {
        let mut library = Library::new();
        library.insert(
            Track::new("One", TrackSource::local("/music/one.flac"))
                .with_artist("Artist")
                .with_duration(Duration::from_millis(83_500)),
        );
        library.insert(Track::new("Two", TrackSource::local("/music/two.flac")));
        library
    }

    fn parts(library: &Library) -> Vec<(TrackId, Track)> {
        library.iter().map(|(id, t)| (id, t.clone())).collect()
    }

    #[test]
    fn round_trip_preserves_tracks_ids_and_counter() {
        let dir = TempDir::new("round-trip");
        let index_path = dir.path().join("index.json");

        let mut library = sample_library();
        let doomed = library.insert(Track::new("Three", TrackSource::local("/music/3.flac")));
        library.remove(doomed);
        library.save_to_path(&index_path).expect("save works");

        let mut loaded = Library::load_from_path(&index_path).expect("load works");
        assert_eq!(parts(&loaded), parts(&library));
        // The removed track's ID (3) stays retired after the restart:
        // the next insert continues at 4.
        let next = loaded.insert(Track::new("Four", TrackSource::local("/music/4.flac")));
        assert_eq!(next.get(), 4);
    }

    #[test]
    fn rescan_after_restart_reuses_ids_and_refreshes_in_place() {
        let dir = TempDir::new("restart-rescan");
        let song = dir.write("song.flac", &flac_bytes(&[("TITLE", "Before")]));
        let index_path = dir.path().join("index.json");

        let mut library = Library::new();
        library.scan(&[dir.path()]);
        // scan also sees index.json (unsupported extension -> skipped),
        // which does not affect track counts.
        let id = library.id_for_path(&song).expect("scanned");
        library.save_to_path(&index_path).expect("save");

        let mut restored = Library::load_from_path(&index_path).expect("load");
        let report = restored.scan(&[dir.path()]);
        assert_eq!(report.added(), 0);
        assert_eq!(report.deduplicated(), 1);
        assert_eq!(restored.id_for_path(&song), Some(id));

        fs::write(&song, flac_bytes(&[("TITLE", "After")])).expect("rewrite");
        let report = restored.scan(&[dir.path()]);
        assert_eq!(report.updated(), 1);
        assert_eq!(restored.id_for_path(&song), Some(id));
        assert_eq!(restored.get(id).map(Track::title), Some("After"));
    }

    #[test]
    fn save_replaces_existing_index_and_leaves_no_temp_file() {
        let dir = TempDir::new("replace");
        let index_path = dir.path().join("index.json");
        let mut library = sample_library();
        library.save_to_path(&index_path).expect("first save");
        library.insert(Track::new("Extra", TrackSource::local("/music/x.flac")));
        library
            .save_to_path(&index_path)
            .expect("second save replaces");

        let loaded = Library::load_from_path(&index_path).expect("load");
        assert_eq!(loaded.len(), 3);
        assert!(
            !dir.path().join("index.json.tmp").exists(),
            "temp file is cleaned up by rename"
        );
    }

    #[test]
    fn missing_file_and_missing_directory_are_io_errors() {
        let dir = TempDir::new("io-errors");
        let err = Library::load_from_path(&dir.path().join("nope.json"))
            .err()
            .expect("must fail");
        assert!(
            matches!(err, StoreError::Io(ref e) if e.kind() == std::io::ErrorKind::NotFound),
            "got {err:?}"
        );

        let library = sample_library();
        let err = library
            .save_to_path(&dir.path().join("no/such/dir/index.json"))
            .unwrap_err();
        assert!(matches!(err, StoreError::Io(_)), "got {err:?}");
    }

    fn load_raw(dir: &TempDir, content: &str) -> Result<Library, StoreError> {
        let path = dir.write("index.json", content.as_bytes());
        Library::load_from_path(&path)
    }

    #[test]
    fn corrupt_json_is_malformed_not_a_panic() {
        let dir = TempDir::new("corrupt");
        assert!(matches!(
            load_raw(&dir, "{ not json"),
            Err(StoreError::Malformed(_))
        ));
        assert!(matches!(
            load_raw(&dir, "[]"),
            Err(StoreError::Malformed(_))
        ));
        assert!(matches!(load_raw(&dir, ""), Err(StoreError::Malformed(_))));
        assert!(
            matches!(
                load_raw(&dir, r#"{"version":1}"#),
                Err(StoreError::Malformed(_))
            ),
            "missing fields are malformed"
        );
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let dir = TempDir::new("version");
        let err = load_raw(&dir, r#"{"version":999,"next_id":1,"tracks":[]}"#)
            .err()
            .expect("must fail");
        assert!(
            matches!(err, StoreError::UnsupportedVersion { found: 999, supported } if supported == INDEX_VERSION),
            "got {err:?}"
        );
    }

    fn record(id: u64, path: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "title": "T", "artist": null, "album": null,
            "track_number": null, "disc_number": null, "duration_ms": null,
            "path": {"utf8": path}
        })
    }

    fn index_json(next_id: u64, tracks: Vec<serde_json::Value>) -> String {
        serde_json::json!({"version": INDEX_VERSION, "next_id": next_id, "tracks": tracks})
            .to_string()
    }

    #[test]
    fn duplicate_ids_and_paths_are_invalid() {
        let dir = TempDir::new("duplicates");
        let dup_ids = index_json(3, vec![record(1, "/a.flac"), record(1, "/b.flac")]);
        assert!(matches!(
            load_raw(&dir, &dup_ids),
            Err(StoreError::InvalidIndex(_))
        ));

        let dup_paths = index_json(3, vec![record(1, "/a.flac"), record(2, "/a.flac")]);
        assert!(matches!(
            load_raw(&dir, &dup_paths),
            Err(StoreError::InvalidIndex(_))
        ));

        // Path equality applies: a `.` spelling variant is the same key.
        let dup_variant = index_json(3, vec![record(1, "/a.flac"), record(2, "/./a.flac")]);
        assert!(matches!(
            load_raw(&dir, &dup_variant),
            Err(StoreError::InvalidIndex(_))
        ));
    }

    #[test]
    fn invalid_counters_and_zero_ids_are_invalid() {
        let dir = TempDir::new("counters");
        assert!(
            matches!(
                load_raw(&dir, &index_json(0, vec![])),
                Err(StoreError::InvalidIndex(_))
            ),
            "next_id 0"
        );
        assert!(
            matches!(
                load_raw(&dir, &index_json(2, vec![record(2, "/a.flac")])),
                Err(StoreError::InvalidIndex(_))
            ),
            "next_id would reuse ID 2"
        );
        assert!(
            matches!(
                load_raw(&dir, &index_json(2, vec![record(0, "/a.flac")])),
                Err(StoreError::InvalidIndex(_))
            ),
            "ID 0 entry"
        );
        // A valid empty index loads fine.
        assert!(load_raw(&dir, &index_json(1, vec![])).is_ok());
    }

    #[test]
    fn migration_decoder_preserves_raw_path_representations() {
        use super::{LegacyPathData, load_legacy_index_data};

        // Raw bytes that are not valid UTF-8: on non-Unix the public
        // loader would decode this with replacement characters, which
        // is exactly what migration must never do.
        let raw_bytes: Vec<u8> = b"/music/bad-\xff-name.flac".to_vec();
        let json = serde_json::json!({
            "version": INDEX_VERSION,
            "next_id": 3,
            "tracks": [
                {
                    "id": 1, "title": "Text", "artist": null, "album": null,
                    "track_number": null, "disc_number": null,
                    "duration_ms": null, "path": {"utf8": "/music/ok.flac"}
                },
                {
                    "id": 2, "title": "Bytes", "artist": "A", "album": null,
                    "track_number": 7, "disc_number": null,
                    "duration_ms": 1234, "path": {"unix_bytes": raw_bytes}
                },
            ]
        })
        .to_string();

        let dir = TempDir::new("raw-decode");
        let path = dir.write("index.json", json.as_bytes());
        let data = load_legacy_index_data(&path).expect("raw decode");
        assert_eq!(data.next_id, 3);
        assert_eq!(
            data.tracks[0].path,
            LegacyPathData::Utf8("/music/ok.flac".to_owned())
        );
        assert_eq!(
            data.tracks[1].path,
            LegacyPathData::UnixBytes(raw_bytes),
            "non-UTF-8 bytes preserved exactly, no lossy decode"
        );
        assert_eq!(data.tracks[1].artist.as_deref(), Some("A"));
        assert_eq!(data.tracks[1].track_number, Some(7));
        assert_eq!(data.tracks[1].duration_ms, Some(1234));

        // The same scalar validation as the public loader applies.
        let dup = index_json(3, vec![record(1, "/a.flac"), record(1, "/b.flac")]);
        let dup_path = dir.write("dup.json", dup.as_bytes());
        assert!(matches!(
            load_legacy_index_data(&dup_path),
            Err(StoreError::InvalidIndex(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_round_trip_on_unix() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = TempDir::new("non-utf8");
        let weird = PathBuf::from(OsString::from_vec(b"/music/bad-\xff-name.flac".to_vec()));
        let mut library = Library::new();
        let id = library.insert(Track::new("Weird", TrackSource::local(weird.clone())));
        let index_path = dir.path().join("index.json");
        library.save_to_path(&index_path).expect("save");

        let loaded = Library::load_from_path(&index_path).expect("load");
        assert_eq!(loaded.id_for_path(&weird), Some(id));
    }
}
