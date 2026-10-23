//! Playback-control vocabulary: repeat modes, playback states, and the
//! play queue.
//!
//! Everything here is pure data and index logic. Nothing in this module
//! decodes or outputs audio; the queue only decides *which* track would be
//! current after each operation.

use crate::error::{Error, Result};
use crate::track::TrackId;

/// Whether and how playback repeats when a track ends or the user skips.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RepeatMode {
    /// Play each queued track once, then stop at the end of the queue.
    #[default]
    Off,
    /// At the end of the queue, wrap around to the first track.
    All,
    /// When a track ends on its own, replay the same track. Explicit
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
    /// The current track is considered to be playing.
    Playing,
    /// The current track is considered paused.
    Paused,
}

/// An ordered list of track IDs with a current-item selection.
///
/// # Invariant
///
/// The queue maintains one invariant at all times: a current item exists
/// **if and only if** the queue is non-empty, and its index always points
/// inside the queue. In particular:
///
/// - The first track pushed onto an empty queue becomes current.
/// - Clearing the queue clears the selection.
/// - Removing tracks adjusts the selection as documented on
///   [`Queue::remove`].
/// - Navigation methods never panic and never leave the index out of
///   bounds; on an empty queue they simply return `None`.
///
/// The same [`TrackId`] may appear multiple times (for example, a song
/// intentionally queued twice).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Queue {
    items: Vec<TrackId>,
    current: Option<usize>,
}

impl Queue {
    /// Creates an empty queue with no current item.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a track to the end of the queue.
    ///
    /// If the queue was empty, the appended track becomes the current item.
    pub fn push(&mut self, id: TrackId) {
        self.items.push(id);
        if self.current.is_none() {
            self.current = Some(0);
        }
    }

