//! The playback engine: a worker thread that decodes local files and
//! feeds an [`OutputBackend`], driven by commands and reporting events.
//!
//! # Architecture
//!
//! ```text
//! caller (any thread)                 engine worker thread
//! ┌──────────────────┐  commands   ┌──────────────────────────────┐
//! │  EngineHandle    │ ──────────▶ │ command loop                 │
//! │  (this module)   │  (mpsc)     │  ├─ active Decoder           │
//! │                  │ ◀────────── │  ├─ prepared next Decoder    │
//! │                  │             │  ├─ FormatConverter          │
//! └──────────────────┘   events    │  └─ ring buffer producer     │
//!        ▲             (mpsc)      └──────────────┬───────────────┘
//!        │ position()/volume: atomics             │ SPSC ring buffer
//!        │                              ┌─────────▼───────────────┐
//!        └──────────── shared state ─── │ SampleSource (callback) │
//!                                       │ OutputBackend thread    │
//!                                       └─────────────────────────┘
//! ```
//!
//! - Decoding and all file I/O happen on the worker thread. Handle
//!   methods only enqueue a command or read an atomic; they never block
//!   on decoding or the device. (The one exception is
//!   [`EngineHandle::spawn`], which waits for the backend to open.)
//! - The audio callback ([`FillCallback`]) only pops from a lock-free
//!   SPSC ring buffer, fills silence on underrun, and publishes the
//!   final mono output into a second bounded lock-free tap for the
//!   oscilloscope — no allocation, locking, or I/O on the real-time
//!   thread. The GUI drains that tap on its own thread.
//! - Every load/stop bumps a **generation counter**. The callback
//!   discards buffered samples when the generation changes and then
//!   *acknowledges* the new generation; the worker pushes no samples
//!   for a generation until it has been acknowledged, so the flush
//!   can never discard the beginning of a just-loaded track. The
//!   worker only emits end-of-track for the current generation, so
//!   rapid track changes can never leak stale samples or stale
//!   `TrackEnded` events.
//! - [`EngineEvent::TrackEnded`] fires only after the decoder reached
//!   EOF **and** the ring buffer has fully drained through the backend
//!   — never merely when decoding finished. It is the fallback when no
//!   prepared successor exists.
//! - A matching prepared source is consumed at the active decoder's
//!   end-of-stream **inside the same generation**. The converter tail
//!   is pushed first, then the prepared decoder continues into the same
//!   ring. No device restart, ring flush or artificial gap is inserted.
//!   [`EngineEvent::Transitioned`] is emitted exactly once when the
//!   device has consumed up to the old source's final output frame.
//! - With a positive [`EngineHandle::set_crossfade_ms`] duration, the
//!   worker retains a bounded tail of outgoing output frames. At EOF it
//!   decodes the incoming head, mixes both through an equal-power
//!   envelope in the worker, and queues the mixed frames in the same
//!   generation. The Controller-visible transition is emitted at the
//!   **start** of that overlap, when incoming source position is zero.
//!   The outgoing tail continues fading after the event. Zero duration
//!   preserves the gapless path above exactly.
//! - [`EngineHandle::preload`] opens at most one future decoder on the
//!   worker, independently of the active one. A matching load consumes
//!   it; a different load, cancellation or replacement drops it. A
//!   preload failure is reported separately and cannot disturb the
//!   active source.
//! - Load, seek, stop and restart bump the generation and flush buffered
//!   audio. A natural handoff does **not** bump it: seek/stop/replacement
//!   invalidate stale samples explicitly, while adjacent sources stay in
//!   one continuous output generation.
//!
//! The engine knows nothing about libraries, queues, or scanning: the
//! caller hands it a [`TrackId`] (echoed back in events) and a file
//! path. [`crate::Player`] remains the single authority for queue/playback
//! *decisions*; the engine is the mechanism that makes them audible.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flux_core::{QueueEntryId, TrackId};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::decode::{DecodeError, Decoder, StreamInfo, open_decoder};
use crate::output::{FillCallback, NullControl, NullOutput, OutputBackend, OutputConfig};

/// Frames decoded per worker iteration (per channel).
const DECODE_CHUNK_FRAMES: usize = 4096;
/// Worker loop tick: how long to wait for a command before pumping
/// more samples / checking end-of-track.
const WORKER_TICK: Duration = Duration::from_millis(5);

/// Largest supported natural-transition crossfade. Zero disables
/// crossfade and preserves the frame-boundary gapless path.
pub const MAX_CROSSFADE_MS: u64 = 5_000;

/// Minimum outgoing audio normally pushed before holding the crossfade
/// tail. This keeps startup and short-source handling bounded.
const CROSSFADE_PREBUFFER_SECONDS: u32 = 1;

/// Number of post-mix samples exposed to the GUI oscilloscope.
///
/// This matches Flow's default Web Audio `AnalyserNode.fftSize` of 4096
/// time-domain samples.
pub const WAVEFORM_WINDOW: usize = 4096;

/// Mono samples buffered between the audio callback and the next GUI
/// waveform read. The callback never blocks when this is full; it drops
/// the excess sample instead. At 48 kHz this is roughly 340 ms.
const WAVEFORM_RING_CAPACITY: usize = 16_384;

/// Errors surfaced by the engine, via [`EngineEvent::Error`] or from
/// [`EngineHandle::spawn`].
#[derive(Debug)]
pub enum EngineError {
    /// Opening or decoding the track's file failed.
    Decode(DecodeError),
    /// The output device/stream failed, or no device is available.
    /// Carries the backend's message.
    Output(String),
    /// Playback was requested with no track loaded.
    NoTrackLoaded,
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(err) => write!(f, "{err}"),
            Self::Output(msg) => write!(f, "audio output error: {msg}"),
            Self::NoTrackLoaded => write!(f, "no track is loaded"),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode(err) => Some(err),
            Self::Output(_) | Self::NoTrackLoaded => None,
        }
    }
}

impl From<DecodeError> for EngineError {
    fn from(err: DecodeError) -> Self {
        Self::Decode(err)
    }
}

/// Events emitted by the engine worker. Receive them through
/// [`EngineHandle::events`]; the channel disconnects after shutdown.
#[derive(Debug)]
pub enum EngineEvent {
    /// A track was opened successfully and is ready to play.
    Loaded {
        /// The load session returned by [`EngineHandle::load`].
        session: u64,
        /// The caller-supplied track ID.
        id: TrackId,
        /// The stable queue entry this source belongs to.
        entry: QueueEntryId,
        /// The decoded stream's facts (file sample rate/channels).
        info: StreamInfo,
    },
    /// A future source was decoded and prepared without touching playback.
    Preloaded {
        /// Caller-supplied track ID.
        id: TrackId,
        /// Stable queue entry the prepared source belongs to.
        entry: QueueEntryId,
        /// Decoded stream facts.
        info: StreamInfo,
    },
    /// Preparing a future source failed. Current playback is unaffected.
    PreloadFailed {
        /// Caller-supplied track ID.
        id: TrackId,
        /// Stable queue entry the preparation was for.
        entry: QueueEntryId,
        /// Failure detail.
        error: EngineError,
    },
    /// Playback of the track started or resumed.
    Started {
        /// The load session this playback belongs to.
        session: u64,
        /// The track that started.
        id: TrackId,
    },
    /// Playback of the track paused.
    Paused {
        /// The load session this playback belongs to.
        session: u64,
        /// The track that paused.
        id: TrackId,
    },
    /// Playback stopped and the loaded track was unloaded.
    Stopped,
    /// The engine completed a natural gapless handoff at the exact
    /// output-frame boundary. No new load was requested and no output
    /// generation was flushed.
    Transitioned {
        /// Load session for the active pipeline generation.
        session: u64,
        /// Output generation that contains the transition.
        generation: u64,
        /// Source that just ended.
        from_id: TrackId,
        /// Stable queue entry of the source that ended.
        from_entry: QueueEntryId,
        /// Source that is now active.
        to_id: TrackId,
        /// Stable queue entry of the source that is now active.
        to_entry: QueueEntryId,
        /// Monotonic output-frame boundary since the generation began.
        boundary_frame: u64,
        /// Decoded facts for the new source.
        info: StreamInfo,
    },
    /// The track played to completion: every decoded sample has been
    /// consumed by the backend.
    TrackEnded {
        /// The load session that ended.
        session: u64,
        /// The stable queue entry that ended.
        entry: QueueEntryId,
        /// The track that ended.
        id: TrackId,
    },
    /// Something failed. `session` is the load attempt concerned when
    /// applicable; `None` denotes a global output or command error.
    /// After a decode error the engine is stopped with nothing playing;
    /// the caller (via [`crate::Player`]) decides what happens next.
    Error {
        /// The load session concerned, if any.
        session: Option<u64>,
        /// The track concerned, if any.
        id: Option<TrackId>,
        /// What failed.
        error: EngineError,
    },
}

/// Commands understood by the worker (handle methods enqueue these).
enum Command {
    Load {
        session: u64,
        id: TrackId,
        entry: QueueEntryId,
        path: PathBuf,
    },
    Seek {
        session: u64,
        position: Duration,
    },
    Preload {
        id: TrackId,
        entry: QueueEntryId,
        path: PathBuf,
    },
    CancelPreload,
    Play,
    Pause,
    Stop,
    Shutdown,
}

/// State shared between the handle, worker, and audio callback.
/// Atomics only — the callback path never locks.
struct Shared {
    /// When false, the callback emits silence (paused/stopped/loading).
    gate_open: AtomicBool,
    /// Playback gain, `f32` bits, in `0.0..=1.0`.
    gain_bits: AtomicU32,
    /// Bumped on every load/stop; the callback flushes on change.
    generation: AtomicU64,
    /// Callback → worker: the newest generation whose ring flush the
    /// callback has completed. Invariant: the worker pushes samples
    /// for a generation only after the callback has acknowledged that
    /// generation, so the flush-on-generation-change can only ever
    /// discard *previous*-generation samples — never the head of the
    /// track that was just loaded.
    acked_generation: AtomicU64,
    /// Worker → callback: decoder reached EOF and all converted
    /// samples have been pushed to the ring.
    decoder_eof: AtomicBool,
    /// Callback → worker: EOF was reached and the ring has drained.
    drained: AtomicBool,
    /// Device audio frames actually delivered for the current generation.
    played_frames: AtomicU64,
    /// Output frames pushed into the ring for the current generation.
    /// This is a pipeline diagnostic; consumed position still comes only
    /// from `played_frames`.
    pushed_frames: AtomicU64,
    /// Output-frame position within the generation where the currently
    /// reported source began. Zero after load/seek; advanced only when a
    /// natural transition actually reaches the output boundary.
    source_start_frame: AtomicU64,
    /// Output frames represented by an earlier seek. Reset on load/stop;
    /// preserved across the callback's generation flush so a seek is
    /// reflected immediately in [`EngineHandle::position`].
    base_frames: AtomicU64,
    /// Requested natural-transition crossfade duration in milliseconds.
    /// Zero disables crossfade.
    crossfade_ms: AtomicU32,
}

impl Shared {
    fn new() -> Self {
        Self {
            gate_open: AtomicBool::new(false),
            gain_bits: AtomicU32::new(1.0f32.to_bits()),
            generation: AtomicU64::new(0),
            acked_generation: AtomicU64::new(0),
            decoder_eof: AtomicBool::new(false),
            drained: AtomicBool::new(false),
            played_frames: AtomicU64::new(0),
            pushed_frames: AtomicU64::new(0),
            source_start_frame: AtomicU64::new(0),
            base_frames: AtomicU64::new(0),
            crossfade_ms: AtomicU32::new(0),
        }
    }

