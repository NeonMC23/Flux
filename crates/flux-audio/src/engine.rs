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
//!   discards buffered samples when the generation changes, and the
//!   worker only emits end-of-track for the current generation, so
//!   rapid track changes can never leak stale samples or stale
//!   `TrackEnded` events.
//! - [`EngineEvent::TrackEnded`] fires only after the decoder reached
//!   EOF **and** the ring buffer has fully drained through the backend
//!   — never merely when decoding finished.
//!
//! The engine knows nothing about libraries, queues, or scanning: the
//! caller hands it a [`TrackId`] (echoed back in events) and a file
//! path. [`Player`] remains the single authority for queue/playback
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
    /// the caller (via [`Player`]) decides what happens next.
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
            // from the old generation before producing any sound.
            self.seen_generation = generation;
            self.consumer.clear();
            self.shared.played_frames.store(0, Ordering::Release);
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

    'worker: loop {
        match commands.recv_timeout(WORKER_TICK) {
            Ok(Command::Load { id, path }) => {
                shared.bump_generation();
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
                        shared.bump_generation();
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
                shared.bump_generation();
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

        // Pump converted samples into the ring. Decoding continues
        // while paused (the bounded ring applies backpressure), so
        // resuming is instant; only an absent/exhausted decoder idles.
        if pending_sent < pending.len() {
            pending_sent += producer.push_slice(&pending[pending_sent..]);
            if pending_sent == pending.len() {
                pending.clear();
                pending_sent = 0;
            }
        } else if !eof_reached && let Some(dec) = decoder.as_mut() {
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
                        // Let already-queued samples drain, but do not
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

    use super::{EngineError, EngineEvent, EngineHandle, FormatConverter};
    use crate::decode::{DecodeError, StreamInfo};
    use crate::output::{NullControl, OutputConfig};

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

        // Unsupported format (file exists, extension not playable).
        let mp3 = dir.0.join("song.mp3");
        fs::write(&mp3, b"whatever").expect("write");
        engine.load(TrackId::new(5), mp3);
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

        // Malformed WAV.
        let bad = dir.0.join("bad.wav");
        fs::write(&bad, b"not a wav").expect("write");
        engine.load(TrackId::new(6), bad);
        let events = drive_until(&engine, &control, |e| {
            matches!(e, EngineEvent::Error { .. })
        });
        assert!(matches!(
            events.last(),
            Some(EngineEvent::Error {
                error: EngineError::Decode(DecodeError::Malformed { .. }),
                ..
            })
        ));

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