    /// Removes all tracks and clears the current selection.
    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
    }

    /// Removes **all** occurrences of a track (for example because the
    /// track was deleted from the library) and returns how many queue
    /// entries were removed.
    ///
    /// Selection adjustment: entries before the current item shift the
    /// index left; if the current entry itself is removed, the entry that
    /// takes its place becomes current (or the new last entry if the
    /// current entry was at the end); if the queue becomes empty, there
    /// is no current item.
    pub fn remove(&mut self, id: TrackId) -> usize {
        let Some(current) = self.current else {
            return 0;
        };
        let removed_before = self.items[..current]
            .iter()
            .filter(|item| **item == id)
            .count();
        let current_removed = self.items[current] == id;
        let old_len = self.items.len();
        self.items.retain(|item| *item != id);
        let removed = old_len - self.items.len();
        if self.items.is_empty() {
            self.current = None;
        } else if current_removed {
            let shifted = current - removed_before;
            self.current = Some(shifted.min(self.items.len() - 1));
        } else {
            self.current = Some(current - removed_before);
        }
        removed
    }

    /// The number of tracks in the queue.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the queue holds no tracks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Whether the queue contains at least one occurrence of a track.
    #[must_use]
    pub fn contains(&self, id: TrackId) -> bool {
        self.items.contains(&id)
    }

    /// All queued track IDs, in queue order.
    #[must_use]
    pub fn items(&self) -> &[TrackId] {
        &self.items
    }

    /// The currently selected track, or `None` on an empty queue.
    #[must_use]
    pub fn current(&self) -> Option<TrackId> {
        self.current.map(|index| self.items[index])
    }

    /// The index of the current selection, or `None` on an empty queue.
    #[must_use]
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// Selects the track at `index` as current.
    ///
    /// # Errors
    ///
    /// Returns [`Error::QueueIndexOutOfBounds`] (leaving the selection
    /// unchanged) if `index` is not a valid position in the queue.
    pub fn set_current_index(&mut self, index: usize) -> Result<()> {
        if index >= self.items.len() {
            return Err(Error::QueueIndexOutOfBounds {
                index,
                len: self.items.len(),
            });
        }
        self.current = Some(index);
        Ok(())
    }

    /// Selects the **first** occurrence of a track as current.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TrackNotInQueue`] (leaving the selection
    /// unchanged) if the track is not in the queue.
    pub fn set_current_by_id(&mut self, id: TrackId) -> Result<()> {
        match self.items.iter().position(|item| *item == id) {
            Some(index) => {
                self.current = Some(index);
                Ok(())
            }
            None => Err(Error::TrackNotInQueue { id }),
        }
    }

    /// Advances the selection as if the current track just ended on its
    /// own, and returns the track that should play next.
    ///
    /// - [`RepeatMode::One`]: the current track repeats.
    /// - [`RepeatMode::All`]: moves forward, wrapping from the end back to
    ///   the first track.
    /// - [`RepeatMode::Off`]: moves forward; at the last track it returns
    ///   `None` (nothing left to play) and the selection stays on that
    ///   last track.
    /// - Empty queue: returns `None`.
    pub fn advance(&mut self, mode: RepeatMode) -> Option<TrackId> {
        let current = self.current?;
        match mode {
            RepeatMode::One => Some(self.items[current]),
            RepeatMode::Off => {
                let next = current + 1;
                if next < self.items.len() {
                    self.current = Some(next);
                    Some(self.items[next])
                } else {
                    None
                }
            }
            RepeatMode::All => {
                let next = (current + 1) % self.items.len();
                self.current = Some(next);
                Some(self.items[next])
            }
        }
    }

    /// Moves the selection forward in response to an explicit user skip,
    /// and returns the newly selected track.
    ///
    /// Unlike [`Queue::advance`], [`RepeatMode::One`] does **not** pin the
    /// selection: an explicit skip always tries to move. At the last track
    /// with `Off`/`One`, returns `None` and the selection is unchanged;
    /// with [`RepeatMode::All`] it wraps. On an empty queue, returns
    /// `None`.
    pub fn skip_next(&mut self, mode: RepeatMode) -> Option<TrackId> {
        let effective = match mode {
            RepeatMode::One => RepeatMode::Off,
            other => other,
        };
        self.advance(effective)
    }

    /// Moves the selection one step back and returns the selected track.
    ///
    /// At the first track the selection stays where it is and that first
    /// track is returned. On an empty queue, returns `None`.
    pub fn skip_previous(&mut self) -> Option<TrackId> {
        let current = self.current?;
        let previous = current.saturating_sub(1);
        self.current = Some(previous);
        Some(self.items[previous])
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybackState, Queue, RepeatMode};
    use crate::error::Error;
    use crate::track::TrackId;

    fn queue_of(raw_ids: &[u64]) -> Queue {
        let mut queue = Queue::new();
        for raw in raw_ids {
            queue.push(TrackId::new(*raw));
        }
        queue
    }

    #[test]
    fn empty_queue_has_no_current_and_navigation_is_none() {
        let mut queue = Queue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert_eq!(queue.current(), None);
        assert_eq!(queue.current_index(), None);
        assert_eq!(queue.advance(RepeatMode::Off), None);
        assert_eq!(queue.advance(RepeatMode::All), None);
        assert_eq!(queue.advance(RepeatMode::One), None);
        assert_eq!(queue.skip_next(RepeatMode::All), None);
        assert_eq!(queue.skip_previous(), None);
        assert_eq!(queue.remove(TrackId::new(1)), 0);
    }

    #[test]
    fn first_push_becomes_current_later_pushes_do_not_steal_selection() {
        let mut queue = Queue::new();
        queue.push(TrackId::new(10));
        assert_eq!(queue.current(), Some(TrackId::new(10)));
        assert_eq!(queue.current_index(), Some(0));
        queue.push(TrackId::new(20));
        assert_eq!(queue.current(), Some(TrackId::new(10)));
        assert_eq!(queue.items(), &[TrackId::new(10), TrackId::new(20)]);
        assert!(queue.contains(TrackId::new(20)));
        assert!(!queue.contains(TrackId::new(99)));
    }

    #[test]
    fn clear_resets_selection() {
        let mut queue = queue_of(&[1, 2, 3]);
        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.current(), None);
        // Invariant restored: the next push is a "first" push again.
        queue.push(TrackId::new(9));
        assert_eq!(queue.current(), Some(TrackId::new(9)));
    }

    #[test]
    fn set_current_index_checks_bounds() {
        let mut queue = queue_of(&[1, 2]);
        assert_eq!(queue.set_current_index(1), Ok(()));
        assert_eq!(queue.current(), Some(TrackId::new(2)));
        assert_eq!(
            queue.set_current_index(2),
            Err(Error::QueueIndexOutOfBounds { index: 2, len: 2 })
        );
        // Failed selection leaves the previous current untouched.
        assert_eq!(queue.current(), Some(TrackId::new(2)));

        let mut empty = Queue::new();
        assert_eq!(
            empty.set_current_index(0),
            Err(Error::QueueIndexOutOfBounds { index: 0, len: 0 })
        );
    }

    #[test]
    fn set_current_by_id_selects_first_occurrence_and_errors_when_absent() {
        let mut queue = queue_of(&[1, 2, 1]);
        assert_eq!(queue.set_current_by_id(TrackId::new(1)), Ok(()));
        assert_eq!(queue.current_index(), Some(0));
        assert_eq!(queue.set_current_by_id(TrackId::new(2)), Ok(()));
        assert_eq!(queue.current_index(), Some(1));
        assert_eq!(
            queue.set_current_by_id(TrackId::new(42)),
            Err(Error::TrackNotInQueue {
                id: TrackId::new(42)
            })
        );
        assert_eq!(queue.current_index(), Some(1));
    }

    #[test]
    fn advance_repeat_off_stops_at_end_keeping_selection() {
        let mut queue = queue_of(&[1, 2]);
        assert_eq!(queue.advance(RepeatMode::Off), Some(TrackId::new(2)));
        assert_eq!(queue.advance(RepeatMode::Off), None);
        // Selection remains on the last track after the queue is exhausted.
        assert_eq!(queue.current(), Some(TrackId::new(2)));
    }

    #[test]
    fn advance_repeat_all_wraps_around() {
        let mut queue = queue_of(&[1, 2]);
        assert_eq!(queue.advance(RepeatMode::All), Some(TrackId::new(2)));
        assert_eq!(queue.advance(RepeatMode::All), Some(TrackId::new(1)));
        // A one-item queue wraps onto itself.
        let mut single = queue_of(&[7]);
        assert_eq!(single.advance(RepeatMode::All), Some(TrackId::new(7)));
    }

    #[test]
    fn advance_repeat_one_replays_current() {
        let mut queue = queue_of(&[1, 2]);
        assert_eq!(queue.advance(RepeatMode::One), Some(TrackId::new(1)));
        assert_eq!(queue.current_index(), Some(0));
    }

    #[test]
    fn skip_next_ignores_repeat_one_pinning() {
        let mut queue = queue_of(&[1, 2]);
        assert_eq!(queue.skip_next(RepeatMode::One), Some(TrackId::new(2)));
        assert_eq!(queue.skip_next(RepeatMode::One), None);
        assert_eq!(queue.current(), Some(TrackId::new(2)));
        assert_eq!(queue.skip_next(RepeatMode::All), Some(TrackId::new(1)));
    }

    #[test]
    fn skip_previous_moves_back_and_sticks_at_start() {
        let mut queue = queue_of(&[1, 2, 3]);
        queue.set_current_index(2).expect("in bounds");
        assert_eq!(queue.skip_previous(), Some(TrackId::new(2)));
        assert_eq!(queue.skip_previous(), Some(TrackId::new(1)));
        // Already at the first track: stays and returns it again.
        assert_eq!(queue.skip_previous(), Some(TrackId::new(1)));
        assert_eq!(queue.current_index(), Some(0));
    }

    #[test]
    fn remove_before_current_shifts_index() {
        let mut queue = queue_of(&[1, 2, 3]);
        queue.set_current_index(2).expect("in bounds");
        assert_eq!(queue.remove(TrackId::new(1)), 1);
        assert_eq!(queue.items(), &[TrackId::new(2), TrackId::new(3)]);
        assert_eq!(queue.current(), Some(TrackId::new(3)));
        assert_eq!(queue.current_index(), Some(1));
    }

    #[test]
    fn remove_current_selects_replacement_or_new_last() {
        // Removing the current middle item selects the item that took its place.
        let mut queue = queue_of(&[1, 2, 3]);
        queue.set_current_index(1).expect("in bounds");
        assert_eq!(queue.remove(TrackId::new(2)), 1);
        assert_eq!(queue.current(), Some(TrackId::new(3)));

        // Removing the current last item selects the new last item.
        let mut queue = queue_of(&[1, 2, 3]);
        queue.set_current_index(2).expect("in bounds");
        assert_eq!(queue.remove(TrackId::new(3)), 1);
        assert_eq!(queue.current(), Some(TrackId::new(2)));
    }

    #[test]
    fn remove_all_occurrences_and_to_empty() {
        let mut queue = queue_of(&[1, 2, 1, 3, 1]);
        queue.set_current_index(3).expect("in bounds"); // current = 3
        assert_eq!(queue.remove(TrackId::new(1)), 3);
        assert_eq!(queue.items(), &[TrackId::new(2), TrackId::new(3)]);
        // Two removed entries were before the current one.
        assert_eq!(queue.current(), Some(TrackId::new(3)));
        assert_eq!(queue.current_index(), Some(1));

        assert_eq!(queue.remove(TrackId::new(3)), 1);
        assert_eq!(queue.remove(TrackId::new(2)), 1);
        assert!(queue.is_empty());
        assert_eq!(queue.current(), None);
    }

    #[test]
    fn defaults_are_stopped_and_repeat_off() {
        assert_eq!(PlaybackState::default(), PlaybackState::Stopped);
        assert_eq!(RepeatMode::default(), RepeatMode::Off);
        assert_eq!(Queue::default(), Queue::new());
    }
}
