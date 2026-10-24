//! The playback engine: a worker thread that decodes local files and
//! feeds an [`OutputBackend`], driven by commands and reporting events.
//!
//! # Architecture
//!
//! ```text
//! caller (any thread)                 engine worker thread
//! ┌──────────────────┐  commands   ┌──────────────────────────────┐
//! │  EngineHandle    │ ──────────▶ │ command loop                 │
//! │  (this module)   │  (mpsc)     │  ├─ Decoder (decode.rs)      │
//! │                  │ ◀────────── │  ├─ FormatConverter          │
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
//!   SPSC ring buffer and fills silence on underrun — no allocation,
//!   locking, or I/O on the real-time thread.
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
//!   — never merely when decoding finished.
//!
//! The engine knows nothing about libraries, queues, or scanning: the
//! caller hands it a [`TrackId`] (echoed back in events) and a file
//! path. [`crate::Player`] remains the single authority for queue/playback
//! *decisions*; the engine is the mechanism that makes them audible.

use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use flux_core::TrackId;
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};

use crate::decode::{DecodeError, Decoder, StreamInfo, open_decoder};
use crate::output::{FillCallback, NullControl, NullOutput, OutputBackend, OutputConfig};

/// Frames decoded per worker iteration (per channel).
const DECODE_CHUNK_FRAMES: usize = 4096;
/// Worker loop tick: how long to wait for a command before pumping
/// more samples / checking end-of-track.
const WORKER_TICK: Duration = Duration::from_millis(5);

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
        /// The caller-supplied track ID.
        id: TrackId,
        /// The decoded stream's facts (file sample rate/channels).
        info: StreamInfo,
    },
    /// Playback of the track started or resumed.
    Started {
        /// The track that started.
        id: TrackId,
    },
    /// Playback of the track paused.
    Paused {
        /// The track that paused.
        id: TrackId,
    },
    /// Playback stopped and the loaded track was unloaded.
    Stopped,
    /// The track played to completion: every decoded sample has been
    /// consumed by the backend.
    TrackEnded {
        /// The track that ended.
        id: TrackId,
    },
    /// Something failed. `id` is the track concerned, when applicable.
    /// After a decode error the engine is stopped with nothing playing;
    /// the caller (via [`crate::Player`]) decides what happens next.
    Error {
        /// The track concerned, if any.
        id: Option<TrackId>,
        /// What failed.
        error: EngineError,
    },
}

/// Commands understood by the worker (handle methods enqueue these).
enum Command {
    Load { id: TrackId, path: PathBuf },
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
    /// Device frames actually delivered for the current generation.
    played_frames: AtomicU64,
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
        }
    }

    /// Starts a new generation (worker thread only): silences the
    /// callback and arms its flush-on-generation-change.
    fn bump_generation(&self) -> u64 {
        self.gate_open.store(false, Ordering::Release);
        self.decoder_eof.store(false, Ordering::Release);
        self.drained.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// The callback-side sample source living inside the backend.
/// Real-time safe: pops from the lock-free ring, applies gain, counts
/// delivered frames, and outputs silence whenever gated or starved.
struct SampleSource {
    consumer: ringbuf::HeapCons<f32>,
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
            return;
        }
        if !self.shared.gate_open.load(Ordering::Acquire) {
            out.fill(0.0);
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
    }
}

