//! The shared Flux application controller.
//!
//! This crate is the orchestration layer the rest of the workspace was
//! designed around: `flux-library` deliberately knows nothing about
//! queues, `flux-audio`'s [`Player`] knows nothing about files, and the
//! playback engine knows nothing about libraries. The [`Controller`]
//! here is the single place that wires them together, so the GUI and
//! TUI share identical behavior and neither reimplements glue logic.
//!
//! # State ownership (exactly one authority per concern)
//!
//! - [`Library`] owns the track collection and stable [`TrackId`]s.
//! - [`Player`] owns every playback *decision*: queue contents/order,
//!   current selection, playing/paused/stopped, repeat mode. The
//!   controller never second-guesses it; after every `Player` mutation
//!   it simply syncs the engine to whatever `Player` now says.
//! - The engine (`flux_audio::engine::EngineHandle`) owns decoding and
//!   the output device. It is a mechanism, not a model: the controller
//!   tells it what to load/play and consumes its events.
//! - The controller itself owns only coordination state: the volume
//!   (`Player` has none), which track it last asked the engine to load
//!   (a sync ledger, not a second selection), the index path, scan
//!   bookkeeping, and the last status/error strings for display.
//! - The track snapshot (`tracks()`) is a display cache refreshed
//!   whenever the library changes; it decides nothing.
//!
//! # Threading
//!
//! All `Controller` methods are synchronous and, except where noted,
//! non-blocking — safe to call from a UI thread. Directory scans run on
//! a controller-owned worker thread: [`begin_scan`](Controller::begin_scan)
//! starts one and [`poll`](Controller::poll) reports completion. The
//! engine has its own threads (see `flux-audio`); its events are
//! drained in `poll` without waiting. The only blocking operations are
//! index load in [`Controller::new`] (call it before starting the UI),
//! [`save_index`](Controller::save_index), and
//! [`shutdown`](Controller::shutdown) — lifecycle points, not gestures.
//!
//! # Playable vs. indexed
//!
//! The library scanner indexes 16 file extensions, but the engine
//! decodes a subset of them (see `flux_audio::decode` for the exact
//! container/codec matrix: WAV, FLAC, MP3, AAC/ADTS, Ogg Vorbis,
//! Opus, and AAC in M4A/MP4). The controller checks
//! `flux_audio::decode::is_playable` before asking the engine to load
//! anything and surfaces a clear error for the rest; indexed tracks are
//! never presented as playable by implication (see
//! [`is_track_playable`], which front-ends use for display). Even for
//! a playable extension the engine still validates the contents, and
//! corrupt files fail with a typed error — never a silent skip.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use flux_audio::Player;
use flux_audio::decode::StreamInfo;
use flux_audio::engine::{EngineEvent, EngineHandle};
use flux_core::{PlaybackState, RepeatMode, Track, TrackId};
use flux_library::{Library, ScanReport};

/// Whether real device audio is available to this controller's engine.
///
/// A controller is fully usable for browsing/scanning in the
/// `Unavailable` state; only playback commands fail, with the reason
/// surfaced as the error message. This exists so a build without an
/// audio backend (or a machine without an output device) says so
/// plainly instead of pretending to play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioStatus {
    /// The engine drives a working output (a real device, or a
    /// deliberately injected headless backend in tests).
    Ready,
    /// Playback is not possible; the string says why, in user terms.
    Unavailable(String),
}

impl AudioStatus {
    /// Whether playback commands can succeed.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }

    /// The user-facing reason playback is unavailable, if it is.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Ready => None,
            Self::Unavailable(reason) => Some(reason),
        }
    }
}

/// Notifications produced by [`Controller::poll`]. Front-ends treat
/// these as "re-read controller state now" signals; `Error`/`Status`
/// also carry the message (mirrored in
/// [`last_error`](Controller::last_error) /
/// [`last_status`](Controller::last_status)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControllerEvent {
    /// The track collection changed (scan finished, index loaded…).
    LibraryChanged,
    /// Playback state, selection, or stream info changed.
    PlaybackChanged,
    /// An informational message for the user.
    Status(String),
    /// A user-facing error. Playback state has already been made
    /// consistent (stopped) by the time this is reported.
    Error(String),
}

/// The result of one background scan, delivered to the controller.
struct ScanOutcome {
    library: Library,
    report: ScanReport,
    /// `Some` when the scan thread also tried to save the index.
    saved: Option<Result<(), String>>,
}

