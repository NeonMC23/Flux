//! Versioned, atomic persistence for a user-managed playback queue.
//!
//! The queue is deliberately a separate file from the track catalogue and
//! from audio tags. The caller chooses the path (the application derives
//! one per active library), so this module stays independent of
//! `flux-platform` and of app-data policy.
//!
//! Loading validates the whole file before accepting anything. A malformed
//! or unreadable file is an error, never an empty queue, and this module
//! never writes during load. Saving uses a sibling temporary file, a file
//! sync, and an atomic replacement of the old queue file.

use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flux_core::{Queue, QueueEntry, QueueEntryId, TrackId};
use serde::{Deserialize, Serialize};

use crate::fs_ops::replace_file;

/// The only queue-file schema version this build reads and writes.
pub const QUEUE_SCHEMA_VERSION: u32 = 1;

/// The result of loading a queue file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedQueue {
    /// The validated queue.
    pub queue: Queue,
    /// Whether this queue is still the automatically library-derived queue
    /// (and may receive newly scanned tracks) rather than a queue the user
    /// has structurally edited.
    pub auto_library: bool,
}

/// A versioned queue file at a caller-supplied path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStore {
    path: PathBuf,
}

impl QueueStore {
    /// Creates a queue store handle for `path`. Nothing is read or written
    /// until [`load`](Self::load) or [`save`](Self::save) is called.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The queue file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the queue, or `None` when the file does not exist.
    ///
    /// The file is only read. On any schema/content error the original
    /// bytes are left untouched.
    ///
    /// # Errors
    ///
    /// [`QueueStoreError`] for I/O failures, malformed JSON, unsupported
    /// versions, or invalid entry identity/selection data.
    pub fn load(&self) -> Result<Option<LoadedQueue>, QueueStoreError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(QueueStoreError::Io(error)),
        };
        let file: QueueFile = serde_json::from_slice(&bytes).map_err(QueueStoreError::Malformed)?;
        if file.version != QUEUE_SCHEMA_VERSION {
            return Err(QueueStoreError::UnsupportedVersion {
                found: file.version,
                supported: QUEUE_SCHEMA_VERSION,
            });
        }
        let entries: Vec<QueueEntry> = file
            .entries
            .into_iter()
            .map(|entry| QueueEntry::new(QueueEntryId::new(entry.id), TrackId::new(entry.track_id)))
            .collect();
        let current = file.current_entry_id.map(QueueEntryId::new);
        let queue = Queue::from_entries(entries, file.next_entry_id, current).ok_or_else(|| {
            QueueStoreError::Invalid(
                "entry IDs, counter, or current-entry selection are inconsistent".to_owned(),
            )
        })?;
        Ok(Some(LoadedQueue {
            queue,
            auto_library: file.auto_library,
        }))
    }

    /// Atomically writes the queue, replacing any previous queue file.
    ///
    /// # Errors
    ///
    /// [`QueueStoreError::Io`] when the file cannot be written, synced, or
    /// replaced. A failed save leaves the previous queue file in place.
    pub fn save(&self, queue: &Queue, auto_library: bool) -> Result<(), QueueStoreError> {
        let file = QueueFile {
            version: QUEUE_SCHEMA_VERSION,
            next_entry_id: queue.next_entry_id(),
            current_entry_id: queue.current_entry_id().map(QueueEntryId::get),
            auto_library,
            entries: queue
                .entries()
                .iter()
                .map(|entry| QueueEntryRecord {
                    id: entry.id().get(),
                    track_id: entry.track().get(),
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(QueueStoreError::Serialize)?;

        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temp_name = self.path.as_os_str().to_os_string();
        temp_name.push(".tmp");
        let temp = PathBuf::from(temp_name);

        let result = (|| -> Result<(), QueueStoreError> {
            let mut output = fs::File::create(&temp)?;
            output.write_all(&bytes)?;
            output.sync_all()?;
            drop(output);
            replace_file(&temp, &self.path)?;
            sync_parent(&self.path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct QueueFile {
    version: u32,
    next_entry_id: u64,
    #[serde(default)]
    current_entry_id: Option<u64>,
    /// True while the queue is still the library-derived default and may
    /// receive tracks added by later scans.
    #[serde(default)]
    auto_library: bool,
    #[serde(default)]
    entries: Vec<QueueEntryRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct QueueEntryRecord {
    id: u64,
    track_id: u64,
}

/// Errors produced while loading or saving a queue file.
#[derive(Debug)]
pub enum QueueStoreError {
    /// The queue file could not be read or written.
    Io(std::io::Error),
    /// The file is not valid JSON of the expected shape.
    Malformed(serde_json::Error),
    /// The file could not be serialized (practically unreachable).
    Serialize(serde_json::Error),
    /// The file uses an unsupported schema version.
    UnsupportedVersion {
        /// Version found in the file.
        found: u32,
        /// Version this build supports.
        supported: u32,
    },
    /// The file parsed but violates queue invariants.
    Invalid(String),
}

impl fmt::Display for QueueStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "could not access the queue file: {error}"),
            Self::Malformed(error) => write!(f, "the queue file is malformed: {error}"),
            Self::Serialize(error) => write!(f, "could not serialize the queue: {error}"),
            Self::UnsupportedVersion { found, supported } => write!(
                f,
                "the queue file uses unsupported schema version {found} \
                 (this build supports {supported})"
            ),
            Self::Invalid(reason) => write!(f, "invalid queue file: {reason}"),
        }
    }
}

impl std::error::Error for QueueStoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Malformed(error) | Self::Serialize(error) => Some(error),
            Self::UnsupportedVersion { .. } | Self::Invalid(_) => None,
        }
    }
}

impl From<std::io::Error> for QueueStoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

fn sync_parent(path: &Path) -> Result<(), QueueStoreError> {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::File::open(parent)?.sync_all()?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use flux_core::{Queue, TrackId};

    use super::{QUEUE_SCHEMA_VERSION, QueueStore};
    use crate::test_support::TempDir;

    fn queue_with_duplicates() -> Queue {
        let mut queue = Queue::new();
        let first = queue.push(TrackId::new(10));
        queue.push(TrackId::new(20));
        let last = queue.push(TrackId::new(10));
        queue.set_current_entry(last).expect("current");
        assert_ne!(first, last);
        queue
    }

    #[test]
    fn round_trip_preserves_duplicates_order_selection_and_counter() {
        let dir = TempDir::new("queue-round-trip");
        let path = dir.path().join("queue.json");
        let store = QueueStore::new(&path);
        let queue = queue_with_duplicates();
        let next = queue.next_entry_id();
        store.save(&queue, true).expect("save");

        let loaded = store.load().expect("load").expect("file exists");
        assert_eq!(loaded.queue, queue);
        assert!(loaded.auto_library);
        assert_eq!(loaded.queue.next_entry_id(), next);
        assert_eq!(
            loaded.queue.current_entry_id(),
            queue.current_entry_id(),
            "selection survives"
        );
        assert_eq!(loaded.queue.len(), 3, "duplicate tracks stay distinct");
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn missing_file_is_none_not_an_empty_queue() {
        let dir = TempDir::new("queue-missing");
        let store = QueueStore::new(dir.path().join("queue.json"));
        assert!(store.load().expect("missing is fine").is_none());
    }

    #[test]
    fn malformed_and_invalid_files_are_errors_and_are_not_rewritten() {
        let dir = TempDir::new("queue-malformed");
        let path = dir.path().join("queue.json");
        let original = b"not json at all";
        fs::write(&path, original).expect("fixture");
        let store = QueueStore::new(&path);
        assert!(store.load().is_err());
        assert_eq!(fs::read(&path).expect("preserved"), original);

        let invalid = serde_json::json!({
            "version": QUEUE_SCHEMA_VERSION,
            "next_entry_id": 2,
            "current_entry_id": 1,
            "entries": [
                {"id": 1, "track_id": 10},
                {"id": 1, "track_id": 20}
            ]
        })
        .to_string();
        fs::write(&path, &invalid).expect("fixture");
        assert!(store.load().is_err(), "duplicate entry IDs are invalid");
        assert_eq!(fs::read(&path).expect("preserved"), invalid.as_bytes());
    }

    #[test]
    fn load_rejects_bad_versions_counters_and_selection() {
        let dir = TempDir::new("queue-validation");
        let path = dir.path().join("queue.json");
        let store = QueueStore::new(&path);

        let bad_version = serde_json::json!({
            "version": 99, "next_entry_id": 1,
            "current_entry_id": null, "entries": []
        });
        fs::write(&path, bad_version.to_string()).expect("version");
        assert!(store.load().is_err());

        let bad_counter = serde_json::json!({
            "version": QUEUE_SCHEMA_VERSION, "next_entry_id": 1,
            "current_entry_id": 1,
            "entries": [{"id": 1, "track_id": 10}]
        });
        fs::write(&path, bad_counter.to_string()).expect("counter");
        assert!(store.load().is_err(), "counter must exceed all entry IDs");

        let bad_current = serde_json::json!({
            "version": QUEUE_SCHEMA_VERSION, "next_entry_id": 3,
            "current_entry_id": 2,
            "entries": [{"id": 1, "track_id": 10}]
        });
        fs::write(&path, bad_current.to_string()).expect("current");
        assert!(store.load().is_err(), "current must name an existing entry");

        let zero_track = serde_json::json!({
            "version": QUEUE_SCHEMA_VERSION, "next_entry_id": 2,
            "current_entry_id": 1,
            "entries": [{"id": 1, "track_id": 0}]
        });
        fs::write(&path, zero_track.to_string()).expect("track");
        assert!(store.load().is_err(), "track IDs start at 1");
    }

    #[test]
    fn save_replaces_an_existing_queue_and_cleans_the_temporary_file() {
        let dir = TempDir::new("queue-replace");
        let path = dir.path().join("queue.json");
        let store = QueueStore::new(&path);
        let mut first = Queue::new();
        first.push(TrackId::new(1));
        store.save(&first, false).expect("first");

        let mut second = Queue::new();
        second.push(TrackId::new(2));
        second.push(TrackId::new(3));
        store.save(&second, false).expect("second");
        let loaded = store.load().expect("load").expect("exists");
        assert_eq!(loaded.queue, second);
        assert!(!loaded.auto_library);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn empty_queue_round_trips_with_preserved_counter() {
        let dir = TempDir::new("queue-empty");
        let path = dir.path().join("queue.json");
        let store = QueueStore::new(&path);
        let mut queue = Queue::new();
        queue.push(TrackId::new(1));
        queue.clear();
        let next = queue.next_entry_id();
        store.save(&queue, true).expect("save");
        let loaded = store.load().expect("load").expect("exists").queue;
        assert!(loaded.is_empty());
        assert_eq!(loaded.current_entry_id(), None);
        assert_eq!(loaded.next_entry_id(), next);
    }
}
