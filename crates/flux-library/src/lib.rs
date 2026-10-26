//! Local music library for Flux: in-memory collection, scanning, and
//! persistence.
//!
//! A [`Library`] is an insertion-ordered collection of [`Track`]s with
//! stable in-memory IDs, path-based deduplication, and simple text search.
//! On top of that collection this crate provides:
//!
//! - [`scan`](Library::scan): recursive scanning of caller-provided root
//!   directories, reading embedded metadata (via `lofty`) with a
//!   filename-derived title fallback. All filesystem traversal and tag
//!   reading in Flux lives in this crate.
//! - Persistence: [`save_to_path`](Library::save_to_path) /
//!   [`load_from_path`](Library::load_from_path) implement a versioned
//!   JSON index at a caller-injected path. This crate never chooses
//!   OS-specific application directories (that is `flux-platform`'s job,
//!   and the two crates are deliberately independent).
//! - [`portable`]: the segmented `.flux` storage engine for a portable
//!   library root — catalog + generation-qualified track segments,
//!   crash-safe publication, backup recovery, byte-exact relative
//!   paths, and sparse metadata overrides.
//! - [`migrate`]: explicit, plan-first migration from the legacy index
//!   to a portable store at a caller-supplied root, preserving track
//!   IDs and the ID counter. Neither the portable store nor migration
//!   is wired into application startup yet — the legacy index above
//!   remains the startup path until the library lifecycle milestone;
//!   both are exposed and tested independently.
//!
//! # Stale-ID boundary
//!
//! `Library` knows nothing about queues or players and never touches
//! them. Removing a track here therefore does **not** clean up any
//! `flux_core::Queue` that still references its ID: reconciliation is the
//! caller's (a future orchestration layer's) responsibility. The intended
//! pattern after a removal is to call `queue.remove(id)` for each removed
//! ID; [`get`](Library::get), [`contains`](Library::contains), and
//! [`iter`](Library::iter) are sufficient to audit which IDs are still
//! alive. This asymmetry is deliberate — it keeps `flux-library`
//! independent of `flux-audio`.
//!
//! # Path identity
//!
//! A track's identity within a library is its local path *value*, exactly
//! as inserted or discovered by a scan. Paths are never canonicalized
//! against the filesystem. Consequences, all intentional: rescanning the
//! same root spelling is fully deduplicated; reaching the same file via a
//! *different* spelling (relative vs. absolute roots, `..` segments,
//! differing case) is a different identity and yields a second entry; and
//! a track whose file has disappeared stays in the library until it is
//! explicitly removed — scanning only ever adds or updates, never prunes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use flux_core::{Track, TrackId};

mod fs_ops;
pub mod library_lock;
pub mod migrate;
pub mod portable;
pub mod queue_store;
pub mod scan;
pub mod store;

pub use library_lock::{LibraryLock, LibraryLockError};
pub use migrate::{
    ExclusionReason, IdGap, LegacyPath, MigrationDecision, MigrationError, MigrationPlan,
    MigrationReport, MigrationStage, PlannedRecord,
};
pub use portable::{
    Availability, EffectiveMetadata, FileFingerprint, ImportedTrack, MetadataOverrides,
    OpenedStore, PORTABLE_SCHEMA_VERSION, PathCasePolicy, PathValidationError,
    PortableRefreshReport, PortableStore, PortableStoreError, PortableSyncReport,
    PortableSyncSkipped, ReconcileOutcome, ReconcileReport, RecoveryInfo, RelPath,
    RelinkCandidates, RelinkError, SegmentId, StoredTrack, TagSet, reconcile_portable_store,
    refresh_library_from_portable_store, relink_portable_track,
};
pub use queue_store::{LoadedQueue, QUEUE_SCHEMA_VERSION, QueueStore, QueueStoreError};
pub use scan::{SUPPORTED_EXTENSIONS, ScanIssue, ScanReport};
pub use store::{INDEX_VERSION, StoreError};

