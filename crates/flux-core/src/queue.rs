//! Playback-control vocabulary: repeat modes, playback states, and the
//! play queue.
//!
//! Everything here is pure data and index logic. Nothing in this module
//! decodes or outputs audio; the queue only decides *which* track would be
//! current after each operation.
//!
//! Queue entries have their own stable identities. A queue may contain the
//! same [`TrackId`] more than once, and each occurrence is independently
//! addressable by its [`QueueEntryId`]. This is what lets a user remove or
//! reorder one duplicate without guessing from a track ID or a volatile
//! index.

use std::fmt;

use crate::error::{Error, Result};
use crate::track::TrackId;

/// Opaque, monotonically allocated identity of one queue entry.
///
/// Entry IDs are unique within one queue and are never reused, including
/// after the entry is removed or the queue is cleared. They are persisted
/// with the queue so identities survive restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QueueEntryId(u64);

impl QueueEntryId {
    /// Creates an ID from a raw value. Zero is not a valid persisted ID.
    #[must_use]
    pub fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw underlying value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for QueueEntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One occurrence of a track in the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct QueueEntry {
    id: QueueEntryId,
    track: TrackId,
}

impl QueueEntry {
    /// Creates an entry from its stable identity and track.
    #[must_use]
    pub fn new(id: QueueEntryId, track: TrackId) -> Self {
        Self { id, track }
    }

    /// This entry's stable identity.
    #[must_use]
    pub fn id(self) -> QueueEntryId {
        self.id
    }

    /// The track this entry references.
    #[must_use]
    pub fn track(self) -> TrackId {
        self.track
    }
}

/// Whether and how playback repeats when a track ends or the user skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RepeatMode {
    /// Play each queued track once, then stop at the end of the queue.
    #[default]
    Off,
    /// At the end of the queue, wrap around to the first entry.
    All,
    /// When a track ends on its own, replay the same entry. Explicit
    /// user skips still move forward (see [`Queue::skip_next`]).
    One,
}

/// The state of the playback-control model.
///
/// This describes the *model's* state only: `Playing` means "the model
/// considers a track to be playing". Turning that intent into actual
/// sound is the playback engine's job, elsewhere; the model state alone
/// never implies that a device is sounding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PlaybackState {
    /// Nothing is playing. The queue's current selection may still be set.
    #[default]
    Stopped,
    /// The current entry is considered to be playing.
    Playing,
    /// The current entry is considered paused.
    Paused,
}

/// An ordered list of queue entries with a current-entry selection.
///
/// # Invariant
///
/// A current entry exists **if and only if** the queue is non-empty, and
/// its ID always names an entry in the queue. Reordering and removing
/// unrelated entries never change the current entry's identity.
///
/// The same [`TrackId`] may appear multiple times; entrant identities stay
/// distinct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queue {
    entries: Vec<QueueEntry>,
    current: Option<QueueEntryId>,
    next_entry_id: u64,
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

