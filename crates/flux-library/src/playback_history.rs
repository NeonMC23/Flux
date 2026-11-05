//! Versioned, atomic persistence for local, derived playback history.
//!
//! History is deliberately separate from the canonical catalogue and audio
//! tags. It is not authoritative track metadata; it records only aggregated
//! meaningful plays and the last-played time for stable TrackIds. The current
//! schema requires the `entries` collection; a missing/wrong-typed collection
//! is corruption, never empty history. A missing/empty `kind` remains readable
//! for legacy version-1 files, but an unknown non-empty kind is rejected.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use flux_core::TrackId;
use serde::{Deserialize, Serialize};

use crate::fs_ops::replace_file;

/// The only playback-history schema version this build reads and writes.
pub const HISTORY_SCHEMA_VERSION: u32 = 1;

const HISTORY_KIND: &str = "flux-playback-history";

/// Aggregated history for one stable track identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayHistoryEntry {
    /// Number of meaningful plays.
    pub play_count: u64,
    /// Unix timestamp in milliseconds of the most recent play.
    pub last_played_unix_ms: u64,
}

/// Aggregated local playback history. Track IDs are stable canonical IDs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaybackHistory {
    entries: BTreeMap<TrackId, PlayHistoryEntry>,
}

impl PlaybackHistory {
    /// Records one meaningful play.
    pub fn record(&mut self, id: TrackId, played_unix_ms: u64) {
        let entry = self.entries.entry(id).or_insert(PlayHistoryEntry {
            play_count: 0,
            last_played_unix_ms: 0,
        });
        entry.play_count = entry.play_count.saturating_add(1);
        entry.last_played_unix_ms = played_unix_ms;
    }

    /// One track's aggregate.
    #[must_use]
    pub fn entry(&self, id: TrackId) -> Option<PlayHistoryEntry> {
        self.entries.get(&id).cloned()
    }

    /// All entries in stable track-ID order.
    pub fn entries(&self) -> impl Iterator<Item = (TrackId, PlayHistoryEntry)> + '_ {
        self.entries.iter().map(|(id, entry)| (*id, entry.clone()))
    }

    /// Total meaningful plays across all tracks.
    #[must_use]
    pub fn total_plays(&self) -> u64 {
        self.entries
            .values()
            .fold(0u64, |total, entry| total.saturating_add(entry.play_count))
    }

    /// Number of distinct tracks with history.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no track has history.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Errors produced while loading or saving playback history.
#[derive(Debug)]
pub enum PlaybackHistoryError {
    /// File I/O failed.
    Io(std::io::Error),
    /// JSON was malformed.
    Malformed(serde_json::Error),
    /// Serialization failed.
    Serialize(serde_json::Error),
    /// The file uses an unsupported schema version.
    UnsupportedVersion {
        /// Version found in the file.
        found: u32,
        /// Version supported by this build.
        supported: u32,
    },
    /// The file parsed but violated invariants.
    Invalid(String),
}

impl fmt::Display for PlaybackHistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not access playback history: {error}"),
            Self::Malformed(error) => write!(formatter, "playback history is malformed: {error}"),
            Self::Serialize(error) => {
                write!(formatter, "could not serialize playback history: {error}")
            }
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "playback history uses unsupported schema version {found} \
                 (this build supports {supported})"
            ),
            Self::Invalid(reason) => write!(formatter, "invalid playback history: {reason}"),
        }
    }
}

impl std::error::Error for PlaybackHistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Malformed(error) | Self::Serialize(error) => Some(error),
            Self::UnsupportedVersion { .. } | Self::Invalid(_) => None,
        }
    }
}

impl From<std::io::Error> for PlaybackHistoryError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// A versioned atomic history file at a caller-supplied path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackHistoryStore {
    path: PathBuf,
}