/// An in-memory, insertion-ordered track collection.
///
/// # ID semantics
///
/// IDs are issued sequentially starting at 1 and increase monotonically.
/// An ID is **never reused** after its track is removed, so a stale ID
/// held elsewhere can never silently start referring to a different track;
/// it will simply not be found. IDs are meaningful only for the lifetime
/// of this `Library` value — plus, when the library is persisted with
/// [`save_to_path`](Library::save_to_path), across save/load cycles of
/// that same index (the ID counter is stored, so removed IDs stay retired
/// after a restart). IDs are *not* unique across independent `Library`
/// instances (see [`TrackId`]).
///
/// # Deduplication
///
/// Inserting a track whose local path already exists in the library does
/// not add a second entry and does not modify the stored track ("first
/// wins" — to refresh metadata for an existing path, use
/// [`update_metadata`](Library::update_metadata)). Path comparison is a
/// plain [`Path`] value comparison — component-wise, case-sensitive on
/// every platform, with no filesystem access or canonicalization. Rust's
/// component-wise equality ignores redundant `.` segments (so
/// `/music/./a.flac` equals `/music/a.flac`), but it does not resolve
/// `..` segments and never folds letter case (so `/music/sub/../a.flac`
/// and `/music/A.flac` are *different* keys here). The rule is
/// intentionally deterministic and platform-independent; resolving such
/// aliases down to one real file is a future scanner concern, not the
/// collection's.
///
/// ID and path lookups go through derived in-memory indexes that mirror
/// `tracks` exactly: the `Vec` remains the authoritative,
/// insertion-ordered record set, and every successful mutation updates
/// the indexes as part of the same operation (failed mutations change
/// neither the records nor the indexes). The indexes are never
/// persisted; they are rebuilt from the validated records whenever a
/// `Library` is constructed.
pub struct Library {
    tracks: Vec<(TrackId, Track)>,
    next_id: u64,
    /// Derived index: track ID → position in `tracks`.
    by_id: HashMap<TrackId, usize>,
    /// Derived index: local path → track ID, keyed by the same plain
    /// [`Path`] value equality (and matching `Hash`) as deduplication.
    by_path: HashMap<PathBuf, TrackId>,
}