/// Creates the engine a Flux application should use, together with its
/// honest [`AudioStatus`].
///
/// With the `cpal` feature, this opens the default output device; on
/// any failure (or in a build without the feature) it returns a
/// headless null engine plus `Unavailable(reason)`, so the application
/// can still run and browse while telling the user exactly why
/// playback will not work. The null engine is never presented as a
/// real device — playback commands check the status first.
#[must_use]
pub fn default_engine() -> (EngineHandle, AudioStatus) {
    #[cfg(feature = "cpal")]
    {
        match EngineHandle::spawn_cpal_default() {
            Ok(engine) => return (engine, AudioStatus::Ready),
            Err(err) => {
                let (engine, _control) = EngineHandle::spawn_null(48_000, 2);
                return (
                    engine,
                    AudioStatus::Unavailable(format!("audio output unavailable: {err}")),
                );
            }
        }
    }
    #[allow(unreachable_code)]
    {
        let (engine, _control) = EngineHandle::spawn_null(48_000, 2);
        (
            engine,
            AudioStatus::Unavailable(
                "this build has no audio output backend (compiled without the `cpal` feature)"
                    .to_owned(),
            ),
        )
    }
}

/// The default library-index path: `library-index.json` inside the
/// per-user data directory resolved by `flux-platform` (for example
/// `~/.local/share/flux/library-index.json` on Linux). `None` when no
/// data directory can be resolved, in which case the application
/// simply runs without persistence.
#[must_use]
pub fn default_index_path() -> Option<PathBuf> {
    flux_platform::app_dirs("flux").map(|dirs| dirs.data_dir().join("library-index.json"))
}

/// Whether the engine can actually decode this track's file, by
/// extension (the contents are validated when the track is opened).
/// Front-ends use this to mark indexed-but-unplayable tracks instead
/// of letting users discover it by failure.
#[must_use]
pub fn is_track_playable(track: &Track) -> bool {
    flux_audio::decode::is_playable(track.source().path())
}

/// Formats a duration as `m:ss` (or `h:mm:ss`) for display.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let total = duration.as_secs();
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// The Flux application controller. See the crate-level docs for the
/// state-ownership contract.
pub struct Controller {
    /// The library, except while a scan thread owns it (`None` then).
    library: Option<Library>,
    /// Display/lookup snapshot of the library, refreshed on change.
    tracks: Vec<(TrackId, Track)>,
    player: Player,
    engine: EngineHandle,
    audio: AudioStatus,
    /// Sync ledger: the track this controller last asked the engine to
    /// load. Not a selection — `Player` owns that; this only decides
    /// "resume vs. load" when syncing.
    engine_loaded: Option<TrackId>,
    /// Stream facts for `engine_loaded`, once the engine confirms it.
    stream_info: Option<StreamInfo>,
    volume: f32,
    index_path: Option<PathBuf>,
    /// Directories scanned this session, for `refresh`.
    roots: Vec<PathBuf>,
    scan_rx: Option<Receiver<ScanOutcome>>,
    outbox: Vec<ControllerEvent>,
    last_error: Option<String>,
    last_status: Option<String>,
}

impl Controller {
    /// Creates a controller around an already-spawned engine.
    ///
    /// If `index_path` names an existing file, the library index is
    /// loaded from it (this reads and parses a file, so call this
    /// before starting the UI event loop). A missing file simply
    /// starts an empty library; an unreadable/corrupt one also starts
    /// empty but records a user-facing error in
    /// [`last_error`](Self::last_error). The play queue is built from
    /// the library in library order.
    #[must_use]
    pub fn new(engine: EngineHandle, audio: AudioStatus, index_path: Option<PathBuf>) -> Self {
        let (library, last_error) = match &index_path {
            Some(path) if path.exists() => match Library::load_from_path(path) {
                Ok(library) => (library, None),
                Err(err) => (
                    Library::new(),
                    Some(format!(
                        "Could not load the saved library index ({}): {err}. \
                         Starting with an empty library.",
                        path.display()
                    )),
                ),
            },
            _ => (Library::new(), None),
        };
        let mut controller = Self {
            tracks: snapshot(&library),
            library: Some(library),
            player: Player::new(),
            engine,
            audio,
            engine_loaded: None,
            stream_info: None,
            volume: 1.0,
            index_path,
            roots: Vec::new(),
            scan_rx: None,
            outbox: Vec::new(),
            last_error: None,
            last_status: None,
        };
        if let Some(error) = last_error {
            controller.report_error(error);
        }
        controller.rebuild_queue();
        controller.engine.set_volume(controller.volume);
        controller
    }

    /// The standard application setup: [`default_engine`] +
    /// [`default_index_path`].
    #[must_use]
    pub fn open_default() -> Self {
        let (engine, audio) = default_engine();
        Self::new(engine, audio, default_index_path())
    }

    // ------------------------------------------------------ accessors ---

    /// All tracks in library order (the display snapshot).
    #[must_use]
    pub fn tracks(&self) -> &[(TrackId, Track)] {
        &self.tracks
    }