    /// Starts a new generation (worker thread only): silences the
    /// callback and arms its flush-on-generation-change.
    fn bump_generation(&self) -> u64 {
        self.gate_open.store(false, Ordering::Release);
        self.decoder_eof.store(false, Ordering::Release);
        self.drained.store(false, Ordering::Release);
        self.pushed_frames.store(0, Ordering::Release);
        self.source_start_frame.store(0, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// GUI-side tail of the lock-free waveform tap.
///
/// The audio callback owns the producer side; this reader is touched
/// only by the UI thread. It keeps the most recent [`WAVEFORM_WINDOW`]
/// mono samples and is allowed to drop older samples when the UI is
/// slower than the device.
struct WaveformReader {
    consumer: HeapCons<f32>,
    window: VecDeque<f32>,
}

impl WaveformReader {
    fn new(consumer: HeapCons<f32>) -> Self {
        Self {
            consumer,
            window: VecDeque::with_capacity(WAVEFORM_WINDOW),
        }
    }

    fn snapshot(&mut self) -> [f32; WAVEFORM_WINDOW] {
        while let Some(sample) = self.consumer.try_pop() {
            if self.window.len() == WAVEFORM_WINDOW {
                self.window.pop_front();
            }
            self.window.push_back(sample);
        }

        let mut snapshot = [0.0; WAVEFORM_WINDOW];
        let start = WAVEFORM_WINDOW.saturating_sub(self.window.len());
        for (target, sample) in snapshot[start..].iter_mut().zip(&self.window) {
            *target = *sample;
        }
        snapshot
    }
}

/// The callback-side sample source living inside the backend.
/// Real-time safe: pops from the lock-free ring, applies gain, counts
/// delivered frames, and outputs silence whenever gated or starved.
struct SampleSource {
    consumer: ringbuf::HeapCons<f32>,
    waveform: HeapProd<f32>,
    shared: Arc<Shared>,
    channels: usize,
    seen_generation: u64,
}

impl FillCallback for SampleSource {
    fn fill(&mut self, out: &mut [f32]) {
        let generation = self.shared.generation.load(Ordering::Acquire);
        if generation != self.seen_generation {
            // Track changed or stopped: discard everything buffered
            // from the old generation before producing any sound. The
            // worker never pushes samples for a generation until it is
            // acknowledged below, so this flush cannot discard samples
            // belonging to the new generation (see
            // `Shared::acked_generation`).
            self.seen_generation = generation;
            self.consumer.clear();
            self.shared.played_frames.store(0, Ordering::Release);
            // Acknowledge only *after* the flush: from this store on,
            // the worker may push this generation's samples and they
            // are guaranteed to survive.
            self.shared
                .acked_generation
                .store(generation, Ordering::Release);
            out.fill(0.0);
            self.tap(out);
            return;
        }
        if !self.shared.gate_open.load(Ordering::Acquire) {
            out.fill(0.0);
            self.tap(out);
            return;
        }
        let popped = self.consumer.pop_slice(out);
        out[popped..].fill(0.0);
        let gain = f32::from_bits(self.shared.gain_bits.load(Ordering::Relaxed));
        if gain != 1.0 {
            for sample in &mut out[..popped] {
                *sample *= gain;
            }
        }
        let frames = (popped / self.channels) as u64;
        if frames > 0 {
            self.shared
                .played_frames
                .fetch_add(frames, Ordering::Relaxed);
        }
        if self.shared.decoder_eof.load(Ordering::Acquire) && self.consumer.is_empty() {
            self.shared.drained.store(true, Ordering::Release);
        }
        self.tap(out);
    }
}

impl SampleSource {
    /// Publishes the final post-gain, post-remix output as a mono
    /// stream. Real-time safe: no allocation and no blocking; excess
    /// samples are simply dropped when the GUI has not read recently.
    fn tap(&mut self, out: &[f32]) {
        let channels = self.channels.max(1);
        for frame in out.chunks_exact(channels) {
            let mono = if channels == 1 {
                frame[0]
            } else {
                frame.iter().sum::<f32>() / channels as f32
            };
            let _ = self.waveform.try_push(mono);
        }
    }
}

/// Handle to a running playback engine. All methods are non-blocking
/// (channel sends and atomic reads) and safe to call from a UI thread.
pub struct EngineHandle {
    commands: Sender<Command>,
    events: Receiver<EngineEvent>,
    shared: Arc<Shared>,
    waveform: Arc<Mutex<WaveformReader>>,
    config: OutputConfig,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Monotonic load-session allocator. A new value is minted for every
    /// load request so delayed events can be correlated even when the
    /// same track is loaded repeatedly.
    next_session: AtomicU64,
}

impl EngineHandle {
    /// Spawns the engine worker, constructing the backend **on the
    /// worker thread** via `make_backend` (some platforms pin audio
    /// streams to their creating thread) and starting it.
    ///
    /// This is the one handle operation that blocks: it waits for the
    /// backend to open (device setup) so configuration is known and
    /// startup failures are reported synchronously.
    ///
    /// # Errors
    ///
    /// [`EngineError::Output`] if the backend cannot be created or
    /// started (for example, no output device exists).
    pub fn spawn<F>(make_backend: F) -> Result<Self, EngineError>
    where
        F: FnOnce() -> Result<Box<dyn OutputBackend>, EngineError> + Send + 'static,
    {
        let (command_tx, command_rx) = std::sync::mpsc::channel::<Command>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<EngineEvent>();
        let (init_tx, init_rx) =
            std::sync::mpsc::sync_channel::<Result<OutputConfig, EngineError>>(1);
        let shared = Arc::new(Shared::new());
        let worker_shared = Arc::clone(&shared);
        let (waveform_tx, waveform_rx) = HeapRb::<f32>::new(WAVEFORM_RING_CAPACITY).split();
        let waveform = Arc::new(Mutex::new(WaveformReader::new(waveform_rx)));
        let worker = std::thread::Builder::new()
            .name("flux-audio-engine".to_owned())
            .spawn(move || {
                run_worker(
                    make_backend,
                    command_rx,
                    event_tx,
                    init_tx,
                    worker_shared,
                    waveform_tx,
                )
            })
            .map_err(|err| EngineError::Output(format!("could not spawn engine thread: {err}")))?;
        match init_rx.recv() {
            Ok(Ok(config)) => Ok(Self {
                commands: command_tx,
                events: event_rx,
                shared,
                waveform,
                config,
                worker: Some(worker),
                // 0 is reserved for "no session"; real sessions start at 1.
                next_session: AtomicU64::new(1),
            }),
            Ok(Err(err)) => {
                let _ = worker.join();
                Err(err)
            }
            Err(_) => {
                let _ = worker.join();
                Err(EngineError::Output(
                    "engine thread exited during startup".to_owned(),
                ))
            }
        }
    }

    /// Spawns an engine on the device-free [`NullOutput`] backend and
    /// returns its [`NullControl`] driver — the deterministic setup for
    /// headless tests and CI.
    #[must_use]
    pub fn spawn_null(sample_rate: u32, channels: u16) -> (Self, NullControl) {
        let (backend, control) = NullOutput::new(sample_rate, channels);
        let handle = Self::spawn(move || Ok(Box::new(backend) as Box<dyn OutputBackend>))
            .expect("the null backend cannot fail to start");
        (handle, control)
    }

    /// Spawns an engine on the default CPAL output device.
    ///
    /// # Errors
    ///
    /// [`EngineError::Output`] when no default device exists or the
    /// stream cannot be opened/started.
    #[cfg(feature = "cpal")]
    pub fn spawn_cpal_default() -> Result<Self, EngineError> {
        Self::spawn(|| -> Result<Box<dyn OutputBackend>, EngineError> {
            Ok(Box::new(crate::cpal_output::CpalOutput::open_default()?))
        })
    }

    /// The backend's output format, fixed for the engine's lifetime.
    #[must_use]
    pub fn output_config(&self) -> OutputConfig {
        self.config
    }

    /// The event stream. Disconnects after shutdown.
    #[must_use]
    pub fn events(&self) -> &Receiver<EngineEvent> {
        &self.events
    }

    /// Loads `path` as track `id`, replacing anything currently loaded
    /// (buffered samples of the old track are discarded). Does not
    /// start playback; listen for [`EngineEvent::Loaded`] (or
    /// [`EngineEvent::Error`]) and then call [`play`](Self::play).
    ///
    /// Returns the monotonically increasing **load session** that will
    /// appear on events for this attempt. Comparing sessions lets a
    /// caller ignore delayed events from an earlier load of the same
    /// track.
    pub fn load(&self, id: TrackId, path: PathBuf) -> u64 {
        self.load_entry(id, QueueEntryId::new(0), path)
    }

    /// Queue-entry-aware load. The stable entry identity is carried
    /// through load, preload and transition events so duplicate TrackIds
    /// cannot be confused during a natural handoff.
    pub fn load_entry(&self, id: TrackId, entry: QueueEntryId, path: PathBuf) -> u64 {
        let session = self.next_session.fetch_add(1, Ordering::Relaxed);
        assert!(
            session != 0,
            "playback load-session identifier exhausted after 2^64 loads"
        );
        self.shared.base_frames.store(0, Ordering::Release);
        self.shared.played_frames.store(0, Ordering::Release);
        self.shared.source_start_frame.store(0, Ordering::Release);
        let _ = self.commands.send(Command::Load {
            session,
            id,
            entry,
            path,
        });
        session
    }

    /// Prepares a future source in the worker without changing current
    /// playback. At most one prepared decoder is retained; a new preload or
    /// cancellation replaces it. A subsequent matching [`load`](Self::load)
    /// can consume the prepared decoder.
    pub fn preload(&self, id: TrackId, path: PathBuf) {
        self.preload_entry(id, QueueEntryId::new(0), path);
    }

    /// Queue-entry-aware preparation. The engine will only consume this
    /// decoder at a natural end-of-stream when its entry and source path
    /// still match the active pipeline's authorized successor.
    pub fn preload_entry(&self, id: TrackId, entry: QueueEntryId, path: PathBuf) {
        let _ = self.commands.send(Command::Preload { id, entry, path });
    }

    /// Drops any prepared future decoder.
    pub fn cancel_preload(&self) {
        let _ = self.commands.send(Command::CancelPreload);
    }

    /// Sets the crossfade duration for subsequent natural transitions.
    /// Values are clamped to `0..=MAX_CROSSFADE_MS`; zero disables
    /// crossfade and preserves gapless behavior. A change does not
    /// retroactively resize an in-flight source's already-computed tail
    /// window. Returns the effective duration.
    pub fn set_crossfade_ms(&self, milliseconds: u64) -> u64 {
        let effective = milliseconds.min(MAX_CROSSFADE_MS);
        self.shared
            .crossfade_ms
            .store(effective as u32, Ordering::Release);
        effective
    }

    /// The configured natural-transition crossfade duration.
    #[must_use]
    pub fn crossfade_ms(&self) -> u64 {
        u64::from(self.shared.crossfade_ms.load(Ordering::Acquire))
    }

    /// Starts or resumes playback of the loaded track. If the track
    /// previously played to completion, it restarts from the beginning.
    /// Emits [`EngineEvent::Started`].
    ///
    /// If no track is loaded because the most recent load failed, the
    /// original load error is the only failure reported; a redundant
    /// [`EngineError::NoTrackLoaded`] is not emitted. A play request
    /// with nothing loaded *and* no preceding load failure still reports
    /// [`EngineError::NoTrackLoaded`].
    pub fn play(&self) {
        let _ = self.commands.send(Command::Play);
    }

    /// Pauses playback (position is retained). A no-op unless playing.
    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }

    /// Stops playback and unloads the current track.
    pub fn stop(&self) {
        self.shared.base_frames.store(0, Ordering::Release);
        self.shared.played_frames.store(0, Ordering::Release);
        self.shared.source_start_frame.store(0, Ordering::Release);
        let _ = self.commands.send(Command::Stop);
    }

    /// Seeks the loaded track to `position`.
    ///
    /// The return value is a new monotonic session for events emitted by
    /// the seeked playback. Delayed events from the previous position are
    /// ignored by the controller. The reported position changes
    /// immediately; the worker reopens and fast-forwards the decoder off
    /// the UI thread. Seeking while paused or stopped prepares the new
    /// position without starting playback.
    ///
    /// The controller owns duration clamping; this layer accepts any
    /// non-negative duration and safely reaches end-of-stream if it is
    /// beyond the file.
    #[must_use]
    pub fn seek(&self, position: Duration) -> u64 {
        let session = self.next_session.fetch_add(1, Ordering::Relaxed);
        assert!(
            session != 0,
            "playback session identifier exhausted after 2^64 requests"
        );
        let frames = (position.as_secs_f64() * f64::from(self.config.sample_rate)).round() as u64;
        self.shared.base_frames.store(frames, Ordering::Release);
        self.shared.played_frames.store(0, Ordering::Release);
        self.shared.source_start_frame.store(0, Ordering::Release);
        let _ = self.commands.send(Command::Seek { session, position });
        session
    }

    /// Sets the playback gain, clamped to `0.0..=1.0` (`NaN` counts as
    /// 0). Applied immediately in the audio callback.
    pub fn set_volume(&self, volume: f32) {
        let volume = if volume.is_nan() {
            0.0
        } else {
            volume.clamp(0.0, 1.0)
        };
        self.shared
            .gain_bits
            .store(volume.to_bits(), Ordering::Relaxed);
    }

    /// Elapsed playback position of the current track, derived from
    /// device frames actually delivered. Resets on load/stop.
    #[must_use]
    pub fn position(&self) -> Duration {
        if self.config.sample_rate == 0 {
            return Duration::ZERO;
        }
        let played = self.shared.played_frames.load(Ordering::Relaxed);
        let source_start = self.shared.source_start_frame.load(Ordering::Relaxed);
        let frames = self
            .shared
            .base_frames
            .load(Ordering::Relaxed)
            .saturating_add(played.saturating_sub(source_start));
        Duration::from_secs_f64(frames as f64 / f64::from(self.config.sample_rate))
    }

    /// The latest post-gain, mono output samples for visualization.
    ///
    /// This is the same signal that reaches the output device, captured
    /// in the real-time callback through a lock-free producer. The
    /// returned window is always [`WAVEFORM_WINDOW`] samples long,
    /// zero-padded while playback has not produced a full window yet.
    #[must_use]
    pub fn waveform(&self) -> [f32; WAVEFORM_WINDOW] {
        let mut reader = match self.waveform.lock() {
            Ok(reader) => reader,
            Err(poisoned) => poisoned.into_inner(),
        };
        reader.snapshot()
    }

    /// Stops the backend and joins the worker thread.
    pub fn shutdown(mut self) {
        self.shutdown_inner();
    }

    fn shutdown_inner(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = self.commands.send(Command::Shutdown);
            let _ = worker.join();
        }
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.shutdown_inner();
    }
}

/// The currently loaded track on the worker side.
struct LoadedTrack {
    id: TrackId,
    entry: QueueEntryId,
    path: PathBuf,
    info: StreamInfo,
}

struct PreparedTrack {
    id: TrackId,
    entry: QueueEntryId,
    path: PathBuf,
    info: StreamInfo,
    decoder: Box<dyn Decoder>,
}

/// Bounded tail staging for one source. The last `hold_frames` output
/// frames are retained in a decoder-side buffer so a natural handoff can
/// mix them with the prepared successor before anything is pushed.
#[derive(Debug, Clone, Copy, Default)]
struct Staging {
    produced_frames: u64,
    hold_start_frame: u64,
    hold_frames: u64,
    holding: bool,
}

impl Staging {
    fn reset(
        info: &StreamInfo,
        config: OutputConfig,
        fade_frames: u64,
        produced_frames: u64,
    ) -> Self {
        let output_rate = f64::from(config.sample_rate);
        let total_output_frames = info.duration.map_or(0, |duration| {
            (duration.as_secs_f64() * output_rate).round() as u64
        });
        let prebuffer_frames =
            u64::from(config.sample_rate) * u64::from(CROSSFADE_PREBUFFER_SECONDS);
        if fade_frames == 0 || total_output_frames == 0 || total_output_frames <= prebuffer_frames {
            return Self {
                produced_frames,
                ..Self::default()
            };
        }
        let hold_frames = fade_frames.min(total_output_frames - prebuffer_frames);
        let hold_start_frame = total_output_frames - hold_frames;
        if hold_frames == 0 || produced_frames >= hold_start_frame {
            // Once a source has already advanced into its would-be tail
            // (for example after a late seek), do not pretend those
            // frames can be held.
            return Self {
                produced_frames,
                ..Self::default()
            };
        }
        Self {
            produced_frames,
            hold_start_frame,
            hold_frames,
            holding: false,
        }
    }
}

/// Splits newly converted outgoing output into immediately-pushable
/// samples and the bounded tail retained for the next crossfade. With
/// `hold_frames == 0` this is a no-copy pass-through.
fn stage_outgoing_output(
    output: Vec<f32>,
    channels: usize,
    staging: &mut Staging,
    tail: &mut VecDeque<f32>,
) -> Vec<f32> {
    let channels = channels.max(1);
    let frames = output.len() / channels;
    let produced_before = staging.produced_frames;
    staging.produced_frames = staging.produced_frames.saturating_add(frames as u64);
    if staging.hold_frames == 0 {
        return output;
    }

    let mut pushable = Vec::new();
    if staging.holding {
        tail.extend(output);
    } else {
        let split = staging
            .hold_start_frame
            .saturating_sub(produced_before)
            .min(frames as u64) as usize;
        let split_samples = split * channels;
        pushable.extend_from_slice(&output[..split_samples]);
        tail.extend(&output[split_samples..]);
        if split < frames {
            staging.holding = true;
        }
    }

    let excess_frames = (tail.len() / channels).saturating_sub(staging.hold_frames as usize);
    for _ in 0..excess_frames {
        for _ in 0..channels {
            if let Some(sample) = tail.pop_front() {
                pushable.push(sample);
            }
        }
    }
    pushable
}

fn crossfade_frames(milliseconds: u64, sample_rate: u32) -> u64 {
    if milliseconds == 0 {
        return 0;
    }
    (milliseconds.saturating_mul(u64::from(sample_rate)) / 1_000).max(1)
}

struct CrossfadeOutcome {
    incoming: LoadedTrack,
    decoder: Box<dyn Decoder>,
    converter: FormatConverter,
    mixed: Vec<f32>,
    surplus: Vec<f32>,
    overlap_frames: u64,
}

enum CrossfadeAttempt {
    Mixed(CrossfadeOutcome),
    Fallback(PreparedTrack),
}

/// Equal-power crossfade of the retained outgoing tail with the first
/// `N` incoming output frames. Frame 0 has gains (1, 0); the final frame
/// has (0, 1). With one overlap frame the incoming source wins. Mixed
/// samples are clamped to the interleaved f32 output range so correlated
/// full-scale sources cannot exceed the backend's nominal range.
fn mix_equal_power_tail(
    tail: &mut VecDeque<f32>,
    incoming_pcm: &[f32],
    channels: usize,
) -> (Vec<f32>, usize) {
    let channels = channels.max(1);
    let overlap_frames = tail.len() / channels;
    let mut mixed = Vec::with_capacity(overlap_frames * channels);
    let denom = overlap_frames.saturating_sub(1).max(1) as f32;
    for frame in 0..overlap_frames {
        let theta = if overlap_frames == 1 {
            std::f32::consts::FRAC_PI_2
        } else {
            (frame as f32 / denom) * std::f32::consts::FRAC_PI_2
        };
        let outgoing_gain = theta.cos();
        let incoming_gain = theta.sin();
        for channel in 0..channels {
            let outgoing = tail.pop_front().unwrap_or(0.0);
            let incoming = incoming_pcm
                .get(frame * channels + channel)
                .copied()
                .unwrap_or(0.0);
            let sample = outgoing * outgoing_gain + incoming * incoming_gain;
            mixed.push(sample.clamp(-1.0, 1.0));
        }
    }
    (mixed, overlap_frames)
}

fn attempt_crossfade(
    prepared: PreparedTrack,
    tail: &mut VecDeque<f32>,
    config: OutputConfig,
) -> Result<CrossfadeAttempt, DecodeError> {
    let channels = usize::from(config.channels).max(1);
    let overlap_frames = tail.len() / channels;
    if overlap_frames == 0 {
        return Ok(CrossfadeAttempt::Fallback(prepared));
    }
    let total_incoming_frames = prepared.info.duration.map_or(0, |duration| {
        (duration.as_secs_f64() * f64::from(config.sample_rate)).round() as u64
    });
    if total_incoming_frames == 0 || total_incoming_frames < overlap_frames as u64 {
        return Ok(CrossfadeAttempt::Fallback(prepared));
    }

    let PreparedTrack {
        id,
        entry,
        path,
        info,
        mut decoder,
    } = prepared;
    let mut converter = FormatConverter::new(&info, config);
    let mut incoming_pcm: Vec<f32> = Vec::new();
    loop {
        let available_frames = incoming_pcm.len() / channels;
        if available_frames >= overlap_frames {
            break;
        }
        match decoder.next_chunk(DECODE_CHUNK_FRAMES) {
            Ok(samples) if samples.is_empty() => {
                // Metadata promised enough samples but the decoder hit
                // EOF early. Reopen from the start and fall back to the
                // gapless path rather than mixing incomplete audio.
                drop(decoder);
                let decoder = open_decoder(&path)?;
                let info = *decoder.info();
                return Ok(CrossfadeAttempt::Fallback(PreparedTrack {
                    id,
                    entry,
                    path,
                    info,
                    decoder,
                }));
            }
            Ok(samples) => {
                incoming_pcm.extend(converter.process(&samples));
            }
            Err(error) => return Err(error),
        }
    }

    let mixed_samples = overlap_frames * channels;
    let (mixed, _) = mix_equal_power_tail(tail, &incoming_pcm, channels);
    let surplus = incoming_pcm.split_off(mixed_samples);
    Ok(CrossfadeAttempt::Mixed(CrossfadeOutcome {
        incoming: LoadedTrack {
            id,
            entry,
            path,
            info,
        },
        decoder,
        converter,
        mixed,
        surplus,
        overlap_frames: overlap_frames as u64,
    }))
}

struct PendingTransition {
    from_id: TrackId,
    from_entry: QueueEntryId,
    to_id: TrackId,
    to_entry: QueueEntryId,
    boundary_frame: u64,
    info: StreamInfo,
}

/// Reopens the currently audible source at its present source-relative
/// position after a queue mutation invalidated an already-installed
/// (but not yet promoted) handoff. The generation bump flushes both the
/// old tail and the obsolete successor; the caller then prepares the
/// newly authorized successor. This path may introduce silence, unlike
/// a valid natural handoff.
#[allow(clippy::too_many_arguments)]
fn resume_current_after_invalidation(
    shared: &Shared,
    config: OutputConfig,
    current: &LoadedTrack,
    playing: bool,
    decoder: &mut Option<Box<dyn Decoder>>,
    converter: &mut Option<FormatConverter>,
    pending: &mut Vec<f32>,
    pending_sent: &mut usize,
    incoming: &mut Option<LoadedTrack>,
    pending_transition: &mut Option<PendingTransition>,
    eof_reached: &mut bool,
    ended_sent: &mut bool,
    staging: &mut Staging,
    tail: &mut VecDeque<f32>,
    current_generation: &mut u64,
) -> Result<(), DecodeError> {
    let output_rate = f64::from(config.sample_rate);
    let source_rate = f64::from(current.info.sample_rate);
    let position_frames = shared.base_frames.load(Ordering::Acquire).saturating_add(
        shared
            .played_frames
            .load(Ordering::Acquire)
            .saturating_sub(shared.source_start_frame.load(Ordering::Acquire)),
    );
    let position = Duration::from_secs_f64(position_frames as f64 / output_rate);
    let target = current
        .info
        .duration
        .map_or(position, |duration| position.min(duration));
    let target_output_frames = (target.as_secs_f64() * output_rate).round() as u64;

    *current_generation = shared.bump_generation();
    shared
        .base_frames
        .store(target_output_frames, Ordering::Release);
    shared.played_frames.store(0, Ordering::Release);
    *decoder = None;
    *converter = None;
    pending.clear();
    *pending_sent = 0;
    *incoming = None;
    *pending_transition = None;
    *ended_sent = false;
    tail.clear();
    let fade = crossfade_frames(
        u64::from(shared.crossfade_ms.load(Ordering::Acquire)),
        config.sample_rate,
    );

    if current
        .info
        .duration
        .is_some_and(|duration| target >= duration)
    {
        *eof_reached = true;
        *staging = Staging::reset(&current.info, config, fade, target_output_frames);
        shared.gate_open.store(playing, Ordering::Release);
        return Ok(());
    }

    let mut dec = open_decoder(&current.path)?;
    let source_frames = (target.as_secs_f64() * source_rate).round() as u64;
    let channels = usize::from(current.info.channels);
    let mut skipped = 0u64;
    let mut hit_eof = false;
    while skipped < source_frames {
        let want = (source_frames - skipped)
            .min(DECODE_CHUNK_FRAMES as u64)
            .max(1) as usize;
        match dec.next_chunk(want) {
            Ok(samples) if samples.is_empty() => {
                hit_eof = true;
                break;
            }
            Ok(samples) => {
                let frames = samples.len() / channels;
                if frames == 0 {
                    hit_eof = true;
                    break;
                }
                skipped = skipped.saturating_add(frames as u64);
            }
            Err(error) => return Err(error),
        }
    }

    let actual_source_frames = skipped.min(source_frames);
    let actual_output_frames =
        (actual_source_frames as f64 / source_rate * output_rate).round() as u64;
    let actual_target_frames = if hit_eof {
        actual_output_frames
    } else {
        target_output_frames
    };
    shared
        .base_frames
        .store(actual_target_frames, Ordering::Release);
    *eof_reached = hit_eof;
    *converter = Some(FormatConverter::new(&current.info, config));
    *decoder = if hit_eof { None } else { Some(dec) };
    *staging = Staging::reset(&current.info, config, fade, actual_target_frames);
    shared.gate_open.store(playing, Ordering::Release);
    Ok(())
}

/// The worker loop. Owns the backend, decoder, converter, and ring
/// producer; everything else lives in [`Shared`] atomics.
fn run_worker<F>(
    make_backend: F,
    commands: Receiver<Command>,
    events: Sender<EngineEvent>,
    init: std::sync::mpsc::SyncSender<Result<OutputConfig, EngineError>>,
    shared: Arc<Shared>,
    waveform: HeapProd<f32>,
) where
    F: FnOnce() -> Result<Box<dyn OutputBackend>, EngineError>,
{
    let mut backend = match make_backend() {
        Ok(backend) => backend,
        Err(err) => {
            let _ = init.send(Err(err));
            return;
        }
    };
    let config = backend.config();

    // ~2 seconds of device-rate audio. The decoder refills in chunks;
    // the callback drains in device-sized pieces.
    let capacity = (config.sample_rate as usize * usize::from(config.channels) * 2).max(8192);
    let (mut producer, consumer) = HeapRb::<f32>::new(capacity).split();
    let source = SampleSource {
        consumer,
        waveform,
        channels: usize::from(config.channels),
        seen_generation: shared.generation.load(Ordering::Acquire),
        shared: Arc::clone(&shared),
    };
    let error_events = events.clone();
    let on_error = Box::new(move |message: String| {
        let _ = error_events.send(EngineEvent::Error {
            session: None,
            id: None,
            error: EngineError::Output(message),
        });
    });
    if let Err(err) = backend.start(Box::new(source), on_error) {
        let _ = init.send(Err(err));
        return;
    }
    if init.send(Ok(config)).is_err() {
        backend.stop();
        return;
    }

    let send = |event: EngineEvent| {
        let _ = events.send(event);
    };

    let mut decoder: Option<Box<dyn Decoder>> = None;
    let mut prepared: Option<PreparedTrack> = None;
    let mut current: Option<LoadedTrack> = None;
    let mut incoming: Option<LoadedTrack> = None;
    let mut pending_transition: Option<PendingTransition> = None;
    let mut converter: Option<FormatConverter> = None;
    let mut pending: Vec<f32> = Vec::new();
    let mut pending_sent = 0usize;
    let mut staging = Staging::default();
    let mut tail: VecDeque<f32> = VecDeque::new();
    let mut playing = false;
    let mut eof_reached = false;
    let mut ended_sent = false;
    // The generation this worker's decoder/pending state belongs to.
    // Only the worker bumps generations, so this mirror is exact.
    let mut current_generation = shared.generation.load(Ordering::Acquire);
    // The session of the most recent Load attempt. 0 means none.
    let mut current_session = 0u64;
    // A failed load already reported the meaningful error; a Play that
    // arrives immediately afterwards must not add NoTrackLoaded.
    let mut last_load_failed = false;
    let output_channels = usize::from(config.channels);

    'worker: loop {
        match commands.recv_timeout(WORKER_TICK) {
            Ok(Command::Load {
                session,
                id,
                entry,
                path,
            }) => {
                current_session = session;
                current_generation = shared.bump_generation();
                decoder = None;
                current = None;
                incoming = None;
                pending_transition = None;
                converter = None;
                pending.clear();
                pending_sent = 0;
                staging = Staging::default();
                tail.clear();
                playing = false;
                eof_reached = false;
                ended_sent = false;
                last_load_failed = false;
                let reusable = prepared.as_ref().is_some_and(|prepared| {
                    prepared.id == id && prepared.entry == entry && prepared.path == path
                });
                let opened = if reusable {
                    let prepared = prepared.take().expect("prepared exists");
                    Ok((prepared.decoder, prepared.info))
                } else {
                    prepared = None;
                    open_decoder(&path).map(|decoder| {
                        let info = *decoder.info();
                        (decoder, info)
                    })
                };
                match opened {
                    Ok((dec, info)) => {
                        converter = Some(FormatConverter::new(&info, config));
                        decoder = Some(dec);
                        current = Some(LoadedTrack {
                            id,
                            entry,
                            path,
                            info,
                        });
                        let fade = crossfade_frames(
                            u64::from(shared.crossfade_ms.load(Ordering::Acquire)),
                            config.sample_rate,
                        );
                        staging = Staging::reset(&info, config, fade, 0);
                        tail.clear();
                        send(EngineEvent::Loaded {
                            session,
                            id,
                            entry,
                            info,
                        });
                    }
                    Err(err) => {
                        last_load_failed = true;
                        send(EngineEvent::Error {
                            session: Some(session),
                            id: Some(id),
                            error: err.into(),
                        });
                    }
                }
            }
            Ok(Command::Seek { session, position }) => {
                let Some((id, path, info)) = current
                    .as_ref()
                    .map(|track| (track.id, track.path.clone(), track.info))
                else {
                    shared.base_frames.store(0, Ordering::Release);
                    shared.played_frames.store(0, Ordering::Release);
                    continue 'worker;
                };

                let total = info.duration;
                let target = total.map_or(position, |duration| position.min(duration));
                let output_rate = f64::from(config.sample_rate);
                let source_rate = f64::from(info.sample_rate);
                let target_output_frames = (target.as_secs_f64() * output_rate).round() as u64;

                current_session = session;
                current_generation = shared.bump_generation();
                shared
                    .base_frames
                    .store(target_output_frames, Ordering::Release);
                shared.played_frames.store(0, Ordering::Release);
                decoder = None;
                incoming = None;
                pending_transition = None;
                converter = None;
                pending.clear();
                pending_sent = 0;
                staging = Staging::default();
                tail.clear();
                ended_sent = false;
                last_load_failed = false;

                if total.is_some_and(|duration| target >= duration) {
                    // The requested point is the end of the track. If a
                    // prepared successor exists, the pump below will hand
                    // off to it; otherwise the normal drained path ends
                    // this source.
                    eof_reached = true;
                    let fade = crossfade_frames(
                        u64::from(shared.crossfade_ms.load(Ordering::Acquire)),
                        config.sample_rate,
                    );
                    staging = Staging::reset(&info, config, fade, target_output_frames);
                    shared.gate_open.store(playing, Ordering::Release);
                    continue 'worker;
                }

                let mut dec = match open_decoder(&path) {
                    Ok(decoder) => decoder,
                    Err(error) => {
                        playing = false;
                        shared.gate_open.store(false, Ordering::Release);
                        ended_sent = true;
                        eof_reached = true;
                        send(EngineEvent::Error {
                            session: Some(session),
                            id: Some(id),
                            error: error.into(),
                        });
                        continue 'worker;
                    }
                };

                let source_frames = (target.as_secs_f64() * source_rate).round() as u64;
                let channels = usize::from(info.channels);
                let mut skipped = 0u64;
                let mut hit_eof = false;
                while skipped < source_frames {
                    let want = (source_frames - skipped)
                        .min(DECODE_CHUNK_FRAMES as u64)
                        .max(1) as usize;
                    match dec.next_chunk(want) {
                        Ok(samples) if samples.is_empty() => {
                            hit_eof = true;
                            break;
                        }
                        Ok(samples) => {
                            let frames = samples.len() / channels;
                            if frames == 0 {
                                hit_eof = true;
                                break;
                            }
                            skipped = skipped.saturating_add(frames as u64);
                        }
                        Err(error) => {
                            playing = false;
                            shared.gate_open.store(false, Ordering::Release);
                            ended_sent = true;
                            eof_reached = true;
                            send(EngineEvent::Error {
                                session: Some(session),
                                id: Some(id),
                                error: error.into(),
                            });
                            continue 'worker;
                        }
                    }
                }

                let actual_source_frames = skipped.min(source_frames);
                let actual_output_frames =
                    (actual_source_frames as f64 / source_rate * output_rate).round() as u64;
                let actual_target_frames = if hit_eof {
                    actual_output_frames
                } else {
                    target_output_frames
                };
                shared
                    .base_frames
                    .store(actual_target_frames, Ordering::Release);
                eof_reached = hit_eof;
                decoder = Some(dec);
                converter = Some(FormatConverter::new(&info, config));
                if hit_eof {
                    // The decoder is already at end-of-stream. The pump
                    // below either hands off to a prepared successor or
                    // lets the normal drained path end this source.
                    decoder = None;
                }
                let fade = crossfade_frames(
                    u64::from(shared.crossfade_ms.load(Ordering::Acquire)),
                    config.sample_rate,
                );
                staging = Staging::reset(&info, config, fade, actual_target_frames);
                shared.gate_open.store(playing, Ordering::Release);
            }
            Ok(Command::Preload { id, entry, path }) => {
                let incoming_matches = incoming.as_ref().is_some_and(|track| {
                    track.id == id && track.entry == entry && track.path == path
                });
                if pending_transition.is_some() && incoming_matches {
                    // The newly authorized successor is exactly the one
                    // already installed. Keep it and do not open a
                    // redundant decoder.
                    continue 'worker;
                }
                if pending_transition.is_some() && incoming.is_some() {
                    let Some(track) = current.as_ref() else {
                        continue 'worker;
                    };
                    if let Err(error) = resume_current_after_invalidation(
                        &shared,
                        config,
                        track,
                        playing,
                        &mut decoder,
                        &mut converter,
                        &mut pending,
                        &mut pending_sent,
                        &mut incoming,
                        &mut pending_transition,
                        &mut eof_reached,
                        &mut ended_sent,
                        &mut staging,
                        &mut tail,
                        &mut current_generation,
                    ) {
                        playing = false;
                        shared.gate_open.store(false, Ordering::Release);
                        ended_sent = true;
                        eof_reached = true;
                        prepared = None;
                        send(EngineEvent::Error {
                            session: Some(current_session),
                            id: Some(track.id),
                            error: error.into(),
                        });
                        continue 'worker;
                    }
                }
                prepared = None;
                match open_decoder(&path) {
                    Ok(decoder) => {
                        let info = *decoder.info();
                        prepared = Some(PreparedTrack {
                            id,
                            entry,
                            path,
                            info,
                            decoder,
                        });
                        send(EngineEvent::Preloaded { id, entry, info });
                    }
                    Err(error) => {
                        send(EngineEvent::PreloadFailed {
                            id,
                            entry,
                            error: error.into(),
                        });
                    }
                }
            }
            Ok(Command::CancelPreload) => {
                prepared = None;
                if pending_transition.is_some() && incoming.is_some() {
                    let Some(track) = current.as_ref() else {
                        continue 'worker;
                    };
                    if let Err(error) = resume_current_after_invalidation(
                        &shared,
                        config,
                        track,
                        playing,
                        &mut decoder,
                        &mut converter,
                        &mut pending,
                        &mut pending_sent,
                        &mut incoming,
                        &mut pending_transition,
                        &mut eof_reached,
                        &mut ended_sent,
                        &mut staging,
                        &mut tail,
                        &mut current_generation,
                    ) {
                        playing = false;
                        shared.gate_open.store(false, Ordering::Release);
                        ended_sent = true;
                        eof_reached = true;
                        send(EngineEvent::Error {
                            session: Some(current_session),
                            id: Some(track.id),
                            error: error.into(),
                        });
                    }
                }
            }
            Ok(Command::Play) => match &current {
                None => {
                    if !last_load_failed {
                        send(EngineEvent::Error {
                            session: None,
                            id: None,
                            error: EngineError::NoTrackLoaded,
                        });
                    }
                }
                Some(track) => {
                    if decoder.is_none() && incoming.is_none() {
                        // The track previously ended (or errored):
                        // pressing play restarts it from the beginning.
                        current_generation = shared.bump_generation();
                        shared.base_frames.store(0, Ordering::Release);
                        shared.played_frames.store(0, Ordering::Release);
                        pending.clear();
                        pending_sent = 0;
                        eof_reached = false;
                        ended_sent = false;
                        pending_transition = None;
                        tail.clear();
                        match open_decoder(&track.path) {
                            Ok(dec) => {
                                converter = Some(FormatConverter::new(&track.info, config));
                                decoder = Some(dec);
                                let fade = crossfade_frames(
                                    u64::from(shared.crossfade_ms.load(Ordering::Acquire)),
                                    config.sample_rate,
                                );
                                staging = Staging::reset(&track.info, config, fade, 0);
                            }
                            Err(err) => {
                                send(EngineEvent::Error {
                                    session: Some(current_session),
                                    id: Some(track.id),
                                    error: err.into(),
                                });
                                continue 'worker;
                            }
                        }
                    }
                    playing = true;
                    shared.gate_open.store(true, Ordering::Release);
                    send(EngineEvent::Started {
                        session: current_session,
                        id: track.id,
                    });
                }
            },
            Ok(Command::Pause) => {
                if playing {
                    playing = false;
                    shared.gate_open.store(false, Ordering::Release);
                    if let Some(track) = &current {
                        send(EngineEvent::Paused {
                            session: current_session,
                            id: track.id,
                        });
                    }
                }
            }
            Ok(Command::Stop) => {
                current_generation = shared.bump_generation();
                shared.base_frames.store(0, Ordering::Release);
                shared.played_frames.store(0, Ordering::Release);
                current_session = 0;
                last_load_failed = false;
                decoder = None;
                prepared = None;
                current = None;
                incoming = None;
                pending_transition = None;
                converter = None;
                pending.clear();
                pending_sent = 0;
                staging = Staging::default();
                tail.clear();
                playing = false;
                eof_reached = false;
                ended_sent = false;
                send(EngineEvent::Stopped);
            }
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break 'worker,
            Err(RecvTimeoutError::Timeout) => {}
        }

        // Pump converted samples into the ring — but only once the
        // callback has flushed and acknowledged the current generation
        // (`Shared::acked_generation`): anything pushed before that
        // acknowledgement could still be wiped by the pending flush.
        // Command handling above is deliberately *not* gated, so
        // load/stop/shutdown stay responsive even if a backend never
        // calls back. Decoding continues while paused (the bounded
        // ring applies backpressure), so resuming is instant; only an
        // absent/exhausted decoder idles.
        let generation_live = shared.acked_generation.load(Ordering::Acquire) == current_generation;
        if generation_live {
            // Several small state advances can happen in one tick. In
            // particular, a prepared decoder is installed only after
            // the old source's converter tail has been pushed, and the
            // new source may then decode immediately into the same
            // ring and generation.
            'pump: loop {
                let mut progressed = false;
                if pending_sent < pending.len() {
                    let pushed = producer.push_slice(&pending[pending_sent..]);
                    if pushed > 0 {
                        pending_sent += pushed;
                        let frames = (pushed / output_channels) as u64;
                        shared.pushed_frames.fetch_add(frames, Ordering::Relaxed);
                        progressed = true;
                    }
                    if pending_sent == pending.len() {
                        pending.clear();
                        pending_sent = 0;
                    }
                } else if eof_reached && incoming.is_none() {
                    let fade = crossfade_frames(
                        u64::from(shared.crossfade_ms.load(Ordering::Acquire)),
                        config.sample_rate,
                    );
                    if !tail.is_empty() {
                        if fade > 0
                            && let Some(next) = prepared.take()
                        {
                            match attempt_crossfade(next, &mut tail, config) {
                                Ok(CrossfadeAttempt::Mixed(outcome)) => {
                                    let Some(from) = current.as_ref() else {
                                        prepared = None;
                                        tail.clear();
                                        break 'pump;
                                    };
                                    let boundary_frame =
                                        shared.pushed_frames.load(Ordering::Acquire);
                                    let surplus_frames =
                                        (outcome.surplus.len() / output_channels) as u64;
                                    staging = Staging::reset(
                                        &outcome.incoming.info,
                                        config,
                                        fade,
                                        outcome.overlap_frames + surplus_frames,
                                    );
                                    tail.clear();
                                    converter = Some(outcome.converter);
                                    decoder = Some(outcome.decoder);
                                    incoming = Some(outcome.incoming);
                                    pending = outcome.mixed;
                                    pending.extend(outcome.surplus);
                                    pending_sent = 0;
                                    eof_reached = false;
                                    ended_sent = false;
                                    shared.decoder_eof.store(false, Ordering::Release);
                                    shared.drained.store(false, Ordering::Release);
                                    pending_transition = Some(PendingTransition {
                                        from_id: from.id,
                                        from_entry: from.entry,
                                        to_id: incoming.as_ref().expect("incoming set").id,
                                        to_entry: incoming.as_ref().expect("incoming set").entry,
                                        boundary_frame,
                                        info: incoming.as_ref().expect("incoming set").info,
                                    });
                                    progressed = true;
                                }
                                Ok(CrossfadeAttempt::Fallback(next)) => {
                                    prepared = Some(next);
                                    pending.extend(tail.drain(..));
                                    progressed = true;
                                }
                                Err(error) => {
                                    let source_id = current.as_ref().map(|track| track.id);
                                    send(EngineEvent::Error {
                                        session: Some(current_session),
                                        id: source_id,
                                        error: error.into(),
                                    });
                                    decoder = None;
                                    prepared = None;
                                    incoming = None;
                                    pending_transition = None;
                                    converter = None;
                                    pending.clear();
                                    pending_sent = 0;
                                    staging = Staging::default();
                                    tail.clear();
                                    playing = false;
                                    shared.gate_open.store(false, Ordering::Release);
                                    ended_sent = true;
                                    eof_reached = true;
                                    break 'pump;
                                }
                            }
                        } else {
                            // No in-flight crossfade (disabled, no prepared
                            // successor, or no eligible tail): preserve the
                            // existing gapless/fallback path.
                            pending.extend(tail.drain(..));
                            progressed = true;
                        }
                    } else {
                        let Some(next) = prepared.take() else {
                            break 'pump;
                        };
                        let Some(from) = current.as_ref() else {
                            prepared = Some(next);
                            break 'pump;
                        };
                        let boundary_frame = shared.pushed_frames.load(Ordering::Acquire);
                        let transition = PendingTransition {
                            from_id: from.id,
                            from_entry: from.entry,
                            to_id: next.id,
                            to_entry: next.entry,
                            boundary_frame,
                            info: next.info,
                        };
                        converter = Some(FormatConverter::new(&next.info, config));
                        decoder = Some(next.decoder);
                        incoming = Some(LoadedTrack {
                            id: next.id,
                            entry: next.entry,
                            path: next.path,
                            info: next.info,
                        });
                        staging = Staging::reset(&next.info, config, fade, 0);
                        tail.clear();
                        pending.clear();
                        pending_sent = 0;
                        eof_reached = false;
                        ended_sent = false;
                        shared.decoder_eof.store(false, Ordering::Release);
                        shared.drained.store(false, Ordering::Release);
                        pending_transition = Some(transition);
                        progressed = true;
                    }
                } else if !eof_reached && pending.is_empty() {
                    let Some(dec) = decoder.as_mut() else {
                        break 'pump;
                    };
                    let chunk_samples = DECODE_CHUNK_FRAMES * output_channels;
                    if producer.vacant_len() < chunk_samples {
                        break 'pump;
                    }
                    match dec.next_chunk(DECODE_CHUNK_FRAMES) {
                        Ok(samples) if samples.is_empty() => {
                            if let Some(conv) = converter.as_mut() {
                                let output = conv.finish();
                                pending = stage_outgoing_output(
                                    output,
                                    output_channels,
                                    &mut staging,
                                    &mut tail,
                                );
                            }
                            eof_reached = true;
                            progressed = true;
                        }
                        Ok(samples) => {
                            if let Some(conv) = converter.as_mut() {
                                let output = conv.process(&samples);
                                pending = stage_outgoing_output(
                                    output,
                                    output_channels,
                                    &mut staging,
                                    &mut tail,
                                );
                            }
                            progressed = true;
                        }
                        Err(err) => {
                            let source_id = incoming
                                .as_ref()
                                .map(|track| track.id)
                                .or_else(|| current.as_ref().map(|track| track.id));
                            send(EngineEvent::Error {
                                session: Some(current_session),
                                id: source_id,
                                error: err.into(),
                            });
                            decoder = None;
                            prepared = None;
                            incoming = None;
                            pending_transition = None;
                            converter = None;
                            pending.clear();
                            pending_sent = 0;
                            staging = Staging::default();
                            tail.clear();
                            playing = false;
                            shared.gate_open.store(false, Ordering::Release);
                            // Output stops here: with the gate closed the
                            // callback emits silence, so samples still
                            // buffered are never played (the next
                            // generation change discards them). Never
                            // report a clean TrackEnded after an error.
                            ended_sent = true;
                            eof_reached = true;
                            break 'pump;
                        }
                    }
                } else {
                    break 'pump;
                }
                if !progressed {
                    break 'pump;
                }
            }
        }
        if eof_reached
            && pending.is_empty()
            && tail.is_empty()
            && (incoming.is_some() || prepared.is_none())
        {
            shared.decoder_eof.store(true, Ordering::Release);
        }