/// Handle to a running playback engine. All methods are non-blocking
/// (channel sends and atomic reads) and safe to call from a UI thread.
pub struct EngineHandle {
    commands: Sender<Command>,
    events: Receiver<EngineEvent>,
    shared: Arc<Shared>,
    config: OutputConfig,
    worker: Option<std::thread::JoinHandle<()>>,
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
        let worker = std::thread::Builder::new()
            .name("flux-audio-engine".to_owned())
            .spawn(move || run_worker(make_backend, command_rx, event_tx, init_tx, worker_shared))
            .map_err(|err| EngineError::Output(format!("could not spawn engine thread: {err}")))?;
        match init_rx.recv() {
            Ok(Ok(config)) => Ok(Self {
                commands: command_tx,
                events: event_rx,
                shared,
                config,
                worker: Some(worker),
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
    pub fn load(&self, id: TrackId, path: PathBuf) {
        let _ = self.commands.send(Command::Load { id, path });
    }

    /// Starts or resumes playback of the loaded track. If the track
    /// previously played to completion, it restarts from the beginning.
    /// Emits [`EngineEvent::Started`], or [`EngineEvent::Error`] with
    /// [`EngineError::NoTrackLoaded`] when nothing is loaded.
    pub fn play(&self) {
        let _ = self.commands.send(Command::Play);
    }

    /// Pauses playback (position is retained). A no-op unless playing.
    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }

    /// Stops playback and unloads the current track.
    pub fn stop(&self) {
        let _ = self.commands.send(Command::Stop);
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
        let frames = self.shared.played_frames.load(Ordering::Relaxed);
        Duration::from_secs_f64(frames as f64 / f64::from(self.config.sample_rate))
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
    path: PathBuf,
    info: StreamInfo,
}

/// The worker loop. Owns the backend, decoder, converter, and ring
/// producer; everything else lives in [`Shared`] atomics.
fn run_worker<F>(
    make_backend: F,
    commands: Receiver<Command>,
    events: Sender<EngineEvent>,
    init: std::sync::mpsc::SyncSender<Result<OutputConfig, EngineError>>,
    shared: Arc<Shared>,
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
        channels: usize::from(config.channels),
        seen_generation: shared.generation.load(Ordering::Acquire),
        shared: Arc::clone(&shared),
    };
    let error_events = events.clone();
    let on_error = Box::new(move |message: String| {
        let _ = error_events.send(EngineEvent::Error {
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
    let mut current: Option<LoadedTrack> = None;
    let mut converter: Option<FormatConverter> = None;
    let mut pending: Vec<f32> = Vec::new();
    let mut pending_sent = 0usize;
    let mut playing = false;
    let mut eof_reached = false;
    let mut ended_sent = false;
    // The generation this worker's decoder/pending state belongs to.
    // Only the worker bumps generations, so this mirror is exact.
    let mut current_generation = shared.generation.load(Ordering::Acquire);

    'worker: loop {
        match commands.recv_timeout(WORKER_TICK) {
            Ok(Command::Load { id, path }) => {
                current_generation = shared.bump_generation();
                decoder = None;
                current = None;
                converter = None;
                pending.clear();
                pending_sent = 0;
                playing = false;
                eof_reached = false;
                ended_sent = false;
                match open_decoder(&path) {
                    Ok(dec) => {
                        let info = *dec.info();
                        converter = Some(FormatConverter::new(&info, config));
                        decoder = Some(dec);
                        current = Some(LoadedTrack { id, path, info });
                        send(EngineEvent::Loaded { id, info });
                    }
                    Err(err) => send(EngineEvent::Error {
                        id: Some(id),
                        error: err.into(),
                    }),
                }
            }
            Ok(Command::Play) => match &current {
                None => send(EngineEvent::Error {
                    id: None,
                    error: EngineError::NoTrackLoaded,
                }),
                Some(track) => {
                    if decoder.is_none() {
                        // The track previously ended (or errored):
                        // pressing play restarts it from the beginning.
                        current_generation = shared.bump_generation();
                        pending.clear();
                        pending_sent = 0;
                        eof_reached = false;
                        ended_sent = false;
                        match open_decoder(&track.path) {
                            Ok(dec) => {
                                converter = Some(FormatConverter::new(&track.info, config));
                                decoder = Some(dec);
                            }
                            Err(err) => {
                                send(EngineEvent::Error {
                                    id: Some(track.id),
                                    error: err.into(),
                                });
                                continue 'worker;
                            }
                        }
                    }
                    playing = true;
                    shared.gate_open.store(true, Ordering::Release);
                    send(EngineEvent::Started { id: track.id });
                }
            },
            Ok(Command::Pause) => {
                if playing {
                    playing = false;
                    shared.gate_open.store(false, Ordering::Release);
                    if let Some(track) = &current {
                        send(EngineEvent::Paused { id: track.id });
                    }
                }
            }
            Ok(Command::Stop) => {
                current_generation = shared.bump_generation();
                decoder = None;
                current = None;
                converter = None;
                pending.clear();
                pending_sent = 0;
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
        if generation_live && pending_sent < pending.len() {
            pending_sent += producer.push_slice(&pending[pending_sent..]);
            if pending_sent == pending.len() {
                pending.clear();
                pending_sent = 0;
            }
        } else if generation_live
            && !eof_reached
            && let Some(dec) = decoder.as_mut()
        {
            let chunk_samples = DECODE_CHUNK_FRAMES * usize::from(config.channels);
            if producer.vacant_len() >= chunk_samples {
                match dec.next_chunk(DECODE_CHUNK_FRAMES) {
                    Ok(samples) if samples.is_empty() => {
                        if let Some(conv) = converter.as_mut() {
                            pending = conv.finish();
                        }
                        eof_reached = true;
                    }
                    Ok(samples) => {
                        if let Some(conv) = converter.as_mut() {
                            pending = conv.process(&samples);
                        }
                    }
                    Err(err) => {
                        let id = current.as_ref().map(|t| t.id);
                        send(EngineEvent::Error {
                            id,
                            error: err.into(),
                        });
                        decoder = None;
                        playing = false;
                        shared.gate_open.store(false, Ordering::Release);
                        // Output stops here: with the gate closed the
                        // callback emits silence, so samples still
                        // buffered are never played (the next
                        // generation change discards them). Never
                        // report a clean TrackEnded after an error.
                        ended_sent = true;
                        eof_reached = true;
                    }
                }
            }
        }
        if eof_reached && pending_sent == pending.len() {
            shared.decoder_eof.store(true, Ordering::Release);
        }

        // End-of-track: decoder EOF + ring fully drained (reported by
        // the callback through Shared::drained).
        if playing && !ended_sent && shared.drained.load(Ordering::Acquire) {
            if let Some(track) = &current {
                send(EngineEvent::TrackEnded { id: track.id });
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

    use flux_core::TrackId;

    use super::{EngineError, EngineEvent, EngineHandle, FormatConverter, SampleSource, Shared};
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
            matches!(events.last(), Some(EngineEvent::Loaded { id: got, info }) if *got == id && info.sample_rate == 44_100)
        );

        engine.play();
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::TrackEnded { .. })
        });
        assert!(
            matches!(events.last(), Some(EngineEvent::TrackEnded { id: got }) if *got == id),
            "expected TrackEnded, got {events:?}"
        );
        // All 11_025 DC frames passed through (silence padding aside).
        assert_eq!(nonzero_frames(&control, 1), 11_025);
        assert!((engine.position().as_millis() as i64 - 250).abs() <= 12);
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
                .all(|e| !matches!(e, EngineEvent::TrackEnded { id } if *id == id_a)),
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
            matches!(events.last(), Some(EngineEvent::Paused { id: got }) if *got == id),
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
            matches!(events.last(), Some(EngineEvent::TrackEnded { id: got }) if *got == id),
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
            matches!(events.last(), Some(EngineEvent::Paused { id: got }) if *got == id_b),
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
            matches!(events.last(), Some(EngineEvent::TrackEnded { id: got }) if *got == id_b),
            "expected TrackEnded for track B, got {events:?}"
        );
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, EngineEvent::TrackEnded { id } if *id == id_a)),
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
                id: Some(_),
                error: EngineError::Decode(DecodeError::UnsupportedFormat { .. })
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
                matches!(events.last(), Some(EngineEvent::Loaded { id: got, info })
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
                matches!(events.last(), Some(EngineEvent::TrackEnded { id: got }) if *got == id),
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
            matches!(events.last(), Some(EngineEvent::TrackEnded { id }) if *id == mp3_id),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .all(|e| !matches!(e, EngineEvent::TrackEnded { id } if *id == ogg_id)),
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
        let shared = Arc::new(Shared::new());
        shared.gate_open.store(true, Ordering::Release);
        let mut source = SampleSource {
            consumer,
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