impl Library {
    /// Creates an empty library.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            next_id: 1,
            by_id: HashMap::new(),
            by_path: HashMap::new(),
        }
    }

    /// Adds a track and returns its library ID.
    ///
    /// If a track with the same local path (see the deduplication rule on
    /// [`Library`]) is already present, the stored track is kept exactly
    /// as it is — the incoming track, including any different metadata,
    /// is dropped — and the **existing** track's ID is returned.
    pub fn insert(&mut self, track: Track) -> TrackId {
        if let Some(existing_id) = self.by_path.get(track.source().path()) {
            return *existing_id;
        }
        let id = TrackId::new(self.next_id);
        self.next_id += 1;
        self.by_path.insert(track.source().path().to_owned(), id);
        self.by_id.insert(id, self.tracks.len());
        self.tracks.push((id, track));
        id
    }

    /// Returns the track with the given ID, or `None` if it is not (or no
    /// longer) in the library.
    #[must_use]
    pub fn get(&self, id: TrackId) -> Option<&Track> {
        let index = *self.by_id.get(&id)?;
        self.tracks.get(index).map(|(_, track)| track)
    }

    /// Returns the ID of the track stored for `path`, using the same path
    /// equality as deduplication, or `None` if no track has that path.
    #[must_use]
    pub fn id_for_path(&self, path: &Path) -> Option<TrackId> {
        self.by_path.get(path).copied()
    }

    /// Replaces the metadata of the track with the given ID, keeping the
    /// ID itself unchanged. Returns whether an update happened.
    ///
    /// The replacement's source path must equal the stored track's path
    /// (under the library's path-equality rule): this operation refreshes
    /// *metadata for the same file*, it never moves an ID to a different
    /// path. It returns `false` — changing nothing — when the ID is
    /// unknown or the paths differ. This is the rescan path: a file whose
    /// tags changed keeps its identity, so queues referencing the ID stay
    /// valid.
    pub fn update_metadata(&mut self, id: TrackId, track: Track) -> bool {
        let Some(&index) = self.by_id.get(&id) else {
            return false;
        };
        let Some((_, stored)) = self.tracks.get_mut(index) else {
            return false;
        };
        if stored.source().path() != track.source().path() {
            return false;
        }
        // The paths are equal but may be spelled differently (e.g. a
        // redundant `.` segment), and the stored track adopts the
        // replacement's spelling — refresh the path-index key so the
        // index stays an exact mirror of the records.
        let old_path = stored.source().path().to_owned();
        let new_path = track.source().path().to_owned();
        *stored = track;
        self.by_path.remove(&old_path);
        self.by_path.insert(new_path, id);
        true
    }

    /// Removes the track with the given ID and returns it, or `None` if it
    /// was not present. The relative order of the remaining tracks is
    /// preserved, and the removed ID is never reused.
    ///
    /// Re-inserting the same path afterwards creates a *new* track with a
    /// *new* ID: removal retires the old identity permanently. Nothing
    /// else (queues, players) is notified — see the stale-ID boundary
    /// documented at the crate level.
    pub fn remove(&mut self, id: TrackId) -> Option<Track> {
        let position = *self.by_id.get(&id)?;
        let (_, removed) = self.tracks.remove(position);
        self.by_id.remove(&id);
        self.by_path.remove(removed.source().path());
        // `Vec::remove` shifts every later entry one position earlier;
        // rewrite their ID-index entries to match.
        for (offset, (track_id, _)) in self.tracks[position..].iter().enumerate() {
            self.by_id.insert(*track_id, position + offset);
        }
        Some(removed)
    }

    /// Whether a track with the given ID is currently in the library.
    #[must_use]
    pub fn contains(&self, id: TrackId) -> bool {
        self.get(id).is_some()
    }

    /// The number of tracks in the library.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    /// Whether the library holds no tracks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Iterates over `(id, track)` pairs in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (TrackId, &Track)> {
        self.tracks.iter().map(|(id, track)| (*id, track))
    }

    /// Case-insensitive substring search over title, artist, and album,
    /// in insertion order.
    ///
    /// Matching uses Unicode-aware lowercasing of both the query and the
    /// fields. Leading/trailing whitespace in the query is ignored, and
    /// an empty (or whitespace-only) query matches every track. A track
    /// matches when *any* of the three fields contains the query; a
    /// missing artist or album simply cannot match on that field.
    pub fn search<'a>(&'a self, query: &str) -> impl Iterator<Item = (TrackId, &'a Track)> + 'a {
        let needle = query.trim().to_lowercase();
        self.iter().filter(move |(_, track)| {
            needle.is_empty()
                || track.title().to_lowercase().contains(&needle)
                || track
                    .artist()
                    .is_some_and(|a| a.to_lowercase().contains(&needle))
                || track
                    .album()
                    .is_some_and(|a| a.to_lowercase().contains(&needle))
        })
    }

    /// Persistence seam: the stored entries and the next-ID counter.
    /// Only [`store`] uses this; it is not part of the public API.
    pub(crate) fn parts(&self) -> (&[(TrackId, Track)], u64) {
        (&self.tracks, self.next_id)
    }

    /// Persistence seam: rebuilds a library from already-validated parts
    /// (see [`store`]'s load validation, which guarantees unique IDs and
    /// paths). The derived indexes are built here, before the library is
    /// exposed. Not part of the public API.
    pub(crate) fn from_parts(tracks: Vec<(TrackId, Track)>, next_id: u64) -> Self {
        let mut by_id = HashMap::with_capacity(tracks.len());
        let mut by_path = HashMap::with_capacity(tracks.len());
        for (index, (id, track)) in tracks.iter().enumerate() {
            debug_assert!(
                by_id.insert(*id, index).is_none(),
                "from_parts received a duplicate track ID"
            );
            debug_assert!(
                by_path
                    .insert(track.source().path().to_owned(), *id)
                    .is_none(),
                "from_parts received a duplicate track path"
            );
        }
        Self {
            tracks,
            next_id,
            by_id,
            by_path,
        }
    }
}