        // Promote the new source only when its first output frame is the
        // next one the device consumes. Until then the reported active
        // source and its source-relative position remain the old one.
        let transition_reached = pending_transition.as_ref().is_some_and(|transition| {
            shared.played_frames.load(Ordering::Acquire) >= transition.boundary_frame
        });
        if transition_reached {
            let transition = pending_transition.take().expect("transition checked");
            if let Some(next) = incoming.take() {
                current = Some(next);
                // Keep the generation's seek base (usually zero) and
                // fold it into the new source start in one atomic store.
                // This avoids a visible position window between two
                // stores: position remains base + played - source_start.
                let base = shared.base_frames.load(Ordering::Acquire);
                shared.source_start_frame.store(
                    transition.boundary_frame.saturating_add(base),
                    Ordering::Release,
                );
                send(EngineEvent::Transitioned {
                    session: current_session,
                    generation: current_generation,
                    from_id: transition.from_id,
                    from_entry: transition.from_entry,
                    to_id: transition.to_id,
                    to_entry: transition.to_entry,
                    boundary_frame: transition.boundary_frame,
                    info: transition.info,
                });
            }
        }

        // End-of-track: decoder EOF + ring fully drained (reported by
        // the callback through Shared::drained).
        if playing
            && !ended_sent
            && incoming.is_none()
            && pending_transition.is_none()
            && tail.is_empty()
            && shared.drained.load(Ordering::Acquire)
        {
            if let Some(track) = &current {
                send(EngineEvent::TrackEnded {
                    session: current_session,
                    entry: track.entry,
                    id: track.id,
                });
            }
            ended_sent = true;
            playing = false;
            shared.gate_open.store(false, Ordering::Release);
            decoder = None;
        }
    }

    backend.stop();
}

