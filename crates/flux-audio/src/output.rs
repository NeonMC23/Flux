//! Audio output backends: where decoded samples go to become sound
//! (or, in tests, to be counted).
//!
//! The engine talks to exactly one [`OutputBackend`]. A backend owns the
//! output stream and repeatedly asks a [`FillCallback`] to fill sample
//! buffers. Two backends exist:
//!
//! - [`NullOutput`] — a device-free backend whose "device" is driven
//!   manually via [`NullControl::drain_frames`]. Always available; this
//!   is what headless tests and CI use.
//! - `CpalOutput` (behind the non-default `cpal` Cargo feature) — real
//!   device output through CPAL. When the feature is off, no code in
//!   this crate can produce sound, and nothing claims otherwise.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::engine::EngineError;

/// The sample format a backend consumes: interleaved `f32` frames at
/// this rate and channel count. The engine converts decoded audio to
/// exactly this shape before handing it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputConfig {
    /// Frames per second.
    pub sample_rate: u32,
    /// Interleaved channel count.
    pub channels: u16,
}

/// Supplies interleaved `f32` samples to an output backend.
///
/// # Real-time contract
///
/// `fill` runs on the backend's highest-priority thread (for CPAL, the
/// device callback). Implementations must not allocate, lock contended
/// mutexes, perform I/O, or block. The engine's implementation pops
/// from a lock-free SPSC ring buffer and writes silence on underrun.
pub trait FillCallback: Send {
    /// Writes the next samples into `out` (whose length is a multiple
    /// of the channel count), filling any remainder with silence.
    fn fill(&mut self, out: &mut [f32]);
}

/// A destination for decoded samples. Implemented by [`NullOutput`]
/// always and by `CpalOutput` under the `cpal` feature.
///
/// Backends are created and started on the engine's worker thread
/// (some platforms pin audio streams to a thread), so `start` receives
/// everything it needs by value.
pub trait OutputBackend: Send {
    /// The format this backend consumes; fixed for its lifetime.
    fn config(&self) -> OutputConfig;

    /// Begins pulling samples from `fill`. Stream/runtime failures
    /// after a successful start are reported through `on_error`
    /// (called from the backend's own threads; must not block).
    ///
    /// # Errors
    ///
    /// [`EngineError::Output`] if the device/stream cannot be opened
    /// or started.
    fn start(
        &mut self,
        fill: Box<dyn FillCallback>,
        on_error: Box<dyn FnMut(String) + Send>,
    ) -> Result<(), EngineError>;

    /// Stops pulling samples and releases the stream. Safe to call
    /// when never started.
    fn stop(&mut self);
}

// ----------------------------------------------------------- NullOutput ---

struct NullShared {
    fill: Mutex<Option<Box<dyn FillCallback>>>,
    captured: Mutex<Vec<f32>>,
    running: AtomicBool,
}

/// A device-free [`OutputBackend`] driven manually through
/// [`NullControl`]: nothing is pulled until a test (or other driver)
/// calls [`NullControl::drain_frames`], which makes engine behavior
/// fully deterministic without audio hardware. It never claims to be a
/// real device.
pub struct NullOutput {
    config: OutputConfig,
    shared: Arc<NullShared>,
}

/// The driver half of a [`NullOutput`]: steps the virtual device and
/// inspects every sample it consumed.
#[derive(Clone)]
pub struct NullControl {
    shared: Arc<NullShared>,
    channels: u16,
}

impl NullOutput {
    /// Creates a null backend consuming `sample_rate` Hz, `channels`
    /// interleaved channels, plus its control handle.
    #[must_use]
    pub fn new(sample_rate: u32, channels: u16) -> (Self, NullControl) {
        let shared = Arc::new(NullShared {
            fill: Mutex::new(None),
            captured: Mutex::new(Vec::new()),
            running: AtomicBool::new(false),
        });
        (
            Self {
                config: OutputConfig {
                    sample_rate,
                    channels,
                },
                shared: Arc::clone(&shared),
            },
            NullControl { shared, channels },
        )
    }
}

impl OutputBackend for NullOutput {
    fn config(&self) -> OutputConfig {
        self.config
    }

    fn start(
        &mut self,
        fill: Box<dyn FillCallback>,
        _on_error: Box<dyn FnMut(String) + Send>,
    ) -> Result<(), EngineError> {
        *self.shared.fill.lock().expect("null output mutex") = Some(fill);
        self.shared.running.store(true, Ordering::Release);
        Ok(())
    }

    fn stop(&mut self) {
        self.shared.running.store(false, Ordering::Release);
    }
}

impl NullControl {
    /// Whether the backend has been started and not yet stopped.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    /// Simulates the device consuming exactly `frames` frames,
    /// appending every produced sample (including silence) to the
    /// capture buffer. Returns the number of frames produced — 0 when
    /// the backend is not running.
    pub fn drain_frames(&self, frames: usize) -> usize {
        if !self.is_running() {
            return 0;
        }
        let mut slot = self.shared.fill.lock().expect("null output mutex");
        let Some(fill) = slot.as_mut() else { return 0 };
        let mut chunk = vec![0.0f32; frames * usize::from(self.channels)];
        fill.fill(&mut chunk);
        self.shared
            .captured
            .lock()
            .expect("null output mutex")
            .extend_from_slice(&chunk);
        frames
    }

    /// Every sample the virtual device has consumed so far, in order.
    #[must_use]
    pub fn captured(&self) -> Vec<f32> {
        self.shared
            .captured
            .lock()
            .expect("null output mutex")
            .clone()
    }

    /// How many whole frames have been captured.
    #[must_use]
    pub fn captured_frames(&self) -> usize {
        self.shared
            .captured
            .lock()
            .expect("null output mutex")
            .len()
            / usize::from(self.channels)
    }

    /// Discards the capture buffer.
    pub fn clear_captured(&self) {
        self.shared
            .captured
            .lock()
            .expect("null output mutex")
            .clear();
    }
}

#[cfg(test)]
mod tests {
    use super::{FillCallback, NullOutput, OutputBackend};

    struct Constant(f32);

    impl FillCallback for Constant {
        fn fill(&mut self, out: &mut [f32]) {
            out.fill(self.0);
        }
    }

    #[test]
    fn null_output_only_moves_when_drained() {
        let (mut backend, control) = NullOutput::new(48_000, 2);
        assert_eq!(control.drain_frames(10), 0, "not started yet");
        backend
            .start(Box::new(Constant(0.25)), Box::new(|_| {}))
            .expect("starts");
        assert!(control.is_running());
        assert_eq!(control.captured_frames(), 0, "nothing drains on its own");
        assert_eq!(control.drain_frames(10), 10);
        assert_eq!(control.captured_frames(), 10);
        assert!(control.captured().iter().all(|s| *s == 0.25));
        backend.stop();
        assert!(!control.is_running());
        assert_eq!(
            control.drain_frames(10),
            0,
            "stopped devices produce nothing"
        );
    }
}