impl Queue {
    /// Creates an empty queue with no current entry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            current: None,
            next_entry_id: 1,
        }
    }

    /// Rebuilds a queue from persisted entries.
    ///
    /// Returns `None` when the parts violate the queue invariants: zero
    /// IDs, duplicate entry IDs, a counter that would reuse an entry ID,
    /// or a missing/inconsistent current entry.
    #[must_use]
    pub fn from_entries(
        entries: Vec<QueueEntry>,
        next_entry_id: u64,
        current_entry_id: Option<QueueEntryId>,
    ) -> Option<Self> {
        if next_entry_id == 0 {
            return None;
        }
        let mut seen = std::collections::HashSet::with_capacity(entries.len());
        let mut max_id = 0u64;
        for entry in &entries {
            let raw = entry.id.get();
            if raw == 0 || entry.track.get() == 0 || !seen.insert(entry.id) {
                return None;
            }
            max_id = max_id.max(raw);
        }
        if next_entry_id <= max_id {
            return None;
        }
        match current_entry_id {
            None if entries.is_empty() => {}
            None => return None,
            Some(_) if entries.is_empty() => return None,
            Some(current) if !seen.contains(&current) => return None,
            Some(_) => {}
        }
        Some(Self {
            entries,
            current: current_entry_id,
            next_entry_id,
        })
    }

    /// Appends a track to the end of the queue and returns its new entry ID.
    ///
    /// If the queue was empty, the appended entry becomes current.
    pub fn push(&mut self, id: TrackId) -> QueueEntryId {
        let entry = QueueEntry::new(self.allocate_entry_id(), id);
        self.entries.push(entry);
        if self.current.is_none() {
            self.current = Some(entry.id);
        }
        entry.id
    }

    /// Inserts a track immediately after the current entry and returns its
    /// new entry ID. With an empty queue the new entry becomes current.
    ///
    /// The current selection is unchanged.
    pub fn insert_next(&mut self, id: TrackId) -> QueueEntryId {
        let entry = QueueEntry::new(self.allocate_entry_id(), id);
        match self.current_index() {
            Some(index) => self.entries.insert(index + 1, entry),
            None => {
                self.entries.push(entry);
                self.current = Some(entry.id);
            }
        }
        entry.id
    }

    fn allocate_entry_id(&mut self) -> QueueEntryId {
        let raw = self.next_entry_id;
        assert!(
            raw != 0,
            "queue entry identifier exhausted after 2^64 entries"
        );
        self.next_entry_id = raw.wrapping_add(1);
        QueueEntryId::new(raw)
    }

    /// Removes all entries and clears the current selection. The entry-ID
    /// counter is retained, so IDs are never reused.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.current = None;
    }

    /// All entries in queue order.
    #[must_use]
    pub fn entries(&self) -> &[QueueEntry] {
        &self.entries
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the queue holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The raw value the next new entry ID will use.
    #[must_use]
    pub fn next_entry_id(&self) -> u64 {
        self.next_entry_id
    }

    /// The stable identity of the current entry, or `None` when empty.
    #[must_use]
    pub fn current_entry_id(&self) -> Option<QueueEntryId> {
        self.current
    }

    /// The current entry, or `None` when empty.
    #[must_use]
    pub fn current_entry(&self) -> Option<QueueEntry> {
        self.current
            .and_then(|id| self.entries.iter().find(|entry| entry.id == id).copied())
    }

    /// The currently selected track, or `None` when empty.
    #[must_use]
    pub fn current(&self) -> Option<TrackId> {
        self.current_entry().map(QueueEntry::track)
    }

    /// The index of the current entry, or `None` when empty.
    #[must_use]
    pub fn current_index(&self) -> Option<usize> {
        self.current
            .and_then(|id| self.entries.iter().position(|entry| entry.id == id))
    }

    /// The entry with the given identity.
    #[must_use]
    pub fn entry(&self, id: QueueEntryId) -> Option<QueueEntry> {
        self.entries.iter().find(|entry| entry.id == id).copied()
    }

    /// The position of an entry with the given identity.
    #[must_use]
    pub fn entry_index(&self, id: QueueEntryId) -> Option<usize> {
        self.entries.iter().position(|entry| entry.id == id)
    }

    /// The first entry referencing `track`.
    #[must_use]
    pub fn entry_for_track(&self, track: TrackId) -> Option<QueueEntry> {
        self.entries
            .iter()
            .find(|entry| entry.track == track)
            .copied()
    }

    /// Whether at least one entry references `track`.
    #[must_use]
    pub fn contains(&self, id: TrackId) -> bool {
        self.entries.iter().any(|entry| entry.track == id)
    }

    /// Selects an entry by its stable identity.
    ///
    /// # Errors
    ///
    /// [`Error::QueueEntryNotFound`] if the entry is not in the queue.
    pub fn set_current_entry(&mut self, id: QueueEntryId) -> Result<()> {
        if self.entry(id).is_none() {
            return Err(Error::QueueEntryNotFound { id });
        }
        self.current = Some(id);
        Ok(())
    }

    /// Selects the entry at `index` as current.
    ///
    /// # Errors
    ///
    /// [`Error::QueueIndexOutOfBounds`] if `index` is outside the queue.
    pub fn set_current_index(&mut self, index: usize) -> Result<()> {
        let Some(entry) = self.entries.get(index) else {
            return Err(Error::QueueIndexOutOfBounds {
                index,
                len: self.entries.len(),
            });
        };
        self.current = Some(entry.id);
        Ok(())
    }

    /// Selects the **first** entry referencing a track.
    ///
    /// # Errors
    ///
    /// [`Error::TrackNotInQueue`] if no entry references the track.
    pub fn set_current_by_id(&mut self, id: TrackId) -> Result<()> {
        match self.entry_for_track(id) {
            Some(entry) => {
                self.current = Some(entry.id);
                Ok(())
            }
            None => Err(Error::TrackNotInQueue { id }),
        }
    }

    /// Removes one entry by identity and returns it.
    ///
    /// If the current entry was removed, the following entry becomes
    /// current (or the new last entry); an emptied queue has no current.
    /// Removing a non-current entry never changes the current identity.
    pub fn remove_entry(&mut self, id: QueueEntryId) -> Option<QueueEntry> {
        let index = self.entry_index(id)?;
        let removed = self.entries.remove(index);
        if self.current == Some(id) {
            self.current = self
                .entries
                .get(index.min(self.entries.len().saturating_sub(1)))
                .map(|entry| entry.id);
        }
        Some(removed)
    }

    /// Removes **all** occurrences of a track and returns how many entries
    /// were removed.
    ///
    /// If the current entry referenced that track, the entry that takes
    /// its place becomes current (or the new last entry if it was at the
    /// end). Removing other tracks only shifts the index of the unchanged
    /// current entry.
    pub fn remove(&mut self, id: TrackId) -> usize {
        let Some(current) = self.current else {
            return 0;
        };
        let current_index = self
            .entries
            .iter()
            .position(|entry| entry.id == current)
            .unwrap_or(0);
        let removed_before = self.entries[..current_index]
            .iter()
            .filter(|entry| entry.track == id)
            .count();
        let current_removed = self.entries[current_index].track == id;
        let old_len = self.entries.len();
        self.entries.retain(|entry| entry.track != id);
        let removed = old_len - self.entries.len();
        if self.entries.is_empty() {
            self.current = None;
        } else if current_removed {
            let shifted = current_index - removed_before;
            self.current = self
                .entries
                .get(shifted.min(self.entries.len() - 1))
                .map(|entry| entry.id);
        }
        removed
    }

    /// Moves one entry to `target_index` (clamped to the queue bounds).
    /// Returns whether the order actually changed.
    ///
    /// The current entry keeps its identity regardless of where it moves.
    pub fn move_entry(&mut self, id: QueueEntryId, target_index: usize) -> bool {
        let Some(source) = self.entry_index(id) else {
            return false;
        };
        let target = target_index.min(self.entries.len().saturating_sub(1));
        if source == target {
            return false;
        }
        let entry = self.entries.remove(source);
        self.entries.insert(target, entry);
        true
    }

    /// Moves one entry by a relative offset, clamped. Returns whether the
    /// order changed.
    pub fn move_entry_by(&mut self, id: QueueEntryId, delta: isize) -> bool {
        let Some(source) = self.entry_index(id) else {
            return false;
        };
        let target = source.saturating_add_signed(delta);
        self.move_entry(id, target)
    }

    /// Moves one entry to insertion gap `gap`.
    ///
    /// Gaps are the spaces between entries: `0` is before the first entry,
    /// `len()` is after the last, and `i` is between entries `i - 1` and
    /// `i`. Returns `false` for an absent entry, an out-of-range gap, or a
    /// drop that would not change the order.
    pub fn move_entry_to_gap(&mut self, id: QueueEntryId, gap: usize) -> bool {
        if gap > self.entries.len() {
            return false;
        }
        let Some(source) = self.entry_index(id) else {
            return false;
        };
        // Removing the source first shifts every later gap left by one.
        let target = if source < gap { gap - 1 } else { gap };
        self.move_entry(id, target)
    }

    /// Advances the selection as if the current track just ended on its
    /// own, and returns the track that should play next.
    ///
    /// - [`RepeatMode::One`]: the current entry repeats.
    /// - [`RepeatMode::All`]: moves forward, wrapping from the end.
    /// - [`RepeatMode::Off`]: moves forward; at the last entry it returns
    ///   `None` and the selection stays on that entry.
    /// - Empty queue: returns `None`.
    pub fn advance(&mut self, mode: RepeatMode) -> Option<TrackId> {
        let current = self.current_index()?;
        match mode {
            RepeatMode::One => self.entries.get(current).map(|entry| entry.track),
            RepeatMode::Off => {
                let next = current + 1;
                if next < self.entries.len() {
                    self.current = Some(self.entries[next].id);
                    Some(self.entries[next].track)
                } else {
                    None
                }
            }
            RepeatMode::All => {
                let next = (current + 1) % self.entries.len();
                self.current = Some(self.entries[next].id);
                Some(self.entries[next].track)
            }
        }
    }

    /// Moves the selection forward in response to an explicit user skip.
    /// Repeat-one does not pin the selection for explicit skips.
    pub fn skip_next(&mut self, mode: RepeatMode) -> Option<TrackId> {
        let effective = match mode {
            RepeatMode::One => RepeatMode::Off,
            other => other,
        };
        self.advance(effective)
    }

    /// Moves the selection one entry back and returns the selected track.
    /// At the first entry the selection stays and that track is returned.
    pub fn skip_previous(&mut self) -> Option<TrackId> {
        let current = self.current_index()?;
        let previous = current.saturating_sub(1);
        self.current = Some(self.entries[previous].id);
        Some(self.entries[previous].track)
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybackState, Queue, QueueEntry, QueueEntryId, RepeatMode};
    use crate::error::Error;
    use crate::track::TrackId;

    fn tracks(queue: &Queue) -> Vec<u64> {
        queue
            .entries()
            .iter()
            .map(|entry| entry.track().get())
            .collect()
    }

    fn queue_of(raw_ids: &[u64]) -> (Queue, Vec<QueueEntryId>) {
        let mut queue = Queue::new();
        let ids = raw_ids
            .iter()
            .map(|raw| queue.push(TrackId::new(*raw)))
            .collect();
        (queue, ids)
    }

    #[test]
    fn empty_queue_has_no_current_and_navigation_is_none() {
        let mut queue = Queue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.current(), None);
        assert_eq!(queue.current_entry_id(), None);
        assert_eq!(queue.current_index(), None);
        assert_eq!(queue.advance(RepeatMode::Off), None);
        assert_eq!(queue.advance(RepeatMode::All), None);
        assert_eq!(queue.advance(RepeatMode::One), None);
        assert_eq!(queue.skip_next(RepeatMode::All), None);
        assert_eq!(queue.skip_previous(), None);
        assert_eq!(queue.remove(TrackId::new(1)), 0);
        assert_eq!(queue.remove_entry(QueueEntryId::new(1)), None);
    }

    #[test]
    fn first_push_becomes_current_later_pushes_do_not_steal_selection() {
        let mut queue = Queue::new();
        let first = queue.push(TrackId::new(10));
        assert_eq!(queue.current(), Some(TrackId::new(10)));
        assert_eq!(queue.current_entry_id(), Some(first));
        let second = queue.push(TrackId::new(20));
        assert_eq!(queue.current(), Some(TrackId::new(10)));
        assert_eq!(
            queue.entry_for_track(TrackId::new(20)).map(|e| e.id()),
            Some(second)
        );
        assert_eq!(tracks(&queue), vec![10, 20]);
        assert!(queue.contains(TrackId::new(20)));
        assert!(!queue.contains(TrackId::new(99)));
    }

    #[test]
    fn duplicate_tracks_get_distinct_entry_ids() {
        let (queue, ids) = queue_of(&[7, 7, 8, 7]);
        assert_eq!(ids.len(), 4);
        assert_eq!(
            ids.iter()
                .map(|id| id.get())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4
        );
        assert_eq!(tracks(&queue), vec![7, 7, 8, 7]);
        assert_eq!(
            queue.entry_for_track(TrackId::new(7)).map(|e| e.id()),
            Some(ids[0])
        );
    }

    #[test]
    fn insert_next_targets_the_position_after_current() {
        let (mut queue, ids) = queue_of(&[1, 2, 3]);
        assert_eq!(ids.len(), 3);
        let inserted = queue.insert_next(TrackId::new(9));
        assert_eq!(tracks(&queue), vec![1, 9, 2, 3]);
        assert_eq!(queue.current(), Some(TrackId::new(1)));
        assert_eq!(queue.entry_index(inserted), Some(1));

        queue.set_current_entry(ids[2]).expect("select entry");
        let after_last = queue.insert_next(TrackId::new(4));
        assert_eq!(tracks(&queue), vec![1, 9, 2, 3, 4]);
        assert_eq!(queue.entry_index(after_last), Some(4));
        assert_eq!(queue.current(), Some(TrackId::new(3)));
    }

    #[test]
    fn insert_next_into_empty_queue_becomes_current() {
        let mut queue = Queue::new();
        let id = queue.insert_next(TrackId::new(5));
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.current(), Some(TrackId::new(5)));
        assert_eq!(queue.current_entry_id(), Some(id));
    }

    #[test]
    fn clear_resets_selection_and_retires_entry_ids() {
        let (mut queue, ids) = queue_of(&[1, 2, 3]);
        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.current(), None);
        let next = queue.push(TrackId::new(9));
        assert!(next > ids[2], "entry IDs are never reused after clear");
        assert_eq!(queue.current(), Some(TrackId::new(9)));
    }

    #[test]
    fn set_current_index_and_entry_validate_bounds_and_identity() {
        let (mut queue, ids) = queue_of(&[1, 2]);
        assert_eq!(queue.set_current_index(1), Ok(()));
        assert_eq!(queue.current(), Some(TrackId::new(2)));
        assert_eq!(
            queue.set_current_index(2),
            Err(Error::QueueIndexOutOfBounds { index: 2, len: 2 })
        );
        assert_eq!(
            queue.set_current_entry(QueueEntryId::new(999)),
            Err(Error::QueueEntryNotFound {
                id: QueueEntryId::new(999)
            })
        );
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
    }

    #[test]
    fn set_current_by_track_selects_first_occurrence_and_errors_when_absent() {
        let (mut queue, ids) = queue_of(&[1, 2, 1]);
        assert_eq!(queue.set_current_by_id(TrackId::new(1)), Ok(()));
        assert_eq!(queue.current_entry_id(), Some(ids[0]));
        assert_eq!(queue.set_current_by_id(TrackId::new(2)), Ok(()));
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
        assert_eq!(
            queue.set_current_by_id(TrackId::new(42)),
            Err(Error::TrackNotInQueue {
                id: TrackId::new(42)
            })
        );
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
    }

    #[test]
    fn advance_repeat_off_stops_at_end_keeping_selection() {
        let (mut queue, ids) = queue_of(&[1, 2]);
        assert_eq!(queue.advance(RepeatMode::Off), Some(TrackId::new(2)));
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
        assert_eq!(queue.advance(RepeatMode::Off), None);
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
    }

    #[test]
    fn advance_repeat_all_wraps_and_one_repeats_entry() {
        let (mut queue, ids) = queue_of(&[1, 2]);
        assert_eq!(queue.advance(RepeatMode::All), Some(TrackId::new(2)));
        assert_eq!(queue.advance(RepeatMode::All), Some(TrackId::new(1)));
        assert_eq!(queue.current_entry_id(), Some(ids[0]));

        let (mut one, ids) = queue_of(&[7, 8]);
        assert_eq!(one.advance(RepeatMode::One), Some(TrackId::new(7)));
        assert_eq!(one.current_entry_id(), Some(ids[0]));
    }

    #[test]
    fn duplicate_entries_are_navigated_by_position_not_track_id() {
        let (mut queue, ids) = queue_of(&[7, 7, 8]);
        assert_eq!(queue.current_entry_id(), Some(ids[0]));
        assert_eq!(queue.advance(RepeatMode::Off), Some(TrackId::new(7)));
        assert_eq!(
            queue.current_entry_id(),
            Some(ids[1]),
            "the second duplicate, not the first matching track, is selected"
        );
        assert_eq!(queue.advance(RepeatMode::Off), Some(TrackId::new(8)));
        assert_eq!(queue.current_entry_id(), Some(ids[2]));
        assert_eq!(queue.advance(RepeatMode::Off), None);
        assert_eq!(queue.current_entry_id(), Some(ids[2]));
    }

    #[test]
    fn explicit_skip_ignores_repeat_one_pinning() {
        let (mut queue, _) = queue_of(&[1, 2]);
        assert_eq!(queue.skip_next(RepeatMode::One), Some(TrackId::new(2)));
        assert_eq!(queue.skip_next(RepeatMode::One), None);
        assert_eq!(queue.current(), Some(TrackId::new(2)));
        assert_eq!(queue.skip_next(RepeatMode::All), Some(TrackId::new(1)));
    }

    #[test]
    fn skip_previous_moves_back_and_sticks_at_start() {
        let (mut queue, _) = queue_of(&[1, 2, 3]);
        queue.set_current_index(2).expect("in bounds");
        assert_eq!(queue.skip_previous(), Some(TrackId::new(2)));
        assert_eq!(queue.skip_previous(), Some(TrackId::new(1)));
        assert_eq!(queue.skip_previous(), Some(TrackId::new(1)));
        assert_eq!(queue.current_index(), Some(0));
    }

    #[test]
    fn removing_non_current_entry_preserves_current_identity() {
        let (mut queue, ids) = queue_of(&[1, 2, 3]);
        queue.set_current_entry(ids[2]).expect("current");
        assert_eq!(
            queue.remove_entry(ids[0]).map(QueueEntry::track),
            Some(TrackId::new(1))
        );
        assert_eq!(queue.current_entry_id(), Some(ids[2]));
        assert_eq!(tracks(&queue), vec![2, 3]);
        assert_eq!(queue.current_index(), Some(1));
    }

    #[test]
    fn removing_current_selects_following_or_last_or_none() {
        let (mut queue, ids) = queue_of(&[1, 2, 3]);
        queue.set_current_entry(ids[1]).expect("current");
        assert_eq!(
            queue.remove_entry(ids[1]).map(QueueEntry::track),
            Some(TrackId::new(2))
        );
        assert_eq!(queue.current_entry_id(), Some(ids[2]));

        queue.set_current_entry(ids[2]).expect("last current");
        queue.remove_entry(ids[2]).expect("remove last");
        assert_eq!(queue.current_entry_id(), Some(ids[0]));

        queue.remove_entry(ids[0]).expect("remove final");
        assert!(queue.is_empty());
        assert_eq!(queue.current_entry_id(), None);
    }

    #[test]
    fn remove_all_occurrences_adjusts_current_coherently() {
        let (mut queue, ids) = queue_of(&[1, 2, 1, 3, 1]);
        queue.set_current_entry(ids[3]).expect("current 3");
        assert_eq!(queue.remove(TrackId::new(1)), 3);
        assert_eq!(tracks(&queue), vec![2, 3]);
        assert_eq!(queue.current(), Some(TrackId::new(3)));
        assert_eq!(queue.current_index(), Some(1));

        assert_eq!(queue.remove(TrackId::new(3)), 1);
        assert_eq!(queue.remove(TrackId::new(2)), 1);
        assert!(queue.is_empty());
        assert_eq!(queue.current(), None);
    }

    #[test]
    fn moving_entries_preserves_current_identity_and_order() {
        let (mut queue, ids) = queue_of(&[1, 2, 3]);
        queue.set_current_entry(ids[1]).expect("current 2");
        assert!(queue.move_entry(ids[2], 0));
        assert_eq!(tracks(&queue), vec![3, 1, 2]);
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
        assert_eq!(queue.current_index(), Some(2));

        assert!(queue.move_entry_by(ids[1], -2));
        assert_eq!(tracks(&queue), vec![2, 3, 1]);
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
        assert_eq!(queue.current_index(), Some(0));

        assert!(!queue.move_entry_by(ids[1], -5), "clamped at start");
        assert!(!queue.move_entry(QueueEntryId::new(999), 1));
    }

    #[test]
    fn moving_to_insertion_gaps_handles_beginning_middle_and_end() {
        let (mut queue, ids) = queue_of(&[1, 2, 3, 4]);
        queue.set_current_entry(ids[1]).expect("current 2");

        assert!(queue.move_entry_to_gap(ids[0], 4));
        assert_eq!(tracks(&queue), vec![2, 3, 4, 1]);
        assert_eq!(queue.current_entry_id(), Some(ids[1]));

        assert!(queue.move_entry_to_gap(ids[3], 0));
        assert_eq!(tracks(&queue), vec![4, 2, 3, 1]);
        assert_eq!(queue.current_entry_id(), Some(ids[1]));

        assert!(queue.move_entry_to_gap(ids[0], 2));
        assert_eq!(tracks(&queue), vec![4, 2, 1, 3]);
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
        assert!(!queue.move_entry_to_gap(ids[0], 2), "same final position");
        assert!(!queue.move_entry_to_gap(ids[0], 5), "gap past the end");
        assert!(!queue.move_entry_to_gap(QueueEntryId::new(999), 1));
    }

    #[test]
    fn moving_a_duplicate_to_a_gap_keeps_occurrences_distinct() {
        let (mut queue, ids) = queue_of(&[7, 7, 8]);
        queue
            .set_current_entry(ids[1])
            .expect("current second duplicate");
        assert!(queue.move_entry_to_gap(ids[2], 0));
        assert_eq!(tracks(&queue), vec![8, 7, 7]);
        assert_eq!(queue.entries()[1].id(), ids[0]);
        assert_eq!(queue.entries()[2].id(), ids[1]);
        assert_eq!(queue.current_entry_id(), Some(ids[1]));
    }

    #[test]
    fn from_entries_validates_identity_and_selection() {
        let entries = vec![
            QueueEntry::new(QueueEntryId::new(2), TrackId::new(10)),
            QueueEntry::new(QueueEntryId::new(5), TrackId::new(10)),
        ];
        let queue = Queue::from_entries(entries.clone(), 6, Some(QueueEntryId::new(5)))
            .expect("valid queue");
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.current(), Some(TrackId::new(10)));

        assert!(Queue::from_entries(entries.clone(), 5, Some(QueueEntryId::new(5))).is_none());
        assert!(Queue::from_entries(entries.clone(), 6, None).is_none());
        assert!(
            Queue::from_entries(
                vec![QueueEntry::new(QueueEntryId::new(0), TrackId::new(1))],
                2,
                Some(QueueEntryId::new(0))
            )
            .is_none()
        );
        assert!(
            Queue::from_entries(
                vec![
                    QueueEntry::new(QueueEntryId::new(1), TrackId::new(1)),
                    QueueEntry::new(QueueEntryId::new(1), TrackId::new(2))
                ],
                3,
                Some(QueueEntryId::new(1))
            )
            .is_none()
        );
        assert!(Queue::from_entries(Vec::new(), 1, None).is_some());
        assert!(Queue::from_entries(Vec::new(), 0, None).is_none());
    }

    #[test]
    fn defaults_are_stopped_and_repeat_off() {
        assert_eq!(PlaybackState::default(), PlaybackState::Stopped);
        assert_eq!(RepeatMode::default(), RepeatMode::Off);
        assert_eq!(Queue::default(), Queue::new());
    }
}