/// Converts decoded audio (file rate/channels) to the output format:
/// channel remapping plus linear-interpolation resampling. Stateful
/// across chunks; call [`finish`](Self::finish) at end-of-stream to
/// flush the final partial frame.
///
/// Channel rules (documented, deliberately simple): downmixing to mono
/// averages all channels; otherwise output channel *c* takes input
/// channel `min(c, in-1)` — mono duplicates to every output, surround
/// keeps its first two channels when downmixing to stereo.
struct FormatConverter {
    in_channels: usize,
    out_channels: usize,
    /// Input frames advanced per output frame (`in_rate / out_rate`).
    step: f64,
    /// Remapped-but-not-yet-emitted frames (flat, `out_channels` stride).
    buffer: VecDeque<f32>,
    /// Fractional read position within `buffer`, in frames.
    phase: f64,
}

impl FormatConverter {
    fn new(info: &StreamInfo, out: OutputConfig) -> Self {
        Self {
            in_channels: usize::from(info.channels),
            out_channels: usize::from(out.channels),
            step: f64::from(info.sample_rate) / f64::from(out.sample_rate.max(1)),
            buffer: VecDeque::new(),
            phase: 0.0,
        }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        for frame in input.chunks_exact(self.in_channels) {
            if self.out_channels == 1 && self.in_channels > 1 {
                let mean = frame.iter().sum::<f32>() / self.in_channels as f32;
                self.buffer.push_back(mean);
            } else {
                for c in 0..self.out_channels {
                    self.buffer.push_back(frame[c.min(self.in_channels - 1)]);
                }
            }
        }
        self.emit(false)
    }

    fn finish(&mut self) -> Vec<f32> {
        self.emit(true)
    }