    /// Looks up one track by ID.
    #[must_use]
    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks
            .iter()
            .find(|(t, _)| *t == id)
            .map(|(_, track)| track)
    }

    /// Tracks matching a case-insensitive substring search over title,
    /// artist, and album (same rule as `Library::search`; an empty
    /// query matches everything).
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<(TrackId, &Track)> {
        let needle = query.trim().to_lowercase();
        self.tracks
            .iter()
            .filter(|(_, track)| {
                needle.is_empty()
                    || track.title().to_lowercase().contains(&needle)
                    || track
                        .artist()
                        .is_some_and(|a| a.to_lowercase().contains(&needle))
                    || track
                        .album()
                        .is_some_and(|a| a.to_lowercase().contains(&needle))
            })
            .map(|(id, track)| (*id, track))
            .collect()
    }

    /// The playback model's state.
    #[must_use]
    pub fn player_state(&self) -> PlaybackState {
        self.player.state()
    }

    /// The currently selected track's ID, if any.
    #[must_use]
    pub fn current_id(&self) -> Option<TrackId> {
        self.player.current_track()
    }

    /// The currently selected track, if any.
    #[must_use]
    pub fn current_track(&self) -> Option<&Track> {
        self.current_id().and_then(|id| self.track(id))
    }

    /// The active repeat mode.
    #[must_use]
    pub fn repeat_mode(&self) -> RepeatMode {
        self.player.repeat_mode()
    }

    /// Elapsed playback position of the loaded track (from the engine;
    /// zero when nothing is loaded).
    #[must_use]
    pub fn position(&self) -> Duration {
        self.engine.position()
    }

    /// The current volume, `0.0..=1.0`.
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Whether a background library scan is running.
    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.scan_rx.is_some()
    }

    /// Whether (and why not) device audio is available.
    #[must_use]
    pub fn audio_status(&self) -> &AudioStatus {
        &self.audio
    }

    /// Decoded-stream facts for the loaded track, once known.
    #[must_use]
    pub fn stream_info(&self) -> Option<StreamInfo> {
        self.stream_info
    }

    /// The most recent user-facing error, until another replaces it.
    #[must_use]
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    /// The most recent informational status message.
    #[must_use]
    pub fn last_status(&self) -> Option<&str> {
        self.last_status.as_deref()
    }

    // ------------------------------------------------------- commands ---

    /// Starts playing the given track (which must be in the library).
    /// Restarting the already-playing track restarts it. Failures
    /// (unknown track, unsupported format, unavailable audio) set
    /// [`last_error`](Self::last_error) and leave playback stopped.
    pub fn play_track(&mut self, id: TrackId) {
        match self.player.play_track(id) {
            Ok(()) => self.sync_engine(true),
            Err(err) => self.report_error(format!("Cannot play that track: {err}")),
        }
    }

    /// Starts or resumes playback of the current selection.
    pub fn play(&mut self) {
        match self.player.play() {
            Ok(()) => self.sync_engine(false),
            Err(err) => self.report_error(format!("Nothing to play: {err}")),
        }
    }

    /// Pauses playback (no-op unless playing).
    pub fn pause(&mut self) {
        self.player.pause();
        self.sync_engine(false);
    }

    /// Toggles between playing and paused (from stopped: plays).
    pub fn toggle_play_pause(&mut self) {
        if self.player.state() == PlaybackState::Playing {
            self.pause();
        } else {
            self.play();
        }
    }

    /// Stops playback. The selection is retained.
    pub fn stop(&mut self) {
        self.player.stop();
        self.sync_engine(false);
    }

    /// Skips to the next track; at the end of the queue (repeat off)
    /// playback stops. While paused, the new track is loaded paused.
    pub fn next(&mut self) {
        self.player.next_track();
        self.sync_engine(true);
    }

    /// Skips to the previous track; on the first track, restarts it.
    pub fn previous(&mut self) {
        self.player.previous_track();
        self.sync_engine(true);
    }

    /// Sets the repeat mode used for skips and end-of-track handling.
    pub fn set_repeat_mode(&mut self, mode: RepeatMode) {
        self.player.set_repeat_mode(mode);
    }

    /// Cycles repeat: Off → All → One → Off.
    pub fn cycle_repeat_mode(&mut self) {
        let next = match self.player.repeat_mode() {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        };
        self.set_repeat_mode(next);
        self.report_status(format!("Repeat: {}", repeat_label(next)));
    }

    /// Sets the volume (clamped to `0.0..=1.0`) and applies it to the
    /// engine immediately.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        };
        self.engine.set_volume(self.volume);
    }

    /// Adjusts the volume by a delta (keyboard-friendly).
    pub fn adjust_volume(&mut self, delta: f32) {
        self.set_volume(self.volume + delta);
    }

    /// Begins a background scan of `root` (recursive). Completion (and
    /// the refreshed track list) arrives via [`poll`](Self::poll);
    /// playback in progress is unaffected. A second scan while one is
    /// running is reported as an error and ignored. When the scan
    /// finishes, the updated library is saved to the index path (if
    /// any) on the scan thread.
    pub fn begin_scan(&mut self, root: PathBuf) {
        self.begin_scan_roots(vec![root]);
    }

    /// Re-scans every directory scanned this session (picking up files
    /// added since). With no prior scan this session, reports a status
    /// message instead. Roots are not persisted in the version-1
    /// index, so refresh is session-scoped by design.
    pub fn refresh(&mut self) {
        if self.roots.is_empty() {
            self.report_status("Nothing to refresh yet — scan a folder first.".to_owned());
            return;
        }
        let roots = self.roots.clone();
        self.begin_scan_roots(roots);
    }

    /// Saves the library index now (blocking; intended for shutdown —
    /// scans already save automatically on completion).
    ///
    /// # Errors
    ///
    /// A user-facing message if a scan is in progress or the write
    /// fails. Without an index path this is a no-op success.
    pub fn save_index(&mut self) -> Result<(), String> {
        let Some(path) = self.index_path.clone() else {
            return Ok(());
        };
        let Some(library) = self.library.as_ref() else {
            return Err("A scan is in progress; the index will be saved when it finishes.".into());
        };
        save_library(library, &path)
    }

    /// Clean shutdown: waits for any running scan to finish (its save
    /// included), then stops the audio engine and joins its worker.
    pub fn shutdown(mut self) {
        if let Some(rx) = self.scan_rx.take()
            && let Ok(outcome) = rx.recv()
        {
            self.library = Some(outcome.library);
        }
        self.engine.shutdown();
    }

    // ------------------------------------------------------------ poll ---

    /// Drains pending notifications: queued command results, scan
    /// completion, and engine events (end-of-track advancement and
    /// playback errors are handled here). Never blocks; call it on a
    /// short timer from the UI event loop.
    pub fn poll(&mut self) -> Vec<ControllerEvent> {
        let mut events = std::mem::take(&mut self.outbox);

        enum ScanPoll {
            Done(ScanOutcome),
            Lost,
        }
        let scan_state = self.scan_rx.as_ref().map(|rx| match rx.try_recv() {
            Ok(outcome) => Some(ScanPoll::Done(outcome)),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(ScanPoll::Lost),
        });
        match scan_state {
            Some(Some(ScanPoll::Done(outcome))) => {
                self.scan_rx = None;
                self.finish_scan(outcome);
                events.push(ControllerEvent::LibraryChanged);
            }
            Some(Some(ScanPoll::Lost)) => {
                self.scan_rx = None;
                if self.library.is_none() {
                    self.library = Some(Library::new());
                    self.tracks.clear();
                    self.rebuild_queue();
                }
                self.report_error("The library scan ended unexpectedly.".to_owned());
            }
            _ => {}
        }

        let engine_events: Vec<EngineEvent> = {
            let receiver = self.engine.events();
            let mut drained = Vec::new();
            while let Ok(event) = receiver.try_recv() {
                drained.push(event);
            }
            drained
        };
        for event in engine_events {
            match event {
                EngineEvent::Loaded { id, info } => {
                    if self.engine_loaded == Some(id) {
                        self.stream_info = Some(info);
                        events.push(ControllerEvent::PlaybackChanged);
                    }
                }
                EngineEvent::Started { .. } | EngineEvent::Paused { .. } | EngineEvent::Stopped => {
                    events.push(ControllerEvent::PlaybackChanged)
                }
                EngineEvent::TrackEnded { id } => {
                    // Accept only the end of the track we loaded and
                    // the model still considers current — stale events
                    // can never advance the wrong queue position (the
                    // engine's generations already prevent them; this
                    // is the controller-side belt to that suspenders).
                    if self.engine_loaded == Some(id) && self.player.current_track() == Some(id) {
                        if self.player.on_track_ended().is_some() {
                            self.sync_engine(true);
                        }
                        events.push(ControllerEvent::PlaybackChanged);
                    }
                }
                EngineEvent::Error { id, error } => {
                    let message = match id.and_then(|i| self.track(i)) {
                        Some(track) => {
                            format!("Playback failed for \"{}\": {error}", track.title())
                        }
                        None => format!("Audio error: {error}"),
                    };
                    self.player.stop();
                    self.engine_loaded = None;
                    self.stream_info = None;
                    self.report_error(message);
                    events.push(ControllerEvent::PlaybackChanged);
                }
            }
        }

        events.append(&mut self.outbox);
        events
    }

    // -------------------------------------------------------- internals ---

    /// Rebuilds the player's queue as "the whole library, in library
    /// order", preserving the current selection when it still exists.
    fn rebuild_queue(&mut self) {
        let previous = self.player.current_track();
        let ids: Vec<TrackId> = self.tracks.iter().map(|(id, _)| *id).collect();
        let queue = self.player.queue_mut();
        queue.clear();
        for id in ids {
            queue.push(id);
        }
        if let Some(id) = previous {
            let _ = queue.set_current_by_id(id);
        }
    }

    /// The one place engine commands are derived from `Player` state.
    /// `force_reload` distinguishes "the user asked for (possibly the
    /// same) track explicitly — start it from the top" from "resume
    /// whatever is loaded".
    fn sync_engine(&mut self, force_reload: bool) {
        match self.player.state() {
            PlaybackState::Playing => {
                let Some(id) = self.player.current_track() else {
                    self.engine.stop();
                    self.engine_loaded = None;
                    return;
                };
                if self.engine_loaded == Some(id) && !force_reload {
                    self.engine.play();
                } else if let Err(message) = self.load_into_engine(id, true) {
                    self.fail_playback(message);
                }
            }
            PlaybackState::Paused => match self.player.current_track() {
                Some(id) if self.engine_loaded == Some(id) => self.engine.pause(),
                Some(id) => {
                    if let Err(message) = self.load_into_engine(id, false) {
                        self.fail_playback(message);
                    }
                }
                None => {
                    self.engine.stop();
                    self.engine_loaded = None;
                }
            },
            PlaybackState::Stopped => {
                self.engine.stop();
                self.engine_loaded = None;
                self.stream_info = None;
            }
        }
    }

    /// Resolves `id` to a file and asks the engine to load it.
    /// Synchronous failures (no audio, unknown track, unsupported
    /// format) are returned for the caller to surface; decode/IO
    /// failures arrive asynchronously as engine error events.
    fn load_into_engine(&mut self, id: TrackId, autoplay: bool) -> Result<(), String> {
        if let AudioStatus::Unavailable(reason) = &self.audio {
            return Err(reason.clone());
        }
        let Some(track) = self.track(id) else {
            return Err("That track is no longer in the library.".to_owned());
        };
        let path = track.source().path().to_path_buf();
        let title = track.title().to_owned();
        if !flux_audio::decode::is_playable(&path) {
            return Err(format!(
                "\"{title}\" cannot be played: Flux plays WAV, FLAC, MP3, AAC, Ogg Vorbis, \
                 Opus, and M4A/MP4 (AAC) files. The file stays in the library, indexed."
            ));
        }
        self.engine.load(id, path);
        if autoplay {
            self.engine.play();
        }
        self.engine_loaded = Some(id);
        self.stream_info = None;
        Ok(())
    }

    /// Common failure path: stop the model and the engine, forget the
    /// load, and tell the user. State is consistent afterwards.
    fn fail_playback(&mut self, message: String) {
        self.player.stop();
        self.engine.stop();
        self.engine_loaded = None;
        self.stream_info = None;
        self.report_error(message);
    }

    fn begin_scan_roots(&mut self, roots: Vec<PathBuf>) {
        if self.scan_rx.is_some() {
            self.report_error("A library scan is already in progress.".to_owned());
            return;
        }
        let Some(library) = self.library.take() else {
            self.report_error("A library scan is already in progress.".to_owned());
            return;
        };
        for root in &roots {
            if !self.roots.contains(root) {
                self.roots.push(root.clone());
            }
        }
        self.report_status(format!(
            "Scanning {}…",
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
        let index_path = self.index_path.clone();
        let (work_tx, work_rx) =
            std::sync::mpsc::channel::<(Library, Vec<PathBuf>, Option<PathBuf>)>();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<ScanOutcome>();
        // Spawn first and hand the library over only once the thread
        // exists, so a spawn failure cannot lose the collection.
        let spawned = std::thread::Builder::new()
            .name("flux-library-scan".to_owned())
            .spawn(move || {
                if let Ok((mut library, roots, index_path)) = work_rx.recv() {
                    let root_refs: Vec<&Path> = roots.iter().map(PathBuf::as_path).collect();
                    let report = library.scan(&root_refs);
                    let saved = index_path.as_ref().map(|path| save_library(&library, path));
                    let _ = done_tx.send(ScanOutcome {
                        library,
                        report,
                        saved,
                    });
                }
            });
        match spawned {
            Ok(_handle) => {
                if work_tx.send((library, roots, index_path)).is_ok() {
                    self.scan_rx = Some(done_rx);
                } else {
                    self.library = Some(Library::new());
                    self.report_error("The library scan could not be started.".to_owned());
                }
            }
            Err(err) => {
                self.library = Some(library);
                self.report_error(format!("Could not start the scan thread: {err}"));
            }
        }
    }

    fn finish_scan(&mut self, outcome: ScanOutcome) {
        self.tracks = snapshot(&outcome.library);
        self.library = Some(outcome.library);
        self.rebuild_queue();
        let report = &outcome.report;
        self.report_status(format!(
            "Scan complete: {} added, {} updated, {} already indexed, {} skipped, {} failed. {} track(s) in the library.",
            report.added(),
            report.updated(),
            report.deduplicated(),
            report.skipped().len(),
            report.failed().len(),
            self.tracks.len(),
        ));
        if let Some(Err(message)) = outcome.saved {
            self.report_error(message);
        }
    }

    fn report_error(&mut self, message: String) {
        self.last_error = Some(message.clone());
        self.outbox.push(ControllerEvent::Error(message));
    }

    fn report_status(&mut self, message: String) {
        self.last_status = Some(message.clone());
        self.outbox.push(ControllerEvent::Status(message));
    }
}

fn snapshot(library: &Library) -> Vec<(TrackId, Track)> {
    library
        .iter()
        .map(|(id, track)| (id, track.clone()))
        .collect()
}

fn repeat_label(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    }
}

