//! Playback-control model for Flux.
//!
//! [`Player`] is a pure, synchronous state machine that coordinates a
//! [`Queue`], a [`PlaybackState`], and a [`RepeatMode`] from `flux-core`.
//! It decides *which* track is current and whether the model considers it
//! playing — nothing more. This crate deliberately contains **no** audio
//! output, decoding, threading, async machinery, or backend selection, and
//! `Playing` never means that sound is actually being produced. A future
//! milestone will drive a real backend from this model's decisions.

use flux_core::{Error, PlaybackState, Queue, RepeatMode, Result, TrackId};

/// A playback-control model with a single queue and state.
///
/// The player owns exactly one [`Queue`] and derives the current track
/// from it; there is no second cached copy of queue or selection state
/// that could drift out of sync.
///
/// # Transition behavior
///
/// - [`play`](Player::play) requires a non-empty queue
///   ([`Error::EmptyQueue`] otherwise, state unchanged) and is idempotent
///   while already playing.
/// - [`pause`](Player::pause) only affects the `Playing` state; pausing
///   while `Paused` or `Stopped` is a documented no-op.
/// - [`stop`](Player::stop) returns to `Stopped` from any state and
///   retains the queue and its selection.
/// - Skips and track-ended handling delegate index movement to the queue
///   so its boundary rules apply. When navigation finds nothing to play
///   (end of queue without [`RepeatMode::All`], or an empty queue), the
///   player moves to `Stopped` and returns `None`. Successful navigation
///   never changes `Playing`/`Paused` by itself.
/// - Because callers can mutate the queue directly via
///   [`queue_mut`](Player::queue_mut), the player defends itself against
///   an externally emptied queue: the next navigation resolves to
///   `Stopped`, and [`play`](Player::play) reports [`Error::EmptyQueue`]
///   instead of claiming to play nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Player {
    queue: Queue,
    state: PlaybackState,
    repeat: RepeatMode,
}

impl Player {
    /// Creates a stopped player with an empty queue and repeat off.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The model's current playback state.
    #[must_use]
    pub fn state(&self) -> PlaybackState {
        self.state
    }

    /// The active repeat mode.
    #[must_use]
    pub fn repeat_mode(&self) -> RepeatMode {
        self.repeat
    }

    /// Sets the repeat mode used by skips and track-ended handling.
    pub fn set_repeat_mode(&mut self, mode: RepeatMode) {
        self.repeat = mode;
    }

    /// The player's queue.
    #[must_use]
    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    /// Mutable access to the player's queue (enqueue, reorder, clear…).
    ///
    /// The queue maintains its own selection invariant, so direct mutation
    /// cannot corrupt the player; see the type-level docs for how an
    /// externally emptied queue is handled.
    pub fn queue_mut(&mut self) -> &mut Queue {
        &mut self.queue
    }

    /// The currently selected track, if the queue is non-empty.
    #[must_use]
    pub fn current_track(&self) -> Option<TrackId> {
        self.queue.current()
    }

    /// Starts (or resumes) playback of the currently selected track.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EmptyQueue`] — leaving the state unchanged — if
    /// there is nothing queued to play.
    pub fn play(&mut self) -> Result<()> {
        if self.queue.is_empty() {
            return Err(Error::EmptyQueue);
        }
        self.state = PlaybackState::Playing;
        Ok(())
    }

    /// Selects a specific queued track and starts playing it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TrackNotInQueue`] — leaving selection and state
    /// unchanged — if the track is not in the queue.
    pub fn play_track(&mut self, id: TrackId) -> Result<()> {
        self.queue.set_current_by_id(id)?;
        self.state = PlaybackState::Playing;
        Ok(())
    }

    /// Pauses playback. Only `Playing → Paused` changes anything; pausing
    /// while already paused or stopped is a no-op.
    pub fn pause(&mut self) {
        if self.state == PlaybackState::Playing {
            self.state = PlaybackState::Paused;
        }
    }

    /// Stops playback from any state. The queue and its current selection
    /// are retained, so a later [`play`](Player::play) resumes from the
    /// same selected track.
    pub fn stop(&mut self) {
        self.state = PlaybackState::Stopped;
    }