impl Default for Library {
    fn default() -> Self {
        Self::new()
    }
}

/// Filesystem-fixture helpers shared by this crate's test modules.
/// Everything here creates small synthetic files under the OS temp dir
/// and cleans them up; no real music library is ever touched.
#[cfg(test)]
pub(crate) mod test_support {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A unique temporary directory that deletes itself on drop.
    pub struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub fn new(test_name: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-test-{}-{test_name}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        pub fn path(&self) -> &std::path::Path {
            &self.path
        }

        /// Writes `bytes` to `relative` inside the dir (creating parent
        /// directories) and returns the full path.
        pub fn write(&self, relative: &str, bytes: &[u8]) -> PathBuf {
            let full = self.path.join(relative);
            fs::create_dir_all(full.parent().expect("parent")).expect("create parents");
            fs::write(&full, bytes).expect("write fixture");
            full
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// Builds the bytes of a minimal valid FLAC file: a well-formed
    /// STREAMINFO (44.1 kHz, 2ch, 16-bit, 3 seconds) followed by a Vorbis
    /// comment block holding `comments` (keys are written verbatim, e.g.
    /// `"TITLE"`). This is enough structure for `lofty` to parse real
    /// properties and tags without any real audio data.
    pub fn flac_bytes(comments: &[(&str, &str)]) -> Vec<u8> {
        let mut out = b"fLaC".to_vec();
        // STREAMINFO block (34 bytes), not the last block.
        let mut streaminfo = Vec::new();
        streaminfo.extend(4096u16.to_be_bytes()); // min block size
        streaminfo.extend(4096u16.to_be_bytes()); // max block size
        streaminfo.extend([0, 0, 0]); // min frame size
        streaminfo.extend([0, 0, 0]); // max frame size
        // sample_rate(20 bits) | channels-1(3) | bits-1(5) | total samples(36)
        let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 132_300u64;
        streaminfo.extend(packed.to_be_bytes());
        streaminfo.extend([0u8; 16]); // MD5 (unused)
        debug_assert_eq!(streaminfo.len(), 34);
        out.push(0x00); // last-block=0, type=0 (STREAMINFO)
        out.extend([0, 0, 34]);
        out.extend(streaminfo);
        // Vorbis comment block, marked as the last block.
        let mut data = Vec::new();
        let vendor = b"flux-test";
        data.extend(u32::try_from(vendor.len()).expect("len").to_le_bytes());
        data.extend(vendor);
        data.extend(u32::try_from(comments.len()).expect("len").to_le_bytes());
        for (key, value) in comments {
            let entry = format!("{key}={value}");
            data.extend(u32::try_from(entry.len()).expect("len").to_le_bytes());
            data.extend(entry.as_bytes());
        }
        out.push(0x84); // last-block=1, type=4 (VORBIS_COMMENT)
        let len = u32::try_from(data.len()).expect("len");
        out.extend([(len >> 16) as u8, (len >> 8) as u8, len as u8]);
        out.extend(data);
        out
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use flux_core::{Track, TrackId, TrackSource};

    use super::Library;

    fn track(title: &str, path: &str) -> Track {
        Track::new(title, TrackSource::local(path))
    }

    #[test]
    fn insert_assigns_sequential_ids_and_get_retrieves() {
        let mut library = Library::new();
        assert!(library.is_empty());
        let first = library.insert(track("One", "/music/one.flac"));
        let second = library.insert(track("Two", "/music/two.flac"));
        assert_eq!(first.get(), 1);
        assert_eq!(second.get(), 2);
        assert_eq!(library.len(), 2);
        assert!(!library.is_empty());
        assert_eq!(library.get(first).map(Track::title), Some("One"));
        assert_eq!(library.get(second).map(Track::title), Some("Two"));
        assert!(library.contains(first));
    }

    #[test]
    fn iter_yields_insertion_order() {
        let mut library = Library::new();
        let a = library.insert(track("A", "/a.flac"));
        let b = library.insert(track("B", "/b.flac"));
        let c = library.insert(track("C", "/c.flac"));
        let order: Vec<_> = library.iter().map(|(id, _)| id).collect();
        assert_eq!(order, vec![a, b, c]);
    }

    #[test]
    fn duplicate_path_returns_existing_id_and_keeps_first_metadata() {
        let mut library = Library::new();
        let first = library.insert(track("Original", "/music/dup.flac").with_artist("First"));
        let again = library.insert(track("Replacement", "/music/dup.flac").with_artist("Other"));
        assert_eq!(again, first);
        assert_eq!(library.len(), 1);
        let stored = library.get(first).expect("stored track exists");
        // "First wins": existing metadata is never silently replaced.
        assert_eq!(stored.title(), "Original");
        assert_eq!(stored.artist(), Some("First"));
    }

    #[test]
    fn dedupe_uses_path_value_equality() {
        let mut library = Library::new();
        let plain = library.insert(track("Plain", "/music/a.flac"));
        // Redundant `.` segments compare equal under Path equality, so this
        // is a duplicate of the first track ("first wins").
        let dotted = library.insert(track("Dotted", "/music/./a.flac"));
        assert_eq!(dotted, plain);
        // `..` is not resolved (no filesystem access) and case is never
        // folded, so these are different keys under the documented rule.
        library.insert(track("Parented", "/music/sub/../a.flac"));
        library.insert(track("Cased", "/music/A.flac"));
        assert_eq!(library.len(), 3);
        assert_eq!(library.get(plain).map(Track::title), Some("Plain"));
    }

    #[test]
    fn id_for_path_finds_stored_identity() {
        let mut library = Library::new();
        let id = library.insert(track("A", "/music/a.flac"));
        assert_eq!(library.id_for_path(Path::new("/music/a.flac")), Some(id));
        // Same identity under Path equality (`.` segments ignored).
        assert_eq!(library.id_for_path(Path::new("/music/./a.flac")), Some(id));
        assert_eq!(library.id_for_path(Path::new("/music/missing.flac")), None);
    }

    #[test]
    fn update_metadata_keeps_id_and_requires_same_path() {
        let mut library = Library::new();
        let id = library.insert(track("Old Title", "/music/a.flac").with_artist("Old Artist"));

        // Refresh for the same path: same ID, new metadata.
        let refreshed = track("New Title", "/music/a.flac").with_artist("New Artist");
        assert!(library.update_metadata(id, refreshed));
        let stored = library.get(id).expect("still stored");
        assert_eq!(stored.title(), "New Title");
        assert_eq!(stored.artist(), Some("New Artist"));
        assert_eq!(library.id_for_path(Path::new("/music/a.flac")), Some(id));

        // A different path must never hijack the ID.
        assert!(!library.update_metadata(id, track("Hijack", "/music/other.flac")));
        assert_eq!(library.get(id).map(Track::title), Some("New Title"));

        // Unknown IDs are a no-op.
        let unknown = flux_core::TrackId::new(999);
        assert!(!library.update_metadata(unknown, track("X", "/music/a.flac")));
        assert_eq!(library.len(), 1);
    }

    #[test]
    fn remove_returns_track_preserves_order_and_never_reuses_ids() {
        let mut library = Library::new();
        let a = library.insert(track("A", "/a.flac"));
        let b = library.insert(track("B", "/b.flac"));
        let c = library.insert(track("C", "/c.flac"));

        let removed = library.remove(b);
        assert_eq!(removed.as_ref().map(Track::title), Some("B"));
        assert_eq!(library.get(b), None);
        assert!(!library.contains(b));
        assert_eq!(library.remove(b), None, "removing twice finds nothing");

        let order: Vec<_> = library.iter().map(|(id, _)| id).collect();
        assert_eq!(order, vec![a, c]);

        // The next insert continues the sequence instead of reusing b's ID.
        let d = library.insert(track("D", "/d.flac"));
        assert_eq!(d.get(), 4);
    }

    #[test]
    fn reinserting_a_removed_path_creates_a_new_identity() {
        // Documented identity policy: removal retires an ID permanently,
        // even for the same path.
        let mut library = Library::new();
        let first = library.insert(track("A", "/a.flac"));
        assert!(library.remove(first).is_some());
        let second = library.insert(track("A again", "/a.flac"));
        assert_ne!(second, first);
        assert_eq!(second.get(), first.get() + 1);
        assert_eq!(library.get(first), None);
    }

    #[test]
    fn search_matches_title_artist_album_case_insensitively() {
        let mut library = Library::new();
        let blue = library.insert(
            track("Blue Monday", "/1.flac")
                .with_artist("New Order")
                .with_album("Power"),
        );
        let true_faith = library.insert(
            track("True Faith", "/2.flac")
                .with_artist("new order")
                .with_album("Substance"),
        );
        let other = library.insert(track("Unrelated", "/3.flac"));

        let by_title: Vec<_> = library.search("MONDAY").map(|(id, _)| id).collect();
        assert_eq!(by_title, vec![blue]);

        let by_artist: Vec<_> = library.search("New Order").map(|(id, _)| id).collect();
        assert_eq!(by_artist, vec![blue, true_faith]);

        let by_album: Vec<_> = library.search("substance").map(|(id, _)| id).collect();
        assert_eq!(by_album, vec![true_faith]);

        // A track without artist/album can still match by title.
        let by_plain_title: Vec<_> = library.search("unrelated").map(|(id, _)| id).collect();
        assert_eq!(by_plain_title, vec![other]);

        assert_eq!(library.search("nothing matches this").count(), 0);
    }

    #[test]
    fn search_trims_query_and_empty_query_matches_everything() {
        let mut library = Library::new();
        library.insert(track("A", "/a.flac"));
        library.insert(track("B", "/b.flac"));
        assert_eq!(library.search("").count(), 2);
        assert_eq!(library.search("   ").count(), 2);
        assert_eq!(library.search("  a  ").count(), 1);
    }

    /// The derived indexes must mirror the authoritative `tracks` Vec
    /// exactly: same cardinality, and every entry reachable by ID and by
    /// path at its Vec position (through both the raw indexes and the
    /// public lookups built on them).
    fn assert_indexes_consistent(library: &Library) {
        assert_eq!(library.by_id.len(), library.tracks.len(), "by_id size");
        assert_eq!(library.by_path.len(), library.tracks.len(), "by_path size");
        for (index, (id, track)) in library.tracks.iter().enumerate() {
            assert_eq!(library.by_id.get(id), Some(&index), "by_id[{id}]");
            assert_eq!(
                library.by_path.get(track.source().path()),
                Some(id),
                "by_path[{id}]"
            );
            assert_eq!(library.get(*id), Some(track), "get({id})");
            assert_eq!(
                library.id_for_path(track.source().path()),
                Some(*id),
                "id_for_path for {id}"
            );
        }
    }

    #[test]
    fn update_metadata_with_respelled_equal_path_keeps_path_index_exact() {
        let mut library = Library::new();
        let id = library.insert(track("Old", "/music/a.flac"));
        // Equal under Path equality (`.` normalized away), differently
        // spelled: the update is accepted and the stored path adopts
        // the replacement's spelling.
        assert!(library.update_metadata(id, track("New", "/music/./a.flac")));
        assert_eq!(
            library.get(id).expect("stored").source().path(),
            Path::new("/music/./a.flac")
        );
        // Both spellings resolve through the path index.
        assert_eq!(library.id_for_path(Path::new("/music/a.flac")), Some(id));
        assert_eq!(library.id_for_path(Path::new("/music/./a.flac")), Some(id));
        assert_indexes_consistent(&library);

        // Removing afterwards frees the path for a fresh identity.
        assert!(library.remove(id).is_some());
        assert_eq!(library.id_for_path(Path::new("/music/a.flac")), None);
        assert_indexes_consistent(&library);
    }

    #[test]
    fn failed_mutations_leave_indexes_unchanged() {
        let mut library = Library::new();
        let a = library.insert(track("A", "/a.flac").with_artist("Artist"));
        let b = library.insert(track("B", "/b.flac"));
        let before: Vec<_> = library
            .iter()
            .map(|(id, t)| (id, t.title().to_owned()))
            .collect();

        // Every failing/no-op mutation shape the API has.
        assert_eq!(
            library.insert(track("A2", "/a.flac")),
            a,
            "duplicate insert"
        );
        assert!(!library.update_metadata(a, track("Hijack", "/b.flac")));
        assert!(!library.update_metadata(TrackId::new(999), track("X", "/a.flac")));
        assert_eq!(library.remove(TrackId::new(999)), None);

        let after: Vec<_> = library
            .iter()
            .map(|(id, t)| (id, t.title().to_owned()))
            .collect();
        assert_eq!(before, after, "records untouched by failed mutations");
        assert_eq!(library.get(a).map(Track::title), Some("A"));
        assert_eq!(library.id_for_path(Path::new("/b.flac")), Some(b));
        assert_indexes_consistent(&library);
    }

    #[test]
    fn from_parts_builds_indexes_including_retired_id_gaps() {
        // The store hands `from_parts` validated records whose IDs may
        // have gaps (retired IDs of long-removed tracks).
        let mut library = Library::from_parts(
            vec![
                (TrackId::new(2), track("Two", "/two.flac")),
                (TrackId::new(5), track("Five", "/five.flac")),
            ],
            6,
        );
        assert_eq!(library.get(TrackId::new(2)).map(Track::title), Some("Two"));
        assert_eq!(library.get(TrackId::new(5)).map(Track::title), Some("Five"));
        assert_eq!(library.get(TrackId::new(1)), None);
        assert_eq!(library.get(TrackId::new(3)), None);
        assert_eq!(
            library.id_for_path(Path::new("/five.flac")),
            Some(TrackId::new(5))
        );
        assert_indexes_consistent(&library);

        // Allocation continues from the restored counter, never into a gap.
        assert_eq!(library.insert(track("Six", "/six.flac")), TrackId::new(6));
        assert_indexes_consistent(&library);
    }

    /// Naive linear-scan oracle implementing the documented `Library`
    /// semantics the way the pre-index implementation did. The indexed
    /// implementation must be indistinguishable from it through the
    /// public API.
    struct ReferenceLibrary {
        tracks: Vec<(TrackId, Track)>,
        next_id: u64,
    }

    impl ReferenceLibrary {
        fn new() -> Self {
            Self {
                tracks: Vec::new(),
                next_id: 1,
            }
        }

        fn insert(&mut self, track: Track) -> TrackId {
            if let Some((id, _)) = self
                .tracks
                .iter()
                .find(|(_, t)| t.source().path() == track.source().path())
            {
                return *id;
            }
            let id = TrackId::new(self.next_id);
            self.next_id += 1;
            self.tracks.push((id, track));
            id
        }

        fn update_metadata(&mut self, id: TrackId, track: Track) -> bool {
            let Some((_, stored)) = self.tracks.iter_mut().find(|(tid, _)| *tid == id) else {
                return false;
            };
            if stored.source().path() != track.source().path() {
                return false;
            }
            *stored = track;
            true
        }

        fn remove(&mut self, id: TrackId) -> Option<String> {
            let position = self.tracks.iter().position(|(tid, _)| *tid == id)?;
            Some(self.tracks.remove(position).1.title().to_owned())
        }
    }

    fn pick_id(issued: &[TrackId], rand: &mut impl FnMut() -> usize) -> TrackId {
        if issued.is_empty() || rand().is_multiple_of(8) {
            TrackId::new(10_000 + (rand() % 100) as u64)
        } else {
            issued[rand() % issued.len()]
        }
    }

    #[test]
    fn indexed_library_matches_linear_reference_model() {
        // Path spellings chosen to stress the equality rule: `.` is
        // normalized away, `..` is not resolved, case is never folded.
        const PATHS: &[&str] = &[
            "/m/a.flac",
            "/m/./a.flac",
            "/m/sub/../a.flac",
            "/m/A.flac",
            "/m/b.flac",
            "/m/sub/c.flac",
            "/m/sub/./c.flac",
            "/x.flac",
        ];
        let mut library = Library::new();
        let mut model = ReferenceLibrary::new();
        let mut issued: Vec<TrackId> = Vec::new();
        // Fixed-seed LCG: the operation sequence is fully deterministic,
        // so any failure reproduces exactly.
        let mut rng_state = 0x243F_6A88_85A3_08D3u64;
        let mut rand = move || {
            rng_state = rng_state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (rng_state >> 33) as usize
        };

        for step in 0..600 {
            let title = format!("T{step}");
            match rand() % 100 {
                0..=39 => {
                    let path = PATHS[rand() % PATHS.len()];
                    let got = library.insert(track(&title, path));
                    let want = model.insert(track(&title, path));
                    assert_eq!(got, want, "insert id at step {step}");
                    issued.push(got);
                }
                40..=64 => {
                    let id = pick_id(&issued, &mut rand);
                    let path = PATHS[rand() % PATHS.len()];
                    let got = library.update_metadata(id, track(&title, path));
                    let want = model.update_metadata(id, track(&title, path));
                    assert_eq!(got, want, "update_metadata at step {step}");
                }
                65..=84 => {
                    let id = pick_id(&issued, &mut rand);
                    let got = library.remove(id).map(|t| t.title().to_owned());
                    let want = model.remove(id);
                    assert_eq!(got, want, "remove at step {step}");
                }
                _ => {}
            }

            // Full observable-state equivalence after every step.
            assert_eq!(library.len(), model.tracks.len(), "len at step {step}");
            let got: Vec<_> = library
                .iter()
                .map(|(id, t)| (id, t.title().to_owned(), t.source().path().to_owned()))
                .collect();
            let want: Vec<_> = model
                .tracks
                .iter()
                .map(|(id, t)| (*id, t.title().to_owned(), t.source().path().to_owned()))
                .collect();
            assert_eq!(got, want, "insertion-ordered contents at step {step}");
            for path in PATHS {
                let path = Path::new(path);
                let want = model
                    .tracks
                    .iter()
                    .find(|(_, t)| t.source().path() == path)
                    .map(|(id, _)| *id);
                assert_eq!(
                    library.id_for_path(path),
                    want,
                    "id_for_path at step {step}"
                );
            }
            assert_indexes_consistent(&library);

            if step % 100 == 99 {
                for query in ["", "t1", "T42", "zzz"] {
                    let got: Vec<_> = library.search(query).map(|(id, _)| id).collect();
                    let needle = query.trim().to_lowercase();
                    let want: Vec<_> = model
                        .tracks
                        .iter()
                        .filter(|(_, t)| {
                            needle.is_empty()
                                || t.title().to_lowercase().contains(&needle)
                                || t.artist()
                                    .is_some_and(|a| a.to_lowercase().contains(&needle))
                                || t.album()
                                    .is_some_and(|a| a.to_lowercase().contains(&needle))
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    assert_eq!(got, want, "search {query:?} at step {step}");
                }
            }
        }
    }
}