/// Creates parent directories, then writes the versioned index
/// (flushing the "parent must exist" precondition of
/// `Library::save_to_path`).
fn save_library(library: &Library, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    library
        .save_to_path(path)
        .map_err(|err| format!("Could not save the library index: {err}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use flux_audio::engine::EngineHandle;
    use flux_audio::output::NullControl;
    use flux_core::{PlaybackState, RepeatMode, TrackId};

    use super::{AudioStatus, Controller, ControllerEvent, is_track_playable};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-app-test-{}-{test}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
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

    /// Mono 44.1 kHz 16-bit WAV of constant DC — sample accounting is
    /// exact and amplitude reveals the applied gain.
    fn write_dc_wav(dir: &Path, name: &str, frames: usize, value: f32) -> PathBuf {
        let path = dir.join(name);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create wav");
        let sample = (value * 32_767.0) as i32;
        for _ in 0..frames {
            writer.write_sample(sample).expect("write sample");
        }
        writer.finalize().expect("finalize");
        path
    }

    /// Standard fixture set: two playable WAVs, one indexed-but-not-
    /// playable AIFF, one corrupt WAV, in `<dir>/music`.
    struct Fixtures {
        root: PathBuf,
        a: PathBuf,
        b: PathBuf,
    }

    fn fixtures(dir: &TempDir) -> Fixtures {
        let root = dir.path().join("music");
        fs::create_dir_all(&root).expect("music dir");
        let a = write_dc_wav(&root, "a.wav", 11_025, 0.4);
        let b = write_dc_wav(&root, "b.wav", 11_025, 0.3);
        fs::write(root.join("c.aif"), b"not really aiff").expect("aiff bytes");
        fs::write(root.join("d.wav"), b"not really wav").expect("corrupt wav");
        Fixtures { root, a, b }
    }

    fn boot(dir: &TempDir) -> (Controller, NullControl) {
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let index = dir.path().join("state").join("library-index.json");
        (
            Controller::new(engine, AudioStatus::Ready, Some(index)),
            control,
        )
    }

    /// Convergence driver: drains the null device deterministically and
    /// polls until `done` observes the controller/events it wants.
    /// Every iteration makes real progress (512 device frames + scan/
    /// engine events), so the cap is a safety net, not a timing guess.
    fn drive(
        controller: &mut Controller,
        control: &NullControl,
        done: impl Fn(&Controller, &[ControllerEvent]) -> bool,
    ) -> Vec<ControllerEvent> {
        let mut seen = Vec::new();
        for _ in 0..4_000 {
            control.drain_frames(512);
            let events = controller.poll();
            let finished = done(controller, &events);
            seen.extend(events);
            if finished {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        seen
    }

    fn scanned(controller: &mut Controller, control: &NullControl, root: PathBuf) {
        controller.begin_scan(root);
        let events = drive(controller, control, |c, _| !c.is_scanning());
        assert!(
            events.contains(&ControllerEvent::LibraryChanged),
            "expected LibraryChanged, got {events:?}"
        );
    }

    fn id_of(controller: &Controller, path: &Path) -> TrackId {
        controller
            .tracks()
            .iter()
            .find(|(_, t)| t.source().path() == path)
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("track for {} indexed", path.display()))
    }

    #[test]
    fn scan_lists_tracks_and_persists_stable_ids() {
        let dir = TempDir::new("scan");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);

        assert!(controller.tracks().is_empty());
        scanned(&mut controller, &control, fx.root.clone());
        // All four files are indexed (even the corrupt one and the
        // MP3) — indexed does not mean playable.
        assert_eq!(
            controller.tracks().len(),
            4,
            "{:?}",
            controller.last_status()
        );
        assert_eq!(
            controller
                .tracks()
                .iter()
                .filter(|(_, t)| is_track_playable(t))
                .count(),
            3
        );
        assert!(
            controller
                .last_status()
                .is_some_and(|s| s.contains("Scan complete"))
        );

        // Re-scan deduplicates; IDs stay stable through a reload.
        let ids_before: Vec<TrackId> = controller.tracks().iter().map(|(id, _)| *id).collect();
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(controller.tracks().len(), 4);
        controller.shutdown();

        let (controller2, _control2) = boot(&dir);
        let ids_after: Vec<TrackId> = controller2.tracks().iter().map(|(id, _)| *id).collect();
        assert_eq!(ids_after, ids_before, "persisted IDs must be stable");
        assert_eq!(id_of(&controller2, &fx.a).get(), ids_before[0].get());
        controller2.shutdown();
    }

    #[test]
    fn play_pause_resume_and_stop_cycle() {
        let dir = TempDir::new("cycle");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);

        controller.play_track(a);
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.current_id(), Some(a));
        drive(&mut controller, &control, |c, _| {
            c.position() > Duration::from_millis(30)
        });
        let nonzero_before = control
            .captured()
            .iter()
            .filter(|s| s.abs() > 0.001)
            .count();
        assert!(nonzero_before > 0);

        controller.pause();
        assert_eq!(controller.player_state(), PlaybackState::Paused);
        // Pause reaches the engine asynchronously; converge on
        // *observed* silence (three silent drains in a row) rather
        // than guessing which control event marks its arrival.
        let mut silent_streak = 0;
        while silent_streak < 3 {
            control.clear_captured();
            control.drain_frames(512);
            controller.poll();
            if control.captured().iter().all(|s| s.abs() <= 0.001) {
                silent_streak += 1;
            } else {
                silent_streak = 0;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        control.clear_captured();
        for _ in 0..8 {
            control.drain_frames(512);
            controller.poll();
        }
        assert_eq!(
            control
                .captured()
                .iter()
                .filter(|s| s.abs() > 0.001)
                .count(),
            0,
            "paused means silence"
        );

        controller.toggle_play_pause();
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        // Drain to the end: A advances into B, B advances onto the MP3,
        // which errors and stops (covered in detail by its own test).
        drive(&mut controller, &control, |c, _| {
            c.player_state() == PlaybackState::Stopped
        });

        controller.play_track(a);
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.stop();
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        // Position resets on the first device fill after the engine
        // processes Stop — converge rather than assume it landed.
        drive(&mut controller, &control, |c, _| {
            c.position() == Duration::ZERO
        });
        assert_eq!(controller.position(), Duration::ZERO);
        controller.shutdown();
    }

    #[test]
    fn track_end_advances_through_queue_then_stops() {
        let dir = TempDir::new("advance");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let b = id_of(&controller, &fx.b);

        // From A, the queue's next playable-advance target is B
        // (library order: a, b, c.aif, d.wav follow scan order).
        controller.play_track(a);
        drive(&mut controller, &control, |c, _| {
            c.current_id() == Some(b) && c.player_state() == PlaybackState::Playing
        });
        assert_eq!(
            controller.current_id(),
            Some(b),
            "end of A must advance to B"
        );

        // B ends → next in queue is the AIFF → clear error, stopped,
        // consistent (no silent skip, no stuck Playing).
        let events = drive(&mut controller, &control, |c, ev| {
            c.player_state() == PlaybackState::Stopped
                && ev.iter().any(|e| matches!(e, ControllerEvent::Error(_)))
        });
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(m) if m.contains("cannot be played"))),
            "auto-advance onto an unplayable format must error clearly: {events:?}"
        );
        controller.shutdown();
    }

    #[test]
    fn compressed_formats_play_and_advance_through_the_controller() {
        // Real encoded fixtures from the flux-audio test suite
        // (provenance: crates/flux-audio/tests/fixtures/README.md).
        const OGG: &[u8] = include_bytes!("../../flux-audio/tests/fixtures/tone.ogg");
        const M4A: &[u8] = include_bytes!("../../flux-audio/tests/fixtures/tone.m4a");
        const MP3: &[u8] = include_bytes!("../../flux-audio/tests/fixtures/silence.mp3");
        let dir = TempDir::new("formats");
        let root = dir.path().join("music");
        fs::create_dir_all(&root).expect("music dir");
        fs::write(root.join("tone.ogg"), OGG).expect("ogg fixture");
        fs::write(root.join("tone.m4a"), M4A).expect("m4a fixture");
        fs::write(root.join("silence.mp3"), MP3).expect("mp3 fixture");
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, root.clone());

        // Play each compressed track in library order: every one must
        // reach Playing with a real stream info and produce no error,
        // then end and auto-advance (or stop after the last).
        let order: Vec<TrackId> = controller.tracks().iter().map(|(id, _)| *id).collect();
        assert_eq!(order.len(), 3, "all three fixtures indexed");
        for track in controller.tracks() {
            assert!(
                super::is_track_playable(&track.1),
                "{} playable",
                track.1.title()
            );
        }
        for (i, id) in order.iter().enumerate() {
            controller.play_track(*id);
            let events = drive(&mut controller, &control, |c, _| {
                c.current_id() == Some(*id)
                    && c.player_state() == PlaybackState::Playing
                    && c.stream_info().is_some()
            });
            assert!(
                events
                    .iter()
                    .all(|e| !matches!(e, ControllerEvent::Error(_))),
                "no playback errors expected: {events:?}"
            );
            // Let it run to completion.
            let is_last = i + 1 == order.len();
            let events = drive(&mut controller, &control, |c, _| {
                if is_last {
                    c.player_state() == PlaybackState::Stopped
                } else {
                    c.current_id() == Some(order[i + 1])
                }
            });
            assert!(
                events
                    .iter()
                    .all(|e| !matches!(e, ControllerEvent::Error(_))),
                "EOF advancement must not error: {events:?}"
            );
        }
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        controller.shutdown();
    }

    #[test]
    fn next_and_previous_move_selection_and_restart_playback() {
        let dir = TempDir::new("skip");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let b = id_of(&controller, &fx.b);

        controller.play_track(a);
        drive(&mut controller, &control, |c, _| {
            c.position() > Duration::from_millis(20)
        });
        controller.next();
        assert_eq!(controller.current_id(), Some(b));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.previous();
        assert_eq!(controller.current_id(), Some(a));
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.shutdown();
    }

    #[test]
    fn volume_is_owned_by_the_controller_and_applied() {
        let dir = TempDir::new("volume");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);

        controller.set_volume(0.5);
        assert_eq!(controller.volume(), 0.5);
        controller.adjust_volume(0.1);
        assert!((controller.volume() - 0.6).abs() < 1e-6);
        controller.set_volume(2.5);
        assert_eq!(controller.volume(), 1.0, "clamped");
        controller.set_volume(0.5);

        controller.play_track(a);
        drive(&mut controller, &control, |c, _| {
            c.position() > Duration::from_millis(50)
        });
        let peak = control
            .captured()
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - 0.2).abs() < 0.02,
            "0.4 DC at gain 0.5 should peak ≈0.2, got {peak}"
        );
        controller.shutdown();
    }

    #[test]
    fn unsupported_format_is_a_clear_error_and_state_stays_consistent() {
        let dir = TempDir::new("unsupported");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let aif = id_of(&controller, &fx.root.join("c.aif"));

        controller.play_track(aif);
        let events = controller.poll();
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(m) if m.contains("cannot be played"))),
            "{events:?}"
        );
        let a = id_of(&controller, &fx.a);
        controller.play_track(a);
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        controller.shutdown();
    }

    #[test]
    fn missing_and_corrupt_files_surface_errors_without_crashing() {
        let dir = TempDir::new("badfiles");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);
        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        let corrupt = id_of(&controller, &fx.root.join("d.wav"));

        // Corrupt: opens as .wav, fails to decode → async engine error.
        controller.play_track(corrupt);
        let events = drive(&mut controller, &control, |_, ev| {
            ev.iter().any(|e| matches!(e, ControllerEvent::Error(_)))
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(m) if m.contains("Playback failed"))),
            "{events:?}"
        );
        assert_eq!(controller.player_state(), PlaybackState::Stopped);

        // Missing: file deleted after indexing (never pruned), play
        // reports the IO failure and stops cleanly.
        fs::remove_file(&fx.a).expect("remove file");
        controller.play_track(a);
        let events = drive(&mut controller, &control, |_, ev| {
            ev.iter().any(|e| matches!(e, ControllerEvent::Error(_)))
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(_)))
        );
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert!(
            controller.track(a).is_some(),
            "missing files are not pruned"
        );
        controller.shutdown();
    }

    #[test]
    fn unavailable_audio_refuses_playback_but_still_browses() {
        let dir = TempDir::new("noaudio");
        let fx = fixtures(&dir);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let mut controller = Controller::new(
            engine,
            AudioStatus::Unavailable("no output device in this test".to_owned()),
            None,
        );
        scanned(&mut controller, &control, fx.root.clone());
        assert_eq!(controller.tracks().len(), 4, "browsing works without audio");

        let a = id_of(&controller, &fx.a);
        controller.play_track(a);
        let events = controller.poll();
        assert_eq!(controller.player_state(), PlaybackState::Stopped);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Error(m) if m.contains("no output device")))
        );
        control.drain_frames(1024);
        assert_eq!(
            control
                .captured()
                .iter()
                .filter(|s| s.abs() > 0.001)
                .count(),
            0,
            "nothing may 'play' silently"
        );
        controller.shutdown();
    }

    #[test]
    fn repeat_one_replays_and_refresh_without_roots_is_a_status() {
        let dir = TempDir::new("repeat");
        let fx = fixtures(&dir);
        let (mut controller, control) = boot(&dir);

        controller.refresh();
        let events = controller.poll();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, ControllerEvent::Status(_)))
        );
        assert!(controller.last_error().is_none());

        scanned(&mut controller, &control, fx.root.clone());
        let a = id_of(&controller, &fx.a);
        controller.set_repeat_mode(RepeatMode::One);
        controller.play_track(a);
        // Two full plays of A (11_025 frames each) must pass through.
        drive(&mut controller, &control, |_, _| {
            control
                .captured()
                .iter()
                .filter(|s| s.abs() > 0.001)
                .count()
                >= 22_050
        });
        assert_eq!(controller.player_state(), PlaybackState::Playing);
        assert_eq!(controller.current_id(), Some(a));
        controller.shutdown();
    }

    #[test]
    fn shutdown_during_scan_is_clean() {
        let dir = TempDir::new("shutdown");
        let fx = fixtures(&dir);
        let (mut controller, _control) = boot(&dir);
        controller.begin_scan(fx.root.clone());
        controller.shutdown(); // joins the scan worker; must not hang
        let (controller2, _control2) = boot(&dir);
        assert_eq!(
            controller2.tracks().len(),
            4,
            "scan saved its index before shutdown"
        );
        controller2.shutdown();
    }
}