    /// Explicit user skip forward; returns the newly selected track.
    ///
    /// Repeat-one does not pin the selection for explicit skips (see
    /// [`Queue::skip_next`]). If there is nothing to skip to, the player
    /// stops and returns `None`.
    pub fn next_track(&mut self) -> Option<TrackId> {
        match self.queue.skip_next(self.repeat) {
            Some(id) => Some(id),
            None => {
                self.state = PlaybackState::Stopped;
                None
            }
        }
    }

    /// Explicit user skip backward; returns the newly selected track.
    ///
    /// At the first track the selection stays on it (see
    /// [`Queue::skip_previous`]). On an empty queue the player stops and
    /// returns `None`.
    pub fn previous_track(&mut self) -> Option<TrackId> {
        match self.queue.skip_previous() {
            Some(id) => Some(id),
            None => {
                self.state = PlaybackState::Stopped;
                None
            }
        }
    }

    /// Handles the current track ending on its own; returns the track
    /// that should play next.
    ///
    /// Only meaningful while `Playing`: in any other state the event is
    /// ignored (returns `None`, changes nothing), so stale backend events
    /// can never resurrect a paused or stopped player. While playing,
    /// repeat-one replays the same track, repeat-all wraps, and with
    /// repeat off the player stops at the end of the queue.
    pub fn on_track_ended(&mut self) -> Option<TrackId> {
        if self.state != PlaybackState::Playing {
            return None;
        }
        match self.queue.advance(self.repeat) {
            Some(id) => Some(id),
            None => {
                self.state = PlaybackState::Stopped;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use flux_core::{Error, PlaybackState, RepeatMode, TrackId};

    use super::Player;

    fn player_with(raw_ids: &[u64]) -> Player {
        let mut player = Player::new();
        for raw in raw_ids {
            player.queue_mut().push(TrackId::new(*raw));
        }
        player
    }

    #[test]
    fn new_player_is_stopped_empty_and_repeat_off() {
        let player = Player::new();
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert_eq!(player.repeat_mode(), RepeatMode::Off);
        assert_eq!(player.current_track(), None);
        assert_eq!(player, Player::default());
    }

    #[test]
    fn play_on_empty_queue_errors_and_keeps_state() {
        let mut player = Player::new();
        assert_eq!(player.play(), Err(Error::EmptyQueue));
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert_eq!(player.next_track(), None);
        assert_eq!(player.previous_track(), None);
        assert_eq!(player.on_track_ended(), None);
        assert_eq!(player.state(), PlaybackState::Stopped);
    }

    #[test]
    fn play_pause_resume_and_stop_transitions() {
        let mut player = player_with(&[1, 2]);
        player.play().expect("non-empty queue plays");
        assert_eq!(player.state(), PlaybackState::Playing);
        // play() is idempotent while playing.
        player.play().expect("re-play is fine");
        assert_eq!(player.state(), PlaybackState::Playing);

        player.pause();
        assert_eq!(player.state(), PlaybackState::Paused);
        // Pausing while paused is a no-op.
        player.pause();
        assert_eq!(player.state(), PlaybackState::Paused);

        // play() resumes from pause, keeping the selected track.
        player.play().expect("resume plays");
        assert_eq!(player.state(), PlaybackState::Playing);
        assert_eq!(player.current_track(), Some(TrackId::new(1)));

        player.stop();
        assert_eq!(player.state(), PlaybackState::Stopped);
        // Stop retains queue + selection; pause while stopped is a no-op.
        assert_eq!(player.current_track(), Some(TrackId::new(1)));
        assert_eq!(player.queue().len(), 2);
        player.pause();
        assert_eq!(player.state(), PlaybackState::Stopped);
    }

    #[test]
    fn play_track_selects_and_plays_or_errors_without_side_effects() {
        let mut player = player_with(&[1, 2, 3]);
        player
            .play_track(TrackId::new(3))
            .expect("queued track plays");
        assert_eq!(player.state(), PlaybackState::Playing);
        assert_eq!(player.current_track(), Some(TrackId::new(3)));

        assert_eq!(
            player.play_track(TrackId::new(99)),
            Err(Error::TrackNotInQueue {
                id: TrackId::new(99)
            })
        );
        assert_eq!(player.state(), PlaybackState::Playing);
        assert_eq!(player.current_track(), Some(TrackId::new(3)));
    }

    #[test]
    fn next_and_previous_move_selection_and_preserve_state() {
        let mut player = player_with(&[1, 2, 3]);
        player.play().expect("plays");
        assert_eq!(player.next_track(), Some(TrackId::new(2)));
        assert_eq!(player.state(), PlaybackState::Playing);
        assert_eq!(player.next_track(), Some(TrackId::new(3)));
        assert_eq!(player.previous_track(), Some(TrackId::new(2)));
        assert_eq!(player.previous_track(), Some(TrackId::new(1)));
        // Previous at the first track stays on it and does not stop.
        assert_eq!(player.previous_track(), Some(TrackId::new(1)));
        assert_eq!(player.state(), PlaybackState::Playing);

        // Skipping while paused moves the selection but stays paused.
        player.pause();
        assert_eq!(player.next_track(), Some(TrackId::new(2)));
        assert_eq!(player.state(), PlaybackState::Paused);
    }

    #[test]
    fn skipping_past_the_end_stops_without_repeat_all() {
        let mut player = player_with(&[1, 2]);
        player.play().expect("plays");
        player.next_track();
        assert_eq!(player.next_track(), None);
        assert_eq!(player.state(), PlaybackState::Stopped);
        // Selection stays on the last track (queue boundary rule).
        assert_eq!(player.current_track(), Some(TrackId::new(2)));
    }

    #[test]
    fn track_ended_repeat_off_advances_then_stops() {
        let mut player = player_with(&[1, 2]);
        player.play().expect("plays");
        assert_eq!(player.on_track_ended(), Some(TrackId::new(2)));
        assert_eq!(player.state(), PlaybackState::Playing);
        assert_eq!(player.on_track_ended(), None);
        assert_eq!(player.state(), PlaybackState::Stopped);
    }

    #[test]
    fn track_ended_repeat_all_wraps() {
        let mut player = player_with(&[1, 2]);
        player.set_repeat_mode(RepeatMode::All);
        player.play().expect("plays");
        assert_eq!(player.on_track_ended(), Some(TrackId::new(2)));
        assert_eq!(player.on_track_ended(), Some(TrackId::new(1)));
        assert_eq!(player.state(), PlaybackState::Playing);
    }

    #[test]
    fn track_ended_repeat_one_replays_but_explicit_skip_moves() {
        let mut player = player_with(&[1, 2]);
        player.set_repeat_mode(RepeatMode::One);
        player.play().expect("plays");
        assert_eq!(player.on_track_ended(), Some(TrackId::new(1)));
        assert_eq!(player.current_track(), Some(TrackId::new(1)));
        // An explicit user skip is not pinned by repeat-one.
        assert_eq!(player.next_track(), Some(TrackId::new(2)));
        assert_eq!(player.state(), PlaybackState::Playing);
    }

    #[test]
    fn track_ended_is_ignored_unless_playing() {
        let mut player = player_with(&[1, 2]);
        // Stopped: ignored.
        assert_eq!(player.on_track_ended(), None);
        assert_eq!(player.current_track(), Some(TrackId::new(1)));

        player.play().expect("plays");
        player.pause();
        // Paused: ignored, selection and state untouched.
        assert_eq!(player.on_track_ended(), None);
        assert_eq!(player.state(), PlaybackState::Paused);
        assert_eq!(player.current_track(), Some(TrackId::new(1)));
    }

    #[test]
    fn externally_emptied_queue_resolves_to_stopped() {
        let mut player = player_with(&[1]);
        player.play().expect("plays");
        player.queue_mut().clear();
        assert_eq!(player.current_track(), None);
        // The model does not keep "playing" nothing.
        assert_eq!(player.on_track_ended(), None);
        assert_eq!(player.state(), PlaybackState::Stopped);
        assert_eq!(player.play(), Err(Error::EmptyQueue));
    }
}