impl PlaybackHistoryStore {
    /// Creates a handle. Nothing is read or written until load/save.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The history file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads history, or `None` when the file does not exist.
    pub fn load(&self) -> Result<Option<PlaybackHistory>, PlaybackHistoryError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(PlaybackHistoryError::Io(error)),
        };
        let file: HistoryFile =
            serde_json::from_slice(&bytes).map_err(PlaybackHistoryError::Malformed)?;
        if file.version != HISTORY_SCHEMA_VERSION {
            return Err(PlaybackHistoryError::UnsupportedVersion {
                found: file.version,
                supported: HISTORY_SCHEMA_VERSION,
            });
        }
        if !file.kind.is_empty() && file.kind != HISTORY_KIND {
            return Err(PlaybackHistoryError::Invalid(format!(
                "playback history declares kind {:?}, expected {HISTORY_KIND:?}",
                file.kind
            )));
        }
        let mut entries = BTreeMap::new();
        for entry in file.entries {
            let id = TrackId::new(entry.track_id);
            if id.get() == 0 || entry.play_count == 0 {
                return Err(PlaybackHistoryError::Invalid(
                    "history entries require nonzero TrackId and play count".to_owned(),
                ));
            }
            if entries
                .insert(
                    id,
                    PlayHistoryEntry {
                        play_count: entry.play_count,
                        last_played_unix_ms: entry.last_played_unix_ms,
                    },
                )
                .is_some()
            {
                return Err(PlaybackHistoryError::Invalid(format!(
                    "track {id} appears more than once in history"
                )));
            }
        }
        Ok(Some(PlaybackHistory { entries }))
    }

    /// Atomically writes history, leaving the old file on failure.
    pub fn save(&self, history: &PlaybackHistory) -> Result<(), PlaybackHistoryError> {
        let file = HistoryFile {
            version: HISTORY_SCHEMA_VERSION,
            kind: HISTORY_KIND.to_owned(),
            entries: history
                .entries
                .iter()
                .map(|(id, entry)| HistoryEntryRecord {
                    track_id: id.get(),
                    play_count: entry.play_count,
                    last_played_unix_ms: entry.last_played_unix_ms,
                })
                .collect(),
        };
        let bytes = serde_json::to_vec_pretty(&file).map_err(PlaybackHistoryError::Serialize)?;
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temp_name = self.path.as_os_str().to_os_string();
        temp_name.push(".tmp");
        let temp = PathBuf::from(temp_name);
        let result = (|| -> Result<(), PlaybackHistoryError> {
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
struct HistoryFile {
    version: u32,
    #[serde(default)]
    kind: String,
    entries: Vec<HistoryEntryRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HistoryEntryRecord {
    track_id: u64,
    play_count: u64,
    last_played_unix_ms: u64,
}

fn sync_parent(path: &Path) -> Result<(), PlaybackHistoryError> {
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
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn round_trip_preserves_counts_and_last_play() {
        let dir = TempDir::new("history-roundtrip");
        let path = dir.path().join("history.flux");
        let store = PlaybackHistoryStore::new(&path);
        let mut history = PlaybackHistory::default();
        history.record(TrackId::new(2), 100);
        history.record(TrackId::new(1), 200);
        history.record(TrackId::new(2), 300);
        store.save(&history).expect("save");
        let loaded = store.load().expect("load").expect("exists");
        assert_eq!(loaded, history);
        assert_eq!(loaded.total_plays(), 3);
        assert_eq!(loaded.entry(TrackId::new(2)).expect("entry").play_count, 2);
        assert_eq!(
            loaded
                .entry(TrackId::new(2))
                .expect("entry")
                .last_played_unix_ms,
            300
        );
        assert!(!path.with_extension("flux.tmp").exists());
    }

    #[test]
    fn malformed_or_duplicate_history_is_rejected_without_rewriting() {
        let dir = TempDir::new("history-invalid");
        let path = dir.path().join("history.flux");
        let store = PlaybackHistoryStore::new(&path);
        fs::write(&path, b"not json").expect("fixture");
        assert!(store.load().is_err());
        assert_eq!(fs::read(&path).expect("preserved"), b"not json");

        let duplicate = r#"{"version":1,"kind":"flux-playback-history","entries":[{"track_id":1,"play_count":1,"last_played_unix_ms":1},{"track_id":1,"play_count":2,"last_played_unix_ms":2}]}"#;
        fs::write(&path, duplicate).expect("fixture");
        assert!(store.load().is_err());
        assert_eq!(fs::read_to_string(&path).expect("preserved"), duplicate);
    }

    #[test]
    fn required_collection_and_entry_fields_fail_closed_without_rewriting() {
        let dir = TempDir::new("history-required-fields");
        let path = dir.path().join("history.flux");
        let store = PlaybackHistoryStore::new(&path);
        let cases = [
            (
                "missing entries",
                br#"{"version":1,"kind":"flux-playback-history"}"#.to_vec(),
            ),
            (
                "wrong entries type",
                br#"{"version":1,"kind":"flux-playback-history","entries":{}}"#.to_vec(),
            ),
            (
                "missing entry track_id",
                br#"{"version":1,"kind":"flux-playback-history","entries":[{"play_count":1,"last_played_unix_ms":1}]}"#.to_vec(),
            ),
            (
                "missing entry play_count",
                br#"{"version":1,"kind":"flux-playback-history","entries":[{"track_id":1,"last_played_unix_ms":1}]}"#.to_vec(),
            ),
            (
                "missing entry timestamp",
                br#"{"version":1,"kind":"flux-playback-history","entries":[{"track_id":1,"play_count":1}]}"#.to_vec(),
            ),
            (
                "zero track",
                br#"{"version":1,"kind":"flux-playback-history","entries":[{"track_id":0,"play_count":1,"last_played_unix_ms":1}]}"#.to_vec(),
            ),
            (
                "zero play count",
                br#"{"version":1,"kind":"flux-playback-history","entries":[{"track_id":1,"play_count":0,"last_played_unix_ms":1}]}"#.to_vec(),
            ),
            (
                "wrong kind",
                br#"{"version":1,"kind":"some-other-kind","entries":[]}"#.to_vec(),
            ),
            (
                "future schema",
                format!(
                    r#"{{"version":{},"kind":"flux-playback-history","entries":[]}}"#,
                    HISTORY_SCHEMA_VERSION + 1
                )
                .into_bytes(),
            ),
            ("malformed", b"{not json".to_vec()),
            ("truncated", br#"{"version":1,"kind":"flux-playback-history","entries":["#.to_vec()),
        ];
        for (name, bytes) in cases {
            fs::write(&path, &bytes).expect("fixture");
            let error = store
                .load()
                .expect_err(&format!("{name} must not load as empty history"));
            assert!(!error.to_string().is_empty(), "{name}: empty error");
            assert_eq!(
                fs::read(&path).expect("preserved"),
                bytes,
                "{name}: failed load rewrote history"
            );
        }
    }

    #[test]
    fn empty_and_legacy_history_remain_supported() {
        let dir = TempDir::new("history-empty-legacy");
        let path = dir.path().join("history.flux");
        let store = PlaybackHistoryStore::new(&path);
        store
            .save(&PlaybackHistory::default())
            .expect("empty history save");
        let loaded = store.load().expect("empty load").expect("exists");
        assert!(loaded.is_empty());

        let legacy =
            br#"{"version":1,"entries":[{"track_id":3,"play_count":2,"last_played_unix_ms":99}]}"#;
        fs::write(&path, legacy).expect("legacy fixture");
        let loaded = store.load().expect("legacy load").expect("exists");
        assert_eq!(loaded.entry(TrackId::new(3)).expect("entry").play_count, 2);
        assert_eq!(fs::read(&path).expect("preserved"), legacy);
    }

    #[test]
    fn missing_history_is_none() {
        let dir = TempDir::new("history-missing");
        let store = PlaybackHistoryStore::new(dir.path().join("history.flux"));
        assert!(store.load().expect("missing").is_none());
    }
}