    fn emit(&mut self, flushing: bool) -> Vec<f32> {
        let stride = self.out_channels;
        let available = self.buffer.len() / stride;
        let mut out = Vec::new();
        loop {
            let index = self.phase.floor() as usize;
            if index + 1 < available {
                let frac = (self.phase - index as f64) as f32;
                for c in 0..stride {
                    let a = self.buffer[index * stride + c];
                    let b = self.buffer[(index + 1) * stride + c];
                    out.push(a + (b - a) * frac);
                }
                self.phase += self.step;
            } else if flushing && index < available {
                // Hold the final frame for the remaining fraction.
                for c in 0..stride {
                    out.push(self.buffer[index * stride + c]);
                }
                self.phase += self.step;
                if self.phase.floor() as usize >= available {
                    break;
                }
            } else {
                break;
            }
        }
        let consumed = (self.phase.floor() as usize).min(available);
        self.buffer.drain(..consumed * stride);
        self.phase -= consumed as f64;
        out
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    use flux_core::{QueueEntryId, TrackId};

    use super::{
        EngineError, EngineEvent, EngineHandle, FormatConverter, SampleSource, Shared,
        WAVEFORM_WINDOW, WaveformReader, mix_equal_power_tail,
    };
    use crate::decode::{DecodeError, StreamInfo, open_decoder};
    use crate::output::{FillCallback, NullControl, OutputConfig};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(test: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "flux-engine-test-{}-{test}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A mono 16-bit WAV holding `frames` frames of constant `value`
    /// (in f32 terms, -1.0..1.0) — DC makes sample accounting exact.
    fn write_dc_wav(dir: &TempDir, name: &str, rate: u32, frames: usize, value: f32) -> PathBuf {
        let path = dir.0.join(name);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
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

    /// A 16-bit WAV with `channels` interleaved constant samples per
    /// frame, used for channel/rate boundary tests.
    fn write_dc_wav_channels(
        dir: &TempDir,
        name: &str,
        rate: u32,
        channels: u16,
        frames: usize,
        value: f32,
    ) -> PathBuf {
        let path = dir.0.join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create wav");
        let sample = (value * 32_767.0) as i32;
        for _ in 0..frames {
            for _ in 0..channels {
                writer.write_sample(sample).expect("write sample");
            }
        }
        writer.finalize().expect("finalize");
        path
    }

    /// A mono 16-bit WAV whose first `first_frames` samples are `first`
    /// and whose remainder are `second`. Used to prove that seeking
    /// really selects a later region of the source.
    fn write_two_level_wav(
        dir: &TempDir,
        name: &str,
        rate: u32,
        first_frames: usize,
        total_frames: usize,
        first: f32,
        second: f32,
    ) -> PathBuf {
        let path = dir.0.join(name);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create wav");
        let a = (first * 32_767.0) as i32;
        let b = (second * 32_767.0) as i32;
        for index in 0..total_frames {
            writer
                .write_sample(if index < first_frames { a } else { b })
                .expect("write sample");
        }
        writer.finalize().expect("finalize");
        path
    }

    /// Drains the null device and collects events until `done` returns
    /// true for one of them (or the iteration cap is hit). Convergence
    /// loop, not a timing assertion: the cap is generous and every
    /// iteration makes deterministic progress (512 device frames).
    fn drive_until(
        engine: &EngineHandle,
        control: &NullControl,
        done: impl Fn(&EngineEvent) -> bool,
    ) -> Vec<EngineEvent> {
        let mut seen = Vec::new();
        for _ in 0..4_000 {
            control.drain_frames(512);
            while let Ok(event) = engine.events().recv_timeout(Duration::from_millis(5)) {
                let finished = done(&event);
                seen.push(event);
                if finished {
                    return seen;
                }
            }
        }
        seen
    }

    fn nonzero_frames(control: &NullControl, channels: usize) -> usize {
        control
            .captured()
            .chunks_exact(channels)
            .filter(|frame| frame.iter().any(|s| s.abs() > 0.001))
            .count()
    }

    /// Acknowledges generation changes and waits until the worker has
    /// pushed `frames` output frames into the ring. Draining zero frames
    /// runs the callback's generation-flush path without consuming audio.
    fn wait_for_pushed_frames(engine: &EngineHandle, control: &NullControl, frames: u64) {
        for _ in 0..4_000 {
            control.drain_frames(0);
            if engine.shared.pushed_frames.load(Ordering::Acquire) >= frames {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "worker pushed {} of {frames} expected frames",
            engine.shared.pushed_frames.load(Ordering::Acquire)
        );
    }

    fn start_playing(engine: &EngineHandle) {
        let events =
            collect_events_until(engine, |event| matches!(event, EngineEvent::Started { .. }));
        assert!(
            matches!(events.last(), Some(EngineEvent::Started { .. })),
            "expected Started before consuming audio: {events:?}"
        );
    }

    fn expected_equal_power(outgoing: f32, incoming: f32, frame: usize, total: usize) -> f32 {
        let theta = if total <= 1 {
            std::f32::consts::FRAC_PI_2
        } else {
            (frame as f32 / (total - 1) as f32) * std::f32::consts::FRAC_PI_2
        };
        (outgoing * theta.cos() + incoming * theta.sin()).clamp(-1.0, 1.0)
    }

    #[test]
    fn playback_progresses_and_reports_track_ended_after_drain() {
        let dir = TempDir::new("progress");
        let path = write_dc_wav(&dir, "dc.wav", 44_100, 11_025, 0.4);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let id = TrackId::new(7);
        engine.load(id, path);
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Loaded { .. })
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::Loaded { id: got, info, .. }) if *got == id && info.sample_rate == 44_100)
        );

        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::TrackEnded { .. })
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::TrackEnded { id: got, .. }) if *got == id),
            "expected TrackEnded, got {events:?}"
        );
        // All 11_025 DC frames passed through (silence padding aside).
        assert_eq!(nonzero_frames(&control, 1), 11_025);
        assert!((engine.position().as_millis() as i64 - 250).abs() <= 12);
        engine.shutdown();
    }

    #[test]
    fn waveform_tap_returns_latest_post_gain_output() {
        let dir = TempDir::new("waveform-tap");
        let path = write_dc_wav(&dir, "dc.wav", 44_100, 44_100, 0.4);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_volume(0.5);

        assert!(
            engine.waveform().iter().all(|sample| *sample == 0.0),
            "nothing has reached the output yet"
        );

        let id = TrackId::new(3);
        engine.load(id, path);
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Loaded { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Loaded { .. })));
        engine.play();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Started { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Started { .. })));

        let mut waveform = engine.waveform();
        for _ in 0..100 {
            control.drain_frames(512);
            waveform = engine.waveform();
            if waveform
                .iter()
                .rev()
                .take(1_000)
                .all(|sample| (*sample - 0.2).abs() < 0.01)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(waveform.len(), WAVEFORM_WINDOW);
        assert!(
            waveform
                .iter()
                .rev()
                .take(1_000)
                .all(|sample| (*sample - 0.2).abs() < 0.01),
            "the tap must expose post-gain output samples: {:?}",
            &waveform[WAVEFORM_WINDOW - 8..]
        );

        engine.pause();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Paused { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Paused { .. })));
        control.drain_frames(WAVEFORM_WINDOW * 2);
        assert!(
            engine.waveform().iter().all(|sample| *sample == 0.0),
            "paused output must decay to the flat center line"
        );

        engine.stop();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Stopped)
        });
        assert!(matches!(events.last(), Some(EngineEvent::Stopped)));
        control.drain_frames(WAVEFORM_WINDOW * 2);
        assert!(
            engine.waveform().iter().all(|sample| *sample == 0.0),
            "stopped output must be silent"
        );
        engine.shutdown();
    }

    #[test]
    fn seek_while_stopped_then_play_starts_at_the_target() {
        let dir = TempDir::new("seek-stopped");
        let path = write_two_level_wav(&dir, "two-level.wav", 44_100, 44_100, 88_200, 0.2, -0.3);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let id = TrackId::new(8);
        engine.load(id, path);
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Loaded { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Loaded { .. })));

        let _ = engine.seek(Duration::from_millis(1_500));
        engine.play();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Started { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Started { .. })));
        control.clear_captured();
        let mut later_samples = 0;
        for _ in 0..4_000 {
            control.drain_frames(512);
            later_samples = control
                .captured()
                .iter()
                .filter(|sample| (**sample + 0.3).abs() < 0.01)
                .count();
            if later_samples > 1_000 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            later_samples > 1_000,
            "playing after a stopped seek must use the later source region"
        );
        engine.shutdown();
    }

    #[test]
    fn seek_repositions_the_source_and_preserves_paused_state() {
        let dir = TempDir::new("seek");
        let path = write_two_level_wav(&dir, "two-level.wav", 44_100, 44_100, 88_200, 0.2, -0.3);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let id = TrackId::new(7);
        engine.load(id, path);
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Loaded { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Loaded { .. })));
        engine.play();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Started { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Started { .. })));
        control.drain_frames(2048);

        let target = Duration::from_millis(1_500);
        let _ = engine.seek(target);
        assert!(
            (engine.position().as_millis() as i64 - 1_500).abs() <= 2,
            "seek must update the reported position immediately"
        );
        control.clear_captured();
        let mut later_samples = 0;
        for _ in 0..4_000 {
            control.drain_frames(512);
            later_samples = control
                .captured()
                .iter()
                .filter(|sample| (**sample + 0.3).abs() < 0.01)
                .count();
            if later_samples > 1_000 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            later_samples > 1_000,
            "post-seek output must come from the later source region"
        );

        engine.pause();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Paused { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Paused { .. })));
        let _ = engine.seek(Duration::from_millis(250));
        assert!(
            (engine.position().as_millis() as i64 - 250).abs() <= 2,
            "a paused seek still updates position"
        );
        control.clear_captured();
        control.drain_frames(4_096);
        assert!(
            control.captured().iter().all(|sample| *sample == 0.0),
            "a paused seek remains silent"
        );

        engine.play();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Started { .. })
        });
        assert!(matches!(events.last(), Some(EngineEvent::Started { .. })));
        control.clear_captured();
        for _ in 0..50 {
            control.drain_frames(512);
            std::thread::sleep(Duration::from_millis(1));
        }
        let captured = control.captured();
        assert!(
            captured
                .iter()
                .filter(|sample| (**sample - 0.2).abs() < 0.01)
                .count()
                > 1_000,
            "resuming after a paused seek must start in the earlier source region"
        );

        let _ = engine.seek(Duration::from_secs(2));
        assert!(
            (engine.position().as_millis() as i64 - 2_000).abs() <= 2,
            "seek must expose the requested endpoint"
        );
        engine.shutdown();
    }

    #[test]
    fn pause_halts_consumption_and_resume_continues() {
        let dir = TempDir::new("pause");
        let path = write_dc_wav(&dir, "dc.wav", 44_100, 44_100, 0.3);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let id = TrackId::new(1);
        engine.load(id, path);
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Loaded { .. })
        });
        engine.play();
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Started { .. })
        });
        // Let some audio through.
        for _ in 0..10 {
            control.drain_frames(512);
            std::thread::sleep(Duration::from_millis(5));
        }
        let before_pause = nonzero_frames(&control, 1);
        assert!(before_pause > 0);

        engine.pause();
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Paused { .. })
        });
        let at_pause = nonzero_frames(&control, 1);
        for _ in 0..10 {
            control.drain_frames(512);
        }
        assert_eq!(
            nonzero_frames(&control, 1),
            at_pause,
            "paused device must consume silence only"
        );

        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::TrackEnded { .. })
        });
        assert!(matches!(
            events.last(),
            Some(EngineEvent::TrackEnded { .. })
        ));
        assert_eq!(nonzero_frames(&control, 1), 44_100);
        engine.shutdown();
    }

    #[test]
    fn stop_flushes_buffer_and_resets_position() {
        let dir = TempDir::new("stop");
        let path = write_dc_wav(&dir, "dc.wav", 44_100, 44_100, 0.3);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.load(TrackId::new(1), path);
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Loaded { .. })
        });
        engine.play();
        for _ in 0..6 {
            control.drain_frames(512);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(engine.position() > Duration::ZERO);

        engine.stop();
        let events = drive_until(&engine, &control, |e| matches!(e, EngineEvent::Stopped));
        assert!(matches!(events.last(), Some(EngineEvent::Stopped)));
        control.drain_frames(512);
        assert_eq!(engine.position(), Duration::ZERO);
        let nonzero = nonzero_frames(&control, 1);
        control.drain_frames(4096);
        assert_eq!(
            nonzero_frames(&control, 1),
            nonzero,
            "only silence after stop"
        );
        engine.shutdown();
    }

    #[test]
    fn replacing_the_track_never_leaks_stale_samples_or_events() {
        let dir = TempDir::new("replace");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 44_100, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 11_025, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));

        engine.load(id_a, a);
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Loaded { .. })
        });
        engine.play();
        for _ in 0..6 {
            control.drain_frames(512);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            nonzero_frames(&control, 1) > 0,
            "track A is audible in the capture"
        );

        engine.load(id_b, b);
        // Wait until the load has been processed (generation bumped),
        // then start observing: from here on, only B may appear.
        drive_until(
            &engine,
            &control,
            |e| matches!(e, EngineEvent::Loaded { id, .. } if *id == id_b),
        );
        control.clear_captured();
        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::TrackEnded { .. })
        });
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, EngineEvent::TrackEnded { id, .. } if *id == id_a)),
            "track A must never report TrackEnded: {events:?}"
        );
        let captured = control.captured();
        assert!(
            captured
                .iter()
                .all(|s| s.abs() < 0.001 || (*s - 0.8).abs() < 0.02),
            "only silence or track B's DC level after replacement"
        );
        assert_eq!(nonzero_frames(&control, 1), 11_025);
        engine.shutdown();
    }

    /// Collects engine events **without draining the null device**
    /// until `done` matches one. Unlike `drive_until`, the audio
    /// callback never runs here — the point of the two generation-race
    /// regression tests below, which let the worker run ahead of the
    /// callback and know exactly how far it got from command/event
    /// ordering alone (the worker runs its pump section before
    /// receiving the next command, so a later command's event happens
    /// after every earlier iteration's pump). The iteration cap is a
    /// hang guard, not a timing assertion.
    fn collect_events_until(
        engine: &EngineHandle,
        done: impl Fn(&EngineEvent) -> bool,
    ) -> Vec<EngineEvent> {
        let mut seen = Vec::new();
        for _ in 0..2_000 {
            if let Ok(event) = engine.events().recv_timeout(Duration::from_millis(5)) {
                let finished = done(&event);
                seen.push(event);
                if finished {
                    return seen;
                }
            }
        }
        seen
    }

    /// Regression test for the generation-flush race (M6-1): the
    /// callback discards the ring contents the first time it observes
    /// a generation change, so if the worker gets to push
    /// new-generation samples before that first post-load callback,
    /// those samples must not be thrown away.
    ///
    /// This is forced deterministically, without sleeps: `load`,
    /// `play` and `pause` are sent back-to-back and the device is
    /// never drained during setup. Receiving `Paused` proves the
    /// worker completed the `Play` iteration — including its pump
    /// section — while the callback has never run. Before the fix,
    /// that pump pushed the first decoded chunk of the new track into
    /// the ring and the first drain after resuming wiped it with the
    /// generation flush (the 44_100-frame total below fell short);
    /// with the fix the worker holds its pushes until the callback
    /// acknowledges the generation, so every frame arrives.
    #[test]
    fn generation_flush_never_discards_new_track_samples() {
        let dir = TempDir::new("generation-race");
        let path = write_dc_wav(&dir, "dc.wav", 44_100, 44_100, 0.4);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let id = TrackId::new(1);

        engine.load(id, path);
        engine.play();
        engine.pause();
        let events = collect_events_until(&engine, |e| matches!(e, EngineEvent::Paused { .. }));
        assert!(
            matches!(events.last(), Some(EngineEvent::Paused { id: got, .. }) if *got == id),
            "expected Paused, got {events:?}"
        );
        assert_eq!(
            control.captured_frames(),
            0,
            "the callback must not have run during setup"
        );

        // Resume and drain to completion: the full track, including
        // its first chunk, must reach the device.
        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::TrackEnded { .. })
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::TrackEnded { id: got, .. }) if *got == id),
            "expected TrackEnded, got {events:?}"
        );
        assert_eq!(nonzero_frames(&control, 1), 44_100);
        engine.shutdown();
    }

    /// M6-1 companion case: rapid consecutive loads with no callback
    /// activity in between. Only the newest track may be heard, and it
    /// must arrive complete. Same ordering argument as above: waiting
    /// for `Paused` proves the worker finished the `Play` iteration's
    /// pump for track B before the callback's first run.
    #[test]
    fn rapid_loads_without_callback_still_deliver_newest_track_fully() {
        let dir = TempDir::new("rapid-loads");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 44_100, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 11_025, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));

        engine.load(id_a, a);
        engine.load(id_b, b);
        engine.play();
        engine.pause();
        let events = collect_events_until(&engine, |e| matches!(e, EngineEvent::Paused { .. }));
        assert!(
            matches!(events.last(), Some(EngineEvent::Paused { id: got, .. }) if *got == id_b),
            "expected Paused for track B, got {events:?}"
        );
        assert_eq!(
            control.captured_frames(),
            0,
            "the callback must not have run during setup"
        );

        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::TrackEnded { .. })
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::TrackEnded { id: got, .. }) if *got == id_b),
            "expected TrackEnded for track B, got {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, EngineEvent::TrackEnded { id, .. } if *id == id_a)),
            "track A must never report TrackEnded: {events:?}"
        );
        let captured = control.captured();
        assert!(
            captured
                .iter()
                .all(|s| s.abs() < 0.001 || (*s - 0.8).abs() < 0.02),
            "only silence or track B's DC level may appear"
        );
        assert_eq!(nonzero_frames(&control, 1), 11_025);
        engine.shutdown();
    }

    #[test]
    fn every_load_mints_a_distinct_monotonic_session() {
        let dir = TempDir::new("session-ids");
        let path = write_dc_wav(&dir, "dc.wav", 44_100, 44_100, 0.4);
        let (engine, _control) = EngineHandle::spawn_null(44_100, 1);
        let id = TrackId::new(1);

        let first = engine.load(id, path.clone());
        let second = engine.load(id, path);
        assert_ne!(first, 0, "0 is reserved for no session");
        assert!(second > first, "sessions must increase: {first}, {second}");
        engine.shutdown();
    }

    #[test]
    fn failed_load_does_not_emit_redundant_no_track_error() {
        let dir = TempDir::new("failed-load-play");
        let bad = dir.0.join("bad.wav");
        fs::write(&bad, b"not a real wav").expect("bad fixture");
        let good = write_dc_wav(&dir, "good.wav", 44_100, 44_100, 0.4);
        let (engine, _control) = EngineHandle::spawn_null(44_100, 1);

        let bad_id = TrackId::new(1);
        let good_id = TrackId::new(2);
        let bad_session = engine.load(bad_id, bad);
        engine.play(); // the controller does this immediately after load
        engine.load(good_id, good);
        let events = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == good_id),
        );

        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Error {
                    session: Some(session),
                    id: Some(id),
                    ..
                } if *session == bad_session && *id == bad_id
            )),
            "the meaningful load error must remain: {events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                EngineEvent::Error {
                    error: EngineError::NoTrackLoaded,
                    ..
                }
            )),
            "a failed load must not add a second NoTrackLoaded error: {events:?}"
        );
        assert!(
            matches!(events.last(), Some(EngineEvent::Loaded { id, .. }) if *id == good_id),
            "the follow-up load must succeed: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn matching_load_consumes_the_preloaded_decoder() {
        let dir = TempDir::new("preload-consume");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 1_024, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1_024, 0.8);
        let (engine, _control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));

        engine.load(id_a, a);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == id_a),
        );
        engine.preload(id_b, b.clone());
        let events = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        assert!(
            matches!(events.last(), Some(EngineEvent::Preloaded { info, .. }) if info.sample_rate == 44_100),
            "expected Preloaded: {events:?}"
        );

        // A matching load must take ownership of the already-open decoder
        // instead of opening the path again.
        fs::remove_file(&b).expect("remove preloaded source");
        engine.load(id_b, b);
        let events = collect_events_until(&engine, |event| {
            matches!(
                event,
                EngineEvent::Loaded { id, .. } | EngineEvent::Error { id: Some(id), .. }
                    if *id == id_b
            )
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::Loaded { id, .. }) if *id == id_b),
            "matching load should reuse the prepared decoder: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn cancelled_preload_is_released_and_not_reused() {
        let dir = TempDir::new("preload-cancel");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 1_024, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1_024, 0.8);
        let (engine, _control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));

        engine.load(id_a, a);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == id_a),
        );
        engine.preload(id_b, b.clone());
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        engine.cancel_preload();
        fs::remove_file(&b).expect("remove cancelled source");

        engine.load(id_b, b);
        let events = collect_events_until(&engine, |event| {
            matches!(
                event,
                EngineEvent::Loaded { id, .. } | EngineEvent::Error { id: Some(id), .. }
                    if *id == id_b
            )
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::Error { id: Some(id), .. }) if *id == id_b),
            "a cancelled preload must not be reused: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn mismatched_load_discards_a_preloaded_decoder() {
        let dir = TempDir::new("preload-mismatch");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 1_024, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1_024, 0.8);
        let (engine, _control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));

        engine.load(id_a, a.clone());
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == id_a),
        );
        engine.preload(id_b, b.clone());
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        engine.load(id_a, a);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == id_a),
        );
        fs::remove_file(&b).expect("remove stale source");

        engine.load(id_b, b);
        let events = collect_events_until(&engine, |event| {
            matches!(
                event,
                EngineEvent::Loaded { id, .. } | EngineEvent::Error { id: Some(id), .. }
                    if *id == id_b
            )
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::Error { id: Some(id), .. }) if *id == id_b),
            "a stale preload must be dropped by a mismatched load: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn preload_failure_never_interrupts_current_playback() {
        let dir = TempDir::new("preload-failure");
        let good = write_dc_wav(&dir, "good.wav", 44_100, 8_192, 0.4);
        let bad = dir.0.join("bad.wav");
        fs::write(&bad, b"not a real wav").expect("bad fixture");
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (good_id, bad_id) = (TrackId::new(1), TrackId::new(2));

        engine.load(good_id, good);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == good_id),
        );
        engine.preload(bad_id, bad);
        let events = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::PreloadFailed { id, .. } if *id == bad_id),
        );
        assert!(
            matches!(events.last(), Some(EngineEvent::PreloadFailed { id, .. }) if *id == bad_id),
            "expected PreloadFailed: {events:?}"
        );

        engine.play();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::TrackEnded { .. })
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::TrackEnded { id, .. }) if *id == good_id),
            "the active source must survive a failed preload: {events:?}"
        );
        assert_eq!(nonzero_frames(&control, 1), 8_192);
        engine.shutdown();
    }

    #[test]
    fn natural_handoff_is_frame_exact_and_reports_the_boundary_once() {
        let dir = TempDir::new("gapless-exact");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 1_000, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(10), QueueEntryId::new(11));

        let session = engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == id_a),
        );
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 2_200);
        let generation = engine.shared.generation.load(Ordering::Acquire);
        let start_count = control.start_count();
        control.clear_captured();

        // Consume exactly one frame of the successor. The transition
        // event must name boundary 1000, and the captured output must be
        // 1000 old frames followed immediately by frame 0 of the new
        // source.
        control.drain_frames(1_001);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        let captured = control.captured();
        assert_eq!(captured.len(), 1_001, "exactly one output frame per drain");
        assert!(
            captured[..1_000].iter().all(|s| (*s - 0.2).abs() < 0.01),
            "first source frames changed"
        );
        assert!(
            (*captured.get(1_000).expect("successor frame") - 0.8).abs() < 0.01,
            "successor must begin at the immediately following frame"
        );
        let transition = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::Transitioned {
                    session: got_session,
                    generation: got_generation,
                    from_id,
                    from_entry,
                    to_id,
                    to_entry,
                    boundary_frame,
                    info,
                } => Some((
                    *got_session,
                    *got_generation,
                    *from_id,
                    *from_entry,
                    *to_id,
                    *to_entry,
                    *boundary_frame,
                    info.sample_rate,
                )),
                _ => None,
            })
            .expect("transition event");
        assert_eq!(
            transition,
            (
                session, generation, id_a, entry_a, id_b, entry_b, 1_000, 44_100,
            )
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                EngineEvent::TrackEnded { id, .. } if *id == id_a
            )),
            "a gapless handoff must not also emit TrackEnded for the old source"
        );
        assert_eq!(
            engine.position(),
            Duration::from_secs_f64(1.0 / 44_100.0),
            "source-relative position must start at the new source"
        );
        assert_eq!(
            engine.shared.generation.load(Ordering::Acquire),
            generation,
            "a natural handoff must not bump the output generation"
        );
        assert_eq!(
            control.start_count(),
            start_count,
            "the output stream must not be recreated"
        );

        // Further consumption must not emit the same transition again.
        control.drain_frames(500);
        while let Ok(event) = engine.events().try_recv() {
            assert!(
                !matches!(event, EngineEvent::Transitioned { .. }),
                "natural transition reported more than once: {event:?}"
            );
        }
        engine.shutdown();
    }

    #[test]
    fn duplicate_track_ids_transition_by_queue_entry_identity() {
        let dir = TempDir::new("gapless-duplicate");
        let path = write_dc_wav(&dir, "same.wav", 44_100, 1_000, 0.4);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let id = TrackId::new(7);
        let (entry_a, entry_b) = (QueueEntryId::new(20), QueueEntryId::new(21));

        engine.load_entry(id, entry_a, path.clone());
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id, entry_b, path);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 2_000);
        control.clear_captured();
        control.drain_frames(1_001);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Transitioned {
                    from_id,
                    from_entry,
                    to_id,
                    to_entry,
                    boundary_frame: 1_000,
                    ..
                } if *from_id == id
                    && *to_id == id
                    && *from_entry == entry_a
                    && *to_entry == entry_b
            )),
            "duplicate TrackIds must keep distinct queue entries: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn replacing_a_preload_routes_the_handoff_to_the_latest_entry() {
        let dir = TempDir::new("gapless-replace");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 800, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 900, 0.5);
        let c = write_dc_wav(&dir, "c.wav", 44_100, 1_100, 0.9);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b, id_c) = (TrackId::new(1), TrackId::new(2), TrackId::new(3));
        let (entry_a, entry_b, entry_c) = (
            QueueEntryId::new(30),
            QueueEntryId::new(31),
            QueueEntryId::new(32),
        );

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        engine.preload_entry(id_c, entry_c, c);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_c),
        );
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 1_900);
        control.clear_captured();
        control.drain_frames(801);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Transitioned {
                    to_id,
                    to_entry,
                    boundary_frame: 800,
                    ..
                } if *to_id == id_c && *to_entry == entry_c
            )),
            "the latest preload must win: {events:?}"
        );
        let captured = control.captured();
        assert!((*captured.last().expect("successor frame") - 0.9).abs() < 0.01);
        engine.shutdown();
    }

    #[test]
    fn reauthorizing_after_install_discards_the_obsolete_successor() {
        let dir = TempDir::new("gapless-reauthorize");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 800, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 900, 0.5);
        let c = write_dc_wav(&dir, "c.wav", 44_100, 1_100, 0.9);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b, id_c) = (TrackId::new(1), TrackId::new(2), TrackId::new(3));
        let (entry_a, entry_b, entry_c) = (
            QueueEntryId::new(100),
            QueueEntryId::new(101),
            QueueEntryId::new(102),
        );

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        engine.play();
        start_playing(&engine);
        // A and B are fully queued and the B handoff is installed, but
        // no boundary frame has been consumed. Re-authorizing C must
        // flush and rewind the current source rather than promote B.
        wait_for_pushed_frames(&engine, &control, 1_700);
        control.drain_frames(300);
        engine.preload_entry(id_c, entry_c, c);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_c),
        );
        // The invalidation reopens A at frame 300, leaving 500 A frames
        // before C's 1100 output frames.
        wait_for_pushed_frames(&engine, &control, 1_600);
        control.clear_captured();
        control.drain_frames(501);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Transitioned {
                    to_id,
                    to_entry,
                    boundary_frame: 500,
                    ..
                } if *to_id == id_c && *to_entry == entry_c
            )),
            "C must replace the invalidated B handoff: {events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                EngineEvent::Transitioned { to_id, .. } if *to_id == id_b
            )),
            "obsolete B must never become active: {events:?}"
        );
        let captured = control.captured();
        assert!(captured[..500].iter().all(|s| (*s - 0.2).abs() < 0.01));
        assert!((*captured.last().expect("C frame") - 0.9).abs() < 0.01);
        engine.shutdown();
    }

    #[test]
    fn cancelling_after_install_discards_the_successor_and_ends_current() {
        let dir = TempDir::new("gapless-cancel-installed");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 800, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 900, 0.5);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(110), QueueEntryId::new(111));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 1_700);
        engine.cancel_preload();
        // Pause follows cancellation on the command channel, so its
        // event proves the invalidation has been processed.
        engine.pause();
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Paused { .. }));
        // Cancellation flushes and rewinds A; only its 800 frames may
        // remain in the rebuilt generation.
        wait_for_pushed_frames(&engine, &control, 800);
        engine.play();
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Started { .. })
        });
        control.clear_captured();
        control.drain_frames(800);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::TrackEnded { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::TrackEnded { entry, id, .. }
                    if *entry == entry_a && *id == id_a
            )),
            "A must end normally after cancellation: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. }))
        );
        control.drain_frames(128);
        assert!(
            control.captured()[800..].iter().all(|s| *s == 0.0),
            "the invalidated successor must not be audible"
        );
        engine.shutdown();
    }

    #[test]
    fn cancelled_preload_falls_back_to_track_ended_with_silence() {
        let dir = TempDir::new("gapless-cancel");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 1_000, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1_000, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(40), QueueEntryId::new(41));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.cancel_preload();
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 1_000);
        control.clear_captured();
        control.drain_frames(1_000);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::TrackEnded { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::TrackEnded { entry, id, .. }
                    if *entry == entry_a && *id == id_a
            )),
            "without a prepared source the track must end normally: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. })),
            "a cancelled preload must not fabricate a transition"
        );
        control.drain_frames(256);
        let captured = control.captured();
        assert!(
            captured[1_000..].iter().all(|s| *s == 0.0),
            "fallback silence must not be presented as a gapless successor"
        );
        engine.shutdown();
    }

    #[test]
    fn pause_before_the_boundary_delays_promotion_until_resume() {
        let dir = TempDir::new("gapless-pause");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 1_000, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1_000, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(50), QueueEntryId::new(51));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 2_000);
        control.drain_frames(900);
        engine.pause();
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Paused { .. }));
        control.drain_frames(200);
        while let Ok(event) = engine.events().try_recv() {
            assert!(
                !matches!(event, EngineEvent::Transitioned { .. }),
                "paused playback advanced across the boundary: {event:?}"
            );
        }
        assert!(engine.position() < Duration::from_secs(1));

        engine.play();
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Started { .. })
        });
        control.drain_frames(101);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Transitioned {
                    to_id,
                    to_entry,
                    boundary_frame: 1_000,
                    ..
                } if *to_id == id_b && *to_entry == entry_b
            )),
            "resume must complete the prepared handoff: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn handoff_normalizes_rate_and_channel_boundaries() {
        let dir = TempDir::new("gapless-format");
        let a = write_dc_wav(&dir, "a.wav", 22_050, 2_000, 0.2);
        let b = write_dc_wav_channels(&dir, "b.wav", 44_100, 2, 2_000, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 2);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(60), QueueEntryId::new(61));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        // 2000 source frames at 22.05 kHz become 4000 output frames;
        // the stereo successor contributes 2000 mono output frames.
        wait_for_pushed_frames(&engine, &control, 6_000);
        control.clear_captured();
        control.drain_frames(4_001);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        let captured = control.captured();
        assert_eq!(captured.len(), 4_001 * 2);
        assert!(
            captured[..4_000 * 2]
                .iter()
                .all(|s| (*s - 0.2).abs() < 0.01),
            "rate-converted mono first source must be duplicated to stereo"
        );
        assert!(
            captured[4_000 * 2..]
                .iter()
                .all(|s| (*s - 0.8).abs() < 0.01),
            "stereo successor must begin at the immediately following frame"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Transitioned {
                    to_id,
                    boundary_frame: 4_000,
                    ..
                } if *to_id == id_b
            )),
            "transition boundary must use output frames: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn stop_during_preload_releases_the_successor_without_a_transition() {
        let dir = TempDir::new("gapless-stop-preload");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 1_000, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1_000, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(70), QueueEntryId::new(71));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.stop();
        let events = collect_events_until(&engine, |event| matches!(event, EngineEvent::Stopped));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Stopped))
        );

        engine.play();
        let events =
            collect_events_until(&engine, |event| matches!(event, EngineEvent::Error { .. }));
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Error {
                    error: EngineError::NoTrackLoaded,
                    ..
                }
            )),
            "stop must unload and release the prepared decoder: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. }))
        );
        let _ = control;
        engine.shutdown();
    }

    #[test]
    fn explicit_load_before_the_boundary_discards_the_old_handoff() {
        let dir = TempDir::new("gapless-skip-preload");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 2_000, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1_000, 0.5);
        let c = write_dc_wav(&dir, "c.wav", 44_100, 1_500, 0.9);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b, id_c) = (TrackId::new(1), TrackId::new(2), TrackId::new(3));
        let (entry_a, entry_b, entry_c) = (
            QueueEntryId::new(80),
            QueueEntryId::new(81),
            QueueEntryId::new(82),
        );

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        // A and B are fully queued and the handoff is installed, but no
        // device frame has crossed the boundary yet.
        wait_for_pushed_frames(&engine, &control, 3_000);

        engine.load_entry(id_c, entry_c, c);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == id_c),
        );
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 1_500);
        control.clear_captured();
        control.drain_frames(1_500);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::TrackEnded { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::TrackEnded { entry, id, .. }
                    if *entry == entry_c && *id == id_c
            )),
            "C must play to its own end: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. })),
            "the obsolete A-to-B handoff must be discarded: {events:?}"
        );
        assert!(
            control.captured().iter().all(|s| (*s - 0.9).abs() < 0.01),
            "only C samples may reach the device"
        );
        engine.shutdown();
    }

    #[test]
    fn seek_after_installing_a_handoff_rebuilds_the_boundary_from_the_seek() {
        let dir = TempDir::new("gapless-seek");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 4_000, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 2_000, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(90), QueueEntryId::new(91));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b.clone());
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 6_000);

        let _ = engine.seek(Duration::from_millis(50));
        engine.preload_entry(id_b, entry_b, b.clone());
        // Preloaded proves the seek command was processed first.
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        // 2205 source frames are skipped; the remaining 1795 A frames
        // precede B's 2000 output frames.
        wait_for_pushed_frames(&engine, &control, 3_795);
        control.clear_captured();
        control.drain_frames(3_795);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Transitioned {
                    boundary_frame: 1_795,
                    to_entry,
                    ..
                } if *to_entry == entry_b
            )),
            "the old boundary must not survive a seek: {events:?}"
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                boundary_frame: 4_000,
                ..
            }
        )));
        let captured = control.captured();
        assert_eq!(captured.len(), 3_795);
        assert!(captured[..1_795].iter().all(|s| (*s - 0.2).abs() < 0.01));
        assert!(captured[1_795..].iter().all(|s| (*s - 0.8).abs() < 0.01));
        engine.shutdown();
    }

    #[test]
    fn one_frame_successor_is_contiguous_and_then_ends_once() {
        let dir = TempDir::new("gapless-one-frame");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 500, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 1, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(120), QueueEntryId::new(121));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 501);
        control.clear_captured();
        control.drain_frames(501);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                boundary_frame: 500,
                ..
            } if *to_id == id_b
        )));
        let captured = control.captured();
        assert_eq!(captured.len(), 501);
        assert!((*captured.last().expect("one successor frame") - 0.8).abs() < 0.01);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::TrackEnded { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::TrackEnded { entry, id, .. }
                    if *entry == entry_b && *id == id_b
            )),
            "the one-frame successor must end exactly once: {events:?}"
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            EngineEvent::TrackEnded { id, .. } if *id == id_a
        )));
        engine.shutdown();
    }

    #[test]
    fn truncated_prepared_successor_fails_without_a_fake_transition() {
        let dir = TempDir::new("gapless-truncated");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 1_000, 0.2);
        let full = write_dc_wav(&dir, "b-full.wav", 44_100, 2_000, 0.8);
        let truncated = dir.0.join("b.wav");
        let mut bytes = fs::read(&full).expect("read full wav");
        bytes.truncate(bytes.len() - 100);
        fs::write(&truncated, bytes).expect("write truncated wav");
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(130), QueueEntryId::new(131));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, truncated);
        let preloaded = collect_events_until(&engine, |event| {
            matches!(
                event,
                EngineEvent::Preloaded { .. } | EngineEvent::PreloadFailed { .. }
            )
        });
        assert!(
            matches!(preloaded.last(), Some(EngineEvent::Preloaded { id, .. }) if *id == id_b),
            "the truncated file must open so the failure happens mid-stream: {preloaded:?}"
        );
        engine.play();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Error { .. })
        });
        assert!(
            events.iter().any(|event| matches!(
                event,
                EngineEvent::Error { id: Some(id), .. } if *id == id_b
            )),
            "the successor's decode failure must name B: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. })),
            "a failed successor must not produce a fake transition: {events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                EngineEvent::TrackEnded { id, .. } if *id == id_a
            )),
            "a failed successor must not report a clean A end"
        );
        engine.shutdown();
    }

    #[test]
    fn crossfade_mixes_equal_power_envelope_and_transitions_at_start() {
        let dir = TempDir::new("crossfade-envelope");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        assert_eq!(engine.set_crossfade_ms(500), 500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(200), QueueEntryId::new(201));

        let session = engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        // 2 s sources at 44.1 kHz; 500 ms of tail, so the overlap starts
        // at output frame 66_150. Wait until the mixed tail is queued.
        wait_for_pushed_frames(&engine, &control, 88_200);
        let generation = engine.shared.generation.load(Ordering::Acquire);
        let start_count = control.start_count();

        // Consume the un-crossfaded prefix, then capture only the overlap.
        control.drain_frames(66_150);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        control.clear_captured();
        control.drain_frames(22_050);
        let mixed = control.captured();
        assert_eq!(mixed.len(), 22_050);
        assert!((mixed[0] - 0.2).abs() < 0.01);
        assert!(
            (mixed[11_025] - (0.2 + 0.8) * std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01,
            "midpoint must use equal-power gains: {}",
            mixed[11_025]
        );
        assert!((mixed[22_049] - 0.8).abs() < 0.01);
        assert!(mixed.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        let transition = events
            .iter()
            .find_map(|event| match event {
                EngineEvent::Transitioned {
                    session: got_session,
                    generation: got_generation,
                    from_id,
                    from_entry,
                    to_id,
                    to_entry,
                    boundary_frame,
                    ..
                } => Some((
                    *got_session,
                    *got_generation,
                    *from_id,
                    *from_entry,
                    *to_id,
                    *to_entry,
                    *boundary_frame,
                )),
                _ => None,
            })
            .expect("transition event");
        assert_eq!(
            transition,
            (session, generation, id_a, entry_a, id_b, entry_b, 66_150)
        );
        assert_eq!(
            engine.shared.generation.load(Ordering::Acquire),
            generation,
            "crossfade must stay in one output generation"
        );
        assert_eq!(
            control.start_count(),
            start_count,
            "crossfade must not recreate the output stream"
        );
        assert_eq!(
            engine.position(),
            Duration::from_secs_f64(22_050.0 / 44_100.0),
            "the incoming source position starts at the crossfade boundary"
        );
        control.drain_frames(500);
        while let Ok(event) = engine.events().try_recv() {
            assert!(
                !matches!(event, EngineEvent::Transitioned { .. }),
                "crossfade transition reported more than once: {event:?}"
            );
        }
        engine.shutdown();
    }

    #[test]
    fn equal_power_envelope_is_defined_for_one_frame_and_endpoints() {
        use std::collections::VecDeque;

        let mut one = VecDeque::from([0.2f32]);
        let (mixed, frames) = mix_equal_power_tail(&mut one, &[0.8], 1);
        assert_eq!(frames, 1);
        assert_eq!(mixed, vec![0.8], "one-frame overlap selects incoming");

        let mut tail = VecDeque::from([0.2f32, 0.2]);
        let incoming = [0.8f32, 0.8];
        let (mixed, frames) = mix_equal_power_tail(&mut tail, &incoming, 1);
        assert_eq!(frames, 2);
        assert!((mixed[0] - 0.2).abs() < 1e-6);
        assert!((mixed[1] - 0.8).abs() < 1e-6);

        for frame in 0..1_000 {
            let sample = expected_equal_power(0.4, 0.9, frame, 1_000);
            assert!(sample.is_finite());
            assert!(sample.abs() <= 1.0);
        }
    }

    #[test]
    fn zero_crossfade_preserves_the_gapless_boundary() {
        let dir = TempDir::new("crossfade-zero");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        assert_eq!(engine.set_crossfade_ms(0), 0);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(210), QueueEntryId::new(211));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        // The ring is exactly 2 s at this fixture size. Drain one
        // decode chunk so the worker has room to reach EOF with no tail.
        wait_for_pushed_frames(&engine, &control, 4_096);
        control.drain_frames(4_096);
        control.clear_captured();
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(88_200 - 4_096);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                to_entry,
                boundary_frame: 88_200,
                ..
            } if *to_id == id_b && *to_entry == entry_b
        )));
        let captured = control.captured();
        assert!(captured.iter().all(|s| (*s - 0.2).abs() < 0.01));
        engine.shutdown();
    }

    #[test]
    fn very_short_crossfade_uses_the_exact_frame_window() {
        let dir = TempDir::new("crossfade-1ms");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        assert_eq!(engine.set_crossfade_ms(1), 1);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(220), QueueEntryId::new(221));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 4_096);
        control.drain_frames(4_096);
        control.clear_captured();
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(88_156 - 4_096);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        control.clear_captured();
        control.drain_frames(44);
        let mixed = control.captured();
        assert_eq!(mixed.len(), 44);
        for (frame, sample) in mixed.iter().enumerate() {
            let expected = expected_equal_power(0.2, 0.8, frame, 44);
            assert!((sample - expected).abs() < 0.01, "frame {frame}");
        }
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                boundary_frame: 88_156,
                ..
            } if *to_id == id_b
        )));
        engine.shutdown();
    }

    #[test]
    fn short_outgoing_source_uses_a_bounded_available_overlap() {
        let dir = TempDir::new("crossfade-short-outgoing");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 132_300, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        assert_eq!(engine.set_crossfade_ms(5_000), 5_000);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(230), QueueEntryId::new(231));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        // A 2 s source cannot donate 5 s. With the 1 s prebuffer, the
        // bounded overlap is 1 s: 44_100 output frames.
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(44_100);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        control.clear_captured();
        control.drain_frames(44_100);
        let mixed = control.captured();
        assert_eq!(mixed.len(), 44_100);
        assert!((mixed[0] - 0.2).abs() < 0.01);
        assert!((mixed[44_099] - 0.8).abs() < 0.01);
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                boundary_frame: 44_100,
                ..
            } if *to_id == id_b
        )));
        engine.shutdown();
    }

    #[test]
    fn outgoing_shorter_than_prebuffer_falls_back_to_gapless() {
        let dir = TempDir::new("crossfade-too-short-outgoing");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 22_050, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(240), QueueEntryId::new(241));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 22_050);
        control.clear_captured();
        control.drain_frames(22_050);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                boundary_frame: 22_050,
                ..
            } if *to_id == id_b
        )));
        assert!(events.iter().all(|event| !matches!(
            event,
            EngineEvent::Transitioned { boundary_frame, .. }
                if *boundary_frame < 22_050
        )));
        control.clear_captured();
        control.drain_frames(1);
        assert!((control.captured()[0] - 0.8).abs() < 0.01);
        engine.shutdown();
    }

    #[test]
    fn incoming_shorter_than_overlap_falls_back_to_gapless() {
        let dir = TempDir::new("crossfade-short-incoming");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 8_820, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(250), QueueEntryId::new(251));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(88_200);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                boundary_frame: 88_200,
                ..
            } if *to_id == id_b
        )));
        control.clear_captured();
        control.drain_frames(1);
        assert!((control.captured()[0] - 0.8).abs() < 0.01);
        engine.shutdown();
    }

    #[test]
    fn correlated_peaks_are_clamped_during_equal_power_mix() {
        let dir = TempDir::new("crossfade-clip");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.9);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.9);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(260), QueueEntryId::new(261));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(66_150);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        control.clear_captured();
        control.drain_frames(22_050);
        let mixed = control.captured();
        assert!(mixed.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        assert!(
            (mixed[11_025] - 1.0).abs() < 0.001,
            "equal-power correlated peaks must be clamped: {}",
            mixed[11_025]
        );
        engine.shutdown();
    }

    #[test]
    fn crossfade_normalizes_rate_and_channel_boundaries() {
        let dir = TempDir::new("crossfade-format");
        let a = write_dc_wav(&dir, "a.wav", 22_050, 44_100, 0.2);
        let b = write_dc_wav_channels(&dir, "b.wav", 44_100, 2, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 2);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(270), QueueEntryId::new(271));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(66_150);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        control.clear_captured();
        control.drain_frames(22_050);
        let mixed = control.captured();
        assert_eq!(mixed.len(), 22_050 * 2);
        for frame in 0..mixed.len() / 2 {
            let expected = expected_equal_power(0.2, 0.8, frame, 22_050);
            assert!(
                (mixed[frame * 2] - expected).abs() < 0.01,
                "left frame {frame}"
            );
            assert!(
                (mixed[frame * 2 + 1] - expected).abs() < 0.01,
                "right frame {frame}"
            );
        }
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                boundary_frame: 66_150,
                ..
            } if *to_id == id_b
        )));
        engine.shutdown();
    }

    #[test]
    fn pause_freezes_the_crossfade_envelope_until_resume() {
        let dir = TempDir::new("crossfade-pause");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(280), QueueEntryId::new(281));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(66_150);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        control.clear_captured();
        control.drain_frames(5_000);
        engine.pause();
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Paused { .. }));
        let frozen = engine.position();
        control.drain_frames(1_000);
        assert_eq!(
            engine.position(),
            frozen,
            "paused output must not advance the envelope"
        );

        engine.play();
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Started { .. })
        });
        control.clear_captured();
        control.drain_frames(1_000);
        let resumed = control.captured();
        for (offset, sample) in resumed.iter().enumerate() {
            let expected = expected_equal_power(0.2, 0.8, 5_000 + offset, 22_050);
            assert!((sample - expected).abs() < 0.01, "frame {}", 5_000 + offset);
        }
        engine.shutdown();
    }

    #[test]
    fn stop_invalidates_a_queued_crossfade_without_a_transition() {
        let dir = TempDir::new("crossfade-stop");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(290), QueueEntryId::new(291));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(1_000);
        engine.stop();
        let events = collect_events_until(&engine, |event| matches!(event, EngineEvent::Stopped));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Stopped))
        );
        control.clear_captured();
        control.drain_frames(4_096);
        while let Ok(event) = engine.events().try_recv() {
            assert!(
                !matches!(event, EngineEvent::Transitioned { .. }),
                "stopped crossfade leaked a transition: {event:?}"
            );
        }
        assert!(control.captured().iter().all(|s| *s == 0.0));
        engine.shutdown();
    }

    #[test]
    fn reauthorizing_during_crossfade_discards_the_old_successor() {
        let dir = TempDir::new("crossfade-reauthorize");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.5);
        let c = write_dc_wav(&dir, "c.wav", 44_100, 88_200, 0.9);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b, id_c) = (TrackId::new(1), TrackId::new(2), TrackId::new(3));
        let (entry_a, entry_b, entry_c) = (
            QueueEntryId::new(300),
            QueueEntryId::new(301),
            QueueEntryId::new(302),
        );

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        engine.preload_entry(id_c, entry_c, c);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_c),
        );
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(66_150);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                to_entry,
                boundary_frame: 66_150,
                ..
            } if *to_id == id_c && *to_entry == entry_c
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned { to_id, .. } if *to_id == id_b
        )));
        control.clear_captured();
        control.drain_frames(22_050);
        let mixed = control.captured();
        assert!((mixed[0] - 0.2).abs() < 0.01);
        assert!((mixed[22_049] - 0.9).abs() < 0.01);
        engine.shutdown();
    }

    #[test]
    fn consecutive_crossfades_preserve_queue_entry_identity() {
        let dir = TempDir::new("crossfade-consecutive");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.5);
        let c = write_dc_wav(&dir, "c.wav", 44_100, 88_200, 0.9);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b, id_c) = (TrackId::new(1), TrackId::new(2), TrackId::new(3));
        let (entry_a, entry_b, entry_c) = (
            QueueEntryId::new(310),
            QueueEntryId::new(311),
            QueueEntryId::new(312),
        );

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(66_150);
        let first = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(first.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                from_entry,
                to_entry,
                boundary_frame: 66_150,
                ..
            } if *from_entry == entry_a && *to_entry == entry_b
        )));
        engine.preload_entry(id_c, entry_c, c);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_c),
        );
        let second = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(
            second.iter().any(|event| matches!(
                event,
                EngineEvent::Transitioned {
                    from_id,
                    from_entry,
                    to_id,
                    to_entry,
                    boundary_frame: 132_300,
                    ..
                } if *from_id == id_b
                    && *from_entry == entry_b
                    && *to_id == id_c
                    && *to_entry == entry_c
            )),
            "second crossfade must start at B's tail: {second:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn duplicate_track_ids_crossfade_with_distinct_entry_ids() {
        let dir = TempDir::new("crossfade-duplicate");
        let path = write_dc_wav(&dir, "same.wav", 44_100, 88_200, 0.5);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let id = TrackId::new(9);
        let (entry_a, entry_b) = (QueueEntryId::new(320), QueueEntryId::new(321));

        engine.load_entry(id, entry_a, path.clone());
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id, entry_b, path);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(66_150);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                from_id,
                from_entry,
                to_id,
                to_entry,
                boundary_frame: 66_150,
                ..
            } if *from_id == id
                && *to_id == id
                && *from_entry == entry_a
                && *to_entry == entry_b
        )));
        engine.shutdown();
    }

    #[test]
    fn truncated_incoming_crossfade_never_reports_success() {
        let dir = TempDir::new("crossfade-truncated");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let full = write_dc_wav(&dir, "b-full.wav", 44_100, 88_200, 0.8);
        let truncated = dir.0.join("b.wav");
        let mut bytes = fs::read(&full).expect("read full wav");
        bytes.truncate(bytes.len() - 100);
        fs::write(&truncated, bytes).expect("write truncated wav");
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(330), QueueEntryId::new(331));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, truncated);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::Error { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Error { id: Some(id), .. } if *id == id_b
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. }))
        );
        engine.shutdown();
    }

    #[test]
    fn seek_before_crossfade_completion_rebuilds_the_tail_window() {
        let dir = TempDir::new("crossfade-seek");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(340), QueueEntryId::new(341));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b.clone());
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(1_000);
        let _ = engine.seek(Duration::from_millis(200));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Preloaded { id, .. } if *id == id_b),
        );
        // Seek to 8_820 output frames, leaving 57_330 before the 22_050
        // frame tail window. The old 66_150 boundary must not survive.
        wait_for_pushed_frames(&engine, &control, 79_380);
        control.drain_frames(57_330);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Transitioned { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                to_id,
                boundary_frame: 57_330,
                ..
            } if *to_id == id_b
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            EngineEvent::Transitioned {
                boundary_frame: 66_150,
                ..
            }
        )));
        control.clear_captured();
        control.drain_frames(22_050);
        let mixed = control.captured();
        assert!((mixed[0] - 0.2).abs() < 0.01);
        assert!((mixed[22_049] - 0.8).abs() < 0.01);
        engine.shutdown();
    }

    #[test]
    fn cancelling_a_queued_crossfade_resumes_current_and_ends_it() {
        let dir = TempDir::new("crossfade-cancel");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(350), QueueEntryId::new(351));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(1_000);
        engine.cancel_preload();
        // Pause is queued after cancellation, proving it was processed.
        engine.pause();
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Paused { .. }));
        engine.play();
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Started { .. })
        });
        let events = drive_until(&engine, &control, |event| {
            matches!(event, EngineEvent::TrackEnded { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::TrackEnded { entry, id, .. }
                if *entry == entry_a && *id == id_a
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. }))
        );
        engine.shutdown();
    }

    #[test]
    fn crossfade_preload_failure_drains_tail_and_ends_normally() {
        let dir = TempDir::new("crossfade-preload-failure");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let bad = dir.0.join("bad.wav");
        fs::write(&bad, b"not a real wav").expect("bad fixture");
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b) = (TrackId::new(1), TrackId::new(2));
        let (entry_a, entry_b) = (QueueEntryId::new(360), QueueEntryId::new(361));

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, bad);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::PreloadFailed { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::PreloadFailed { id, .. } if *id == id_b
        )));
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);
        control.drain_frames(88_200);
        let events = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::TrackEnded { .. })
        });
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::TrackEnded { entry, id, .. }
                if *entry == entry_a && *id == id_a
        )));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. }))
        );
        engine.shutdown();
    }

    #[test]
    fn explicit_load_during_a_queued_crossfade_replaces_both_sources() {
        let dir = TempDir::new("crossfade-explicit-load");
        let a = write_dc_wav(&dir, "a.wav", 44_100, 88_200, 0.2);
        let b = write_dc_wav(&dir, "b.wav", 44_100, 88_200, 0.5);
        let c = write_dc_wav(&dir, "c.wav", 44_100, 88_200, 0.9);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_crossfade_ms(500);
        let (id_a, id_b, id_c) = (TrackId::new(1), TrackId::new(2), TrackId::new(3));
        let (entry_a, entry_b, entry_c) = (
            QueueEntryId::new(370),
            QueueEntryId::new(371),
            QueueEntryId::new(372),
        );

        engine.load_entry(id_a, entry_a, a);
        let _ = collect_events_until(&engine, |event| matches!(event, EngineEvent::Loaded { .. }));
        engine.preload_entry(id_b, entry_b, b);
        let _ = collect_events_until(&engine, |event| {
            matches!(event, EngineEvent::Preloaded { .. })
        });
        engine.play();
        start_playing(&engine);
        wait_for_pushed_frames(&engine, &control, 88_200);

        engine.load_entry(id_c, entry_c, c);
        let events = collect_events_until(
            &engine,
            |event| matches!(event, EngineEvent::Loaded { id, .. } if *id == id_c),
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::Loaded { id, .. } if *id == id_c))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EngineEvent::Transitioned { .. }))
        );
        engine.play();
        start_playing(&engine);
        control.drain_frames(4_096);
        while let Ok(event) = engine.events().try_recv() {
            assert!(
                !matches!(event, EngineEvent::Transitioned { .. }),
                "explicit load leaked the old crossfade: {event:?}"
            );
        }
        engine.shutdown();
    }

    #[test]
    fn load_and_play_errors_are_typed_events() {
        let dir = TempDir::new("errors");
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);

        // Play with nothing loaded.
        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Error { .. })
        });
        assert!(matches!(
            events.last(),
            Some(EngineEvent::Error {
                error: EngineError::NoTrackLoaded,
                ..
            })
        ));

        // Unsupported format (file exists, extension not playable:
        // AIFF is indexed by the library but has no decoder).
        let aif = dir.0.join("song.aif");
        fs::write(&aif, b"whatever").expect("write");
        engine.load(TrackId::new(5), aif);
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Error { .. })
        });
        assert!(matches!(
            events.last(),
            Some(EngineEvent::Error {
                session: Some(_),
                id: Some(_),
                error: EngineError::Decode(DecodeError::UnsupportedFormat { .. }),
                ..
            })
        ));

        // Malformed contents under decodable extensions (WAV, MP3,
        // M4A): each is a typed Decode(Malformed) engine error, never
        // a silent skip or a crash.
        for (id, name) in [(6, "bad.wav"), (60, "bad.mp3"), (61, "bad.m4a")] {
            let bad = dir.0.join(name);
            fs::write(&bad, b"not the format the extension promises").expect("write");
            engine.load(TrackId::new(id), bad);
            let events = drive_until(&engine, &control, |e| {
                matches!(e, EngineEvent::Error { .. })
            });
            assert!(
                matches!(
                    events.last(),
                    Some(EngineEvent::Error {
                        error: EngineError::Decode(DecodeError::Malformed { .. }),
                        ..
                    })
                ),
                "{name}: {events:?}"
            );
        }

        // Missing file.
        engine.load(TrackId::new(7), dir.0.join("missing.flac"));
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Error { .. })
        });
        assert!(matches!(
            events.last(),
            Some(EngineEvent::Error {
                error: EngineError::Decode(DecodeError::Io(_)),
                ..
            })
        ));
        engine.shutdown();
    }

    /// Committed format fixtures (same files the decoder tests use;
    /// see tests/fixtures/README.md for provenance).
    const OGG_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone.ogg");
    const OPUS_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone.opus");
    const AAC_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone.aac");
    const M4A_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone.m4a");
    const MP3_FIXTURE: &[u8] = include_bytes!("../tests/fixtures/silence.mp3");

    #[test]
    fn compressed_formats_play_through_engine_with_audible_content() {
        let dir = TempDir::new("formats");
        // (name, bytes, expected Loaded rate/channels, min nonzero frames
        // at the 44.1 kHz mono null device)
        let cases: [(&str, &[u8], u32, u16, usize); 4] = [
            ("tone.ogg", OGG_FIXTURE, 44_100, 2, 18_000),
            ("tone.opus", OPUS_FIXTURE, 48_000, 1, 9_500),
            ("tone.aac", AAC_FIXTURE, 44_100, 2, 11_500),
            ("tone.m4a", M4A_FIXTURE, 44_100, 2, 11_500),
        ];
        for (name, bytes, rate, channels, min_nonzero) in cases {
            let path = dir.0.join(name);
            fs::write(&path, bytes).expect("write fixture");
            let (engine, control) = EngineHandle::spawn_null(44_100, 1);
            let id = TrackId::new(42);
            engine.load(id, path);
            let events = drive_until(&engine, &control, |e| {
                matches!(e, EngineEvent::Loaded { .. } | EngineEvent::Error { .. })
            });
            assert!(
                matches!(events.last(), Some(EngineEvent::Loaded { id: got, info, .. })
                    if *got == id && info.sample_rate == rate && info.channels == channels),
                "{name}: unexpected load outcome {events:?}"
            );
            engine.play();
            let events = drive_until(&engine, &control, |e| {
                matches!(
                    e,
                    EngineEvent::TrackEnded { .. } | EngineEvent::Error { .. }
                )
            });
            assert!(
                matches!(events.last(), Some(EngineEvent::TrackEnded { id: got, .. }) if *got == id),
                "{name}: expected TrackEnded, got {events:?}"
            );
            let nonzero = nonzero_frames(&control, 1);
            assert!(
                nonzero >= min_nonzero,
                "{name}: only {nonzero} nonzero frames reached the device"
            );
            let peak = control
                .captured()
                .iter()
                .fold(0.0f32, |m, s| m.max(s.abs()));
            assert!((0.15..0.5).contains(&peak), "{name}: peak {peak}");
            engine.shutdown();
        }
    }

    #[test]
    fn mp3_silence_advances_to_track_end_and_replacing_formats_is_clean() {
        let dir = TempDir::new("mp3-replace");
        let ogg_path = dir.0.join("tone.ogg");
        fs::write(&ogg_path, OGG_FIXTURE).expect("write");
        let mp3_path = dir.0.join("silence.mp3");
        fs::write(&mp3_path, MP3_FIXTURE).expect("write");
        let (engine, control) = EngineHandle::spawn_null(44_100, 2);
        let ogg_id = TrackId::new(1);
        let mp3_id = TrackId::new(2);

        engine.load(ogg_id, ogg_path);
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Loaded { .. })
        });
        engine.play();
        for _ in 0..6 {
            control.drain_frames(512);
            std::thread::sleep(Duration::from_millis(5));
        }

        // Replace mid-playback with the (silent) MP3: the Ogg track
        // must never report TrackEnded, the MP3 must play its full
        // 20-frame span and end cleanly despite being digital silence.
        engine.load(mp3_id, mp3_path);
        let events = drive_until(
            &engine,
            &control,
            |e| matches!(e, EngineEvent::Loaded { id, .. } if *id == mp3_id),
        );
        assert!(
            matches!(events.last(), Some(EngineEvent::Loaded { info, .. })
                if info.sample_rate == 44_100 && info.channels == 2),
            "{events:?}"
        );
        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(
                e,
                EngineEvent::TrackEnded { .. } | EngineEvent::Error { .. }
            )
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::TrackEnded { id, .. }) if *id == mp3_id),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, EngineEvent::TrackEnded { id, .. } if *id == ogg_id)),
            "replaced track must not report TrackEnded: {events:?}"
        );
        engine.shutdown();
    }

    #[test]
    fn pause_and_resume_work_on_a_vorbis_track() {
        let dir = TempDir::new("Vorbis-pause");
        let path = dir.0.join("tone.ogg");
        fs::write(&path, OGG_FIXTURE).expect("write");
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.load(TrackId::new(1), path);
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Loaded { .. })
        });
        engine.play();
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Started { .. })
        });
        for _ in 0..8 {
            control.drain_frames(512);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(nonzero_frames(&control, 1) > 0);

        engine.pause();
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Paused { .. })
        });
        let at_pause = nonzero_frames(&control, 1);
        for _ in 0..8 {
            control.drain_frames(512);
        }
        assert_eq!(
            nonzero_frames(&control, 1),
            at_pause,
            "paused Vorbis playback must emit silence only"
        );

        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(
                e,
                EngineEvent::TrackEnded { .. } | EngineEvent::Error { .. }
            )
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::TrackEnded { .. })),
            "{events:?}"
        );
        assert!(nonzero_frames(&control, 1) > at_pause);
        engine.shutdown();
    }

    #[test]
    fn volume_scales_output_and_rate_conversion_doubles_frames() {
        let dir = TempDir::new("volume-rate");
        // 22.05 kHz file into a 44.1 kHz device: expect ~2x frames out.
        let path = write_dc_wav(&dir, "half-rate.wav", 22_050, 5_512, 0.8);
        let (engine, control) = EngineHandle::spawn_null(44_100, 1);
        engine.set_volume(0.5);
        engine.load(TrackId::new(1), path);
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Loaded { .. })
        });
        engine.play();
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::TrackEnded { .. })
        });

        let frames = nonzero_frames(&control, 1);
        assert!(
            (10_900..=11_150).contains(&frames),
            "expected ≈11_024 output frames, got {frames}"
        );
        let peak = control
            .captured()
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            (peak - 0.4).abs() < 0.02,
            "0.8 DC at gain 0.5 should peak ≈0.4, got {peak}"
        );
        engine.shutdown();
    }

    #[test]
    fn shutdown_and_drop_do_not_hang() {
        let (engine, _control) = EngineHandle::spawn_null(44_100, 2);
        engine.shutdown();
        let (engine, _control) = EngineHandle::spawn_null(44_100, 2);
        drop(engine); // Drop must join the worker promptly.
    }

    /// A stereo 16-bit WAV of two distinct tones (L: 440 Hz cosine,
    /// R: 330 Hz sine). Unlike the DC fixtures, dropped, duplicated
    /// or reordered samples change the *content*, so exact-sequence
    /// comparisons can see them.
    fn write_tone_wav(dir: &TempDir, name: &str, rate: u32, frames: usize) -> PathBuf {
        let path = dir.0.join(name);
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create wav");
        for n in 0..frames {
            let t = n as f64 / f64::from(rate);
            let l = 0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).cos();
            let r = 0.4 * (2.0 * std::f64::consts::PI * 330.0 * t).sin();
            writer
                .write_sample((l * 32_767.0) as i32)
                .expect("write sample");
            writer
                .write_sample((r * 32_767.0) as i32)
                .expect("write sample");
        }
        writer.finalize().expect("finalize");
        path
    }

    /// The exact sample sequence the engine *should* deliver for a
    /// file: direct decode through the same converter, chunked
    /// exactly as the worker chunks (`DECODE_CHUNK_FRAMES`), flushed
    /// at EOF. Comparing device-captured output against this isolates
    /// the worker/ring/callback path from decoding and conversion.
    fn reference_pipeline(path: &std::path::Path, out: OutputConfig) -> Vec<f32> {
        let mut decoder = open_decoder(path).expect("decoder opens");
        let info = *decoder.info();
        let mut converter = FormatConverter::new(&info, out);
        let mut samples = Vec::new();
        loop {
            let chunk = decoder
                .next_chunk(super::DECODE_CHUNK_FRAMES)
                .expect("chunk decodes");
            if chunk.is_empty() {
                samples.extend(converter.finish());
                break;
            }
            samples.extend(converter.process(&chunk));
        }
        samples
    }

    fn first_diff(a: &[f32], b: &[f32]) -> Option<usize> {
        a.iter()
            .zip(b.iter())
            .position(|(x, y)| x != y)
            .or_else(|| (a.len() != b.len()).then_some(a.len().min(b.len())))
    }

    /// The converter must be chunking-invariant: feeding the same
    /// signal in any piece sizes (the worker feeds 4096-frame decode
    /// chunks; frame-quantised decoders make chunk lengths vary in
    /// practice) must yield the same number of output samples with
    /// the same values (up to f64 phase-rebasing rounding) at the
    /// ratios real setups use, above all 44.1 kHz files on 48 kHz
    /// devices.
    #[test]
    fn converter_chunked_input_matches_one_shot_at_real_ratios() {
        for (in_rate, out_rate) in [(44_100u32, 48_000u32), (48_000, 44_100), (44_100, 44_100)] {
            let frames = in_rate as usize * 2;
            let mut signal = Vec::with_capacity(frames * 2);
            for n in 0..frames {
                let t = n as f64 / f64::from(in_rate);
                signal.push((0.5 * (2.0 * std::f64::consts::PI * 440.0 * t).cos()) as f32);
                signal.push((0.4 * (2.0 * std::f64::consts::PI * 330.0 * t).sin()) as f32);
            }
            let info = StreamInfo {
                sample_rate: in_rate,
                channels: 2,
                duration: None,
            };
            let out_cfg = OutputConfig {
                sample_rate: out_rate,
                channels: 2,
            };
            let one_shot = {
                let mut conv = FormatConverter::new(&info, out_cfg);
                let mut v = conv.process(&signal);
                v.extend(conv.finish());
                v
            };
            for chunk_frames in [1usize, 7, 1000, 4096] {
                let mut conv = FormatConverter::new(&info, out_cfg);
                let mut v = Vec::new();
                for chunk in signal.chunks(chunk_frames * 2) {
                    v.extend(conv.process(chunk));
                }
                v.extend(conv.finish());
                // Values are compared with a tight tolerance, not
                // bitwise: each `emit` rebases the f64 phase to the
                // drained buffer, so chunk boundaries legitimately
                // change rounding at the ~1e-8 level (measured max
                // 3e-8 for this signal across ratios/chunkings). A
                // dropped, duplicated, or mis-interpolated sample
                // would err by ~1e-2 — six orders of magnitude above
                // the bound. Total length may differ by at most one
                // output frame: whether the residual fractional
                // phase at EOF earns one final "held" frame is an
                // end-of-stream rounding coin-flip (measured ±1
                // frame, common prefix always in tolerance).
                assert!(
                    v.len().abs_diff(one_shot.len()) <= 2,
                    "{in_rate}->{out_rate} Hz: {chunk_frames}-frame chunks change the output \
                     length by more than one frame ({} vs {})",
                    v.len(),
                    one_shot.len()
                );
                let beyond = v
                    .iter()
                    .zip(one_shot.iter())
                    .position(|(x, y)| (x - y).abs() > 1e-6);
                assert_eq!(
                    beyond, None,
                    "{in_rate}->{out_rate} Hz: {chunk_frames}-frame chunks diverge from one-shot \
                     beyond phase-rounding tolerance"
                );
            }
            // Duration is preserved to within one output frame.
            let expected_frames = frames as f64 * f64::from(out_rate) / f64::from(in_rate);
            let got_frames = one_shot.len() / 2;
            assert!(
                (got_frames as f64 - expected_frames).abs() <= 1.5,
                "{in_rate}->{out_rate} Hz: {got_frames} frames out, expected ≈{expected_frames:.1}"
            );
        }
    }

    /// End-to-end content integrity through worker → ring → callback
    /// with resampling (44.1 kHz file, 48 kHz device — the most
    /// common real-world shape): every converted sample must arrive
    /// exactly once, in order, bracketed only by silence — so a
    /// starved fill, which would insert zeros mid-sequence, fails
    /// here too. DC fixtures cannot see reordering or duplication;
    /// this can.
    #[test]
    fn end_to_end_tone_delivery_is_sample_exact() {
        let dir = TempDir::new("tone-e2e");
        let path = write_tone_wav(&dir, "tone.wav", 44_100, 44_100 * 3);
        let (engine, control) = EngineHandle::spawn_null(48_000, 2);
        let id = TrackId::new(1);
        engine.load(id, path.clone());
        drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Loaded { .. })
        });
        engine.play();
        // Wait until real audio has been captured (proves the
        // pipeline actually started), then run to track end.
        for _ in 0..4_000 {
            control.drain_frames(512);
            let _ = engine.events().recv_timeout(Duration::from_millis(5));
            if control.captured().iter().any(|s| *s != 0.0) {
                break;
            }
        }
        assert!(
            control.captured().iter().any(|s| *s != 0.0),
            "audio never started flowing"
        );
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::TrackEnded { .. })
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::TrackEnded { .. })),
            "{events:?}"
        );
        // 3.0 s of device frames were delivered (position is derived
        // from delivered frames).
        let millis = engine.position().as_millis();
        assert!((2_990..=3_010).contains(&millis), "position {millis} ms");

        let captured = control.captured();
        let reference = reference_pipeline(
            &path,
            OutputConfig {
                sample_rate: 48_000,
                channels: 2,
            },
        );
        assert!(reference.len() >= 128);
        let start = captured
            .windows(128)
            .position(|w| w == &reference[..128])
            .expect("reference sequence appears in the capture");
        assert!(
            captured[..start].iter().all(|s| *s == 0.0),
            "only silence may precede the track"
        );
        assert!(
            captured.len() >= start + reference.len(),
            "capture holds fewer samples than the decoded track"
        );
        assert_eq!(
            first_diff(&captured[start..start + reference.len()], &reference),
            None,
            "delivered samples diverge from direct decode+convert"
        );
        assert!(
            captured[start + reference.len()..]
                .iter()
                .all(|s| *s == 0.0),
            "only silence may follow the track"
        );
        engine.shutdown();
    }

    /// The callback-side source in isolation: irregular producer
    /// pushes and consumer fills must deliver the input sequence in
    /// exact order (silence appears only where the ring was short),
    /// played-frame accounting must track delivered frames, and the
    /// drained flag must rise only once the stream is at EOF *and*
    /// the ring is empty.
    #[test]
    fn sample_source_preserves_order_and_signals_drain_at_eof() {
        use ringbuf::traits::{Producer, Split};
        use std::sync::Arc;

        let (mut producer, consumer) = ringbuf::HeapRb::<f32>::new(4096).split();
        let (waveform, _waveform_consumer) =
            ringbuf::HeapRb::<f32>::new(WAVEFORM_WINDOW * 2).split();
        let shared = Arc::new(Shared::new());
        shared.gate_open.store(true, Ordering::Release);
        let mut source = SampleSource {
            consumer,
            waveform,
            shared: Arc::clone(&shared),
            channels: 2,
            seen_generation: 0,
        };
        // Strictly positive input: any 0.0 in the output is inserted
        // silence, never data.
        let input: Vec<f32> = (0..3000).map(|i| (i + 1) as f32 * 0.0001).collect();
        let mut output: Vec<f32> = Vec::new();
        let mut pushed = 0usize;
        let steps = [
            (6usize, 14usize),
            (200, 512),
            (2, 2),
            (1000, 300),
            (34, 900),
            (700, 62),
            (4, 512),
            (1054, 512),
        ];
        for (push_len, fill_len) in steps {
            let n = producer.push_slice(&input[pushed..pushed + push_len]);
            assert_eq!(n, push_len, "test keeps the ring far from full");
            pushed += n;
            let mut out = vec![0.0f32; fill_len];
            source.fill(&mut out);
            output.extend(out);
        }
        // Drain the remainder.
        for _ in 0..100 {
            if output.iter().filter(|s| **s != 0.0).count() == input.len() {
                break;
            }
            let mut out = vec![0.0f32; 512];
            source.fill(&mut out);
            output.extend(out);
        }
        let delivered: Vec<f32> = output.into_iter().filter(|s| *s != 0.0).collect();
        assert_eq!(
            first_diff(&delivered, &input),
            None,
            "samples dropped, duplicated, or reordered through the ring"
        );
        assert_eq!(
            shared.played_frames.load(Ordering::Relaxed),
            1500,
            "played-frame accounting tracks delivered frames"
        );

        // Drain signalling: an empty ring alone does not end the
        // stream; EOF while samples remain does not either. Both
        // together do. (The input above is fully delivered here, so
        // the ring is empty.)
        let mut out = vec![0.0f32; 512];
        source.fill(&mut out);
        assert!(
            !shared.drained.load(Ordering::Relaxed),
            "empty ring before EOF must not signal end-of-track"
        );
        assert!(out.iter().all(|s| *s == 0.0), "starved fill is silence");
        shared.decoder_eof.store(true, Ordering::Release);
        source.fill(&mut out);
        assert!(
            shared.drained.load(Ordering::Relaxed),
            "EOF with an empty ring signals end-of-track"
        );
    }

    #[test]
    fn waveform_tap_mixes_stereo_to_mono_and_keeps_signal_order() {
        use ringbuf::traits::{Producer, Split};
        use std::sync::Arc;

        let (mut audio_producer, audio_consumer) = ringbuf::HeapRb::<f32>::new(64).split();
        let (waveform, waveform_consumer) =
            ringbuf::HeapRb::<f32>::new(WAVEFORM_WINDOW * 2).split();
        let mut reader = WaveformReader::new(waveform_consumer);
        let shared = Arc::new(Shared::new());
        shared.gate_open.store(true, Ordering::Release);
        let mut source = SampleSource {
            consumer: audio_consumer,
            waveform,
            shared: Arc::clone(&shared),
            channels: 2,
            seen_generation: 0,
        };

        assert_eq!(
            audio_producer.push_slice(&[0.2, 0.4, -0.2, -0.4, 0.8, 0.0]),
            6
        );
        let mut out = [0.0; 6];
        source.fill(&mut out);
        let snapshot = reader.snapshot();
        assert_eq!(
            &snapshot[WAVEFORM_WINDOW - 3..],
            &[0.3, -0.3, 0.4],
            "stereo is averaged to mono in output order"
        );
    }

    #[test]
    fn converter_identity_stereo_to_mono_and_upsample() {
        let info = StreamInfo {
            sample_rate: 44_100,
            channels: 1,
            duration: None,
        };
        let out_cfg = OutputConfig {
            sample_rate: 44_100,
            channels: 1,
        };

        // Identity rate/channel: exact passthrough across chunk seams.
        let mut conv = FormatConverter::new(&info, out_cfg);
        let mut result = conv.process(&[0.0, 0.5, -0.5]);
        result.extend(conv.process(&[0.25]));
        result.extend(conv.finish());
        assert_eq!(result, vec![0.0, 0.5, -0.5, 0.25]);

        // Mono duplicates into stereo.
        let mut conv = FormatConverter::new(
            &info,
            OutputConfig {
                channels: 2,
                ..out_cfg
            },
        );
        let mut result = conv.process(&[0.3, -0.3]);
        result.extend(conv.finish());
        assert_eq!(result, vec![0.3, 0.3, -0.3, -0.3]);

        // Stereo averages into mono.
        let stereo = StreamInfo {
            channels: 2,
            ..info
        };
        let mut conv = FormatConverter::new(&stereo, out_cfg);
        let mut result = conv.process(&[1.0, -1.0, 0.5, 0.5]);
        result.extend(conv.finish());
        assert_eq!(result, vec![0.0, 0.5]);

        // 2x upsample inserts linear midpoints; end-of-stream holds the
        // final frame for the fractional remainder so 2 input frames
        // yield 4 output frames (duration is preserved).
        let mut conv = FormatConverter::new(
            &StreamInfo {
                sample_rate: 22_050,
                ..info
            },
            OutputConfig {
                sample_rate: 44_100,
                channels: 1,
            },
        );
        let mut result = conv.process(&[0.0, 1.0]);
        result.extend(conv.finish());
        assert_eq!(result, vec![0.0, 0.5, 1.0, 1.0]);
    }
}
