//! Real device output via CPAL. Compiled only under the crate's
//! non-default `cpal` feature; see the crate-level docs for the
//! feature rationale and platform prerequisites.
//!
//! Device selection prefers an `f32` stream (the engine's native
//! format) and falls back to the device default, converting to the
//! device's integer/float sample type in the callback via
//! [`cpal::FromSample`]. The callback itself only forwards to the
//! engine's lock-free [`FillCallback`]. The scratch buffer is allocated
//! once before the stream starts and is reused without resizing; data
//! exceeding one scratch window is filled in frame-aligned chunks.
//! Underruns degrade to silence — the callback never blocks, locks,
//! allocates, or performs I/O.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig};

use crate::engine::EngineError;
use crate::output::{FillCallback, OutputBackend, OutputConfig};

/// Frames handed to the engine per scratch window. Larger device buffers
/// are processed as multiple frame-aligned chunks without allocating.
const CALLBACK_CHUNK_FRAMES: usize = 4_096;

/// An [`OutputBackend`] playing through a CPAL output device.
pub struct CpalOutput {
    device: cpal::Device,
    supported: SupportedStreamConfig,
    stream: Option<cpal::Stream>,
}

impl CpalOutput {
    /// Opens the host's default output device and selects a stream
    /// configuration (preferring `f32`; see module docs). The stream
    /// itself is created in [`OutputBackend::start`].
    ///
    /// # Errors
    ///
    /// [`EngineError::Output`] when no default output device exists or
    /// the device offers no usable configuration.
    pub fn open_default() -> Result<Self, EngineError> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| {
            EngineError::Output("no default audio output device is available".to_owned())
        })?;
        let supported = select_config(&device)?;
        Ok(Self {
            device,
            supported,
            stream: None,
        })
    }

    fn build<T>(
        &mut self,
        stream_config: StreamConfig,
        mut fill: Box<dyn FillCallback>,
        mut on_error: Box<dyn FnMut(String) + Send>,
    ) -> Result<(), EngineError>
    where
        T: SizedSample + cpal::FromSample<f32>,
    {
        let channels = usize::from(stream_config.channels).max(1);
        let mut scratch = vec![0.0f32; CALLBACK_CHUNK_FRAMES * channels];
        let stream = self
            .device
            .build_output_stream::<T, _, _>(
                stream_config,
                move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                    fill_chunked(data, &mut scratch, fill.as_mut());
                },
                move |err: cpal::Error| on_error(format!("output stream error: {err}")),
                None,
            )
            .map_err(|err| EngineError::Output(format!("could not build output stream: {err}")))?;
        stream
            .play()
            .map_err(|err| EngineError::Output(format!("could not start output stream: {err}")))?;
        self.stream = Some(stream);
        Ok(())
    }
}

fn fill_chunked<T>(data: &mut [T], scratch: &mut [f32], fill: &mut dyn FillCallback)
where
    T: SizedSample + cpal::FromSample<f32>,
{
    if scratch.is_empty() {
        for sample in data {
            *sample = T::from_sample(0.0);
        }
        return;
    }
    for chunk in data.chunks_mut(scratch.len()) {
        let buffer = &mut scratch[..chunk.len()];
        fill.fill(buffer);
        for (out, sample) in chunk.iter_mut().zip(buffer.iter()) {
            *out = T::from_sample(*sample);
        }
    }
}

impl OutputBackend for CpalOutput {
    fn config(&self) -> OutputConfig {
        OutputConfig {
            sample_rate: self.supported.sample_rate(),
            channels: self.supported.channels(),
        }
    }

    fn start(
        &mut self,
        fill: Box<dyn FillCallback>,
        on_error: Box<dyn FnMut(String) + Send>,
    ) -> Result<(), EngineError> {
        let stream_config = self.supported.config();
        match self.supported.sample_format() {
            SampleFormat::F32 => self.build::<f32>(stream_config, fill, on_error),
            SampleFormat::F64 => self.build::<f64>(stream_config, fill, on_error),
            SampleFormat::I8 => self.build::<i8>(stream_config, fill, on_error),
            SampleFormat::I16 => self.build::<i16>(stream_config, fill, on_error),
            SampleFormat::I24 => self.build::<cpal::I24>(stream_config, fill, on_error),
            SampleFormat::I32 => self.build::<i32>(stream_config, fill, on_error),
            SampleFormat::I64 => self.build::<i64>(stream_config, fill, on_error),
            SampleFormat::U8 => self.build::<u8>(stream_config, fill, on_error),
            SampleFormat::U16 => self.build::<u16>(stream_config, fill, on_error),
            SampleFormat::U24 => self.build::<cpal::U24>(stream_config, fill, on_error),
            SampleFormat::U32 => self.build::<u32>(stream_config, fill, on_error),
            SampleFormat::U64 => self.build::<u64>(stream_config, fill, on_error),
            other => Err(EngineError::Output(format!(
                "unsupported output device sample format: {other:?}"
            ))),
        }
    }

    fn stop(&mut self) {
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            drop(stream);
        }
    }
}

/// Picks the stream configuration: the device default if it is
/// already `f32`; otherwise any supported `f32` configuration
/// (preferring 48 kHz); otherwise the device default as-is.
fn select_config(device: &cpal::Device) -> Result<SupportedStreamConfig, EngineError> {
    let default = device
        .default_output_config()
        .map_err(|err| EngineError::Output(format!("no default output configuration: {err}")))?;
    if default.sample_format() == SampleFormat::F32 {
        return Ok(default);
    }
    if let Ok(ranges) = device.supported_output_configs() {
        let mut fallback = None;
        for range in ranges {
            if range.sample_format() != SampleFormat::F32 {
                continue;
            }
            if (range.min_sample_rate()..=range.max_sample_rate()).contains(&48_000) {
                return Ok(range.with_sample_rate(48_000));
            }
            fallback = Some(range.with_max_sample_rate());
        }
        if let Some(config) = fallback {
            return Ok(config);
        }
    }
    Ok(default)
}

#[cfg(test)]
mod tests {
    use super::fill_chunked;
    use crate::output::FillCallback;

    struct Sequence {
        next: f32,
        calls: usize,
        maximum_len: usize,
        channels: usize,
        frame_aligned: bool,
    }

    impl FillCallback for Sequence {
        fn fill(&mut self, out: &mut [f32]) {
            self.calls += 1;
            self.maximum_len = self.maximum_len.max(out.len());
            self.frame_aligned &= out.len().is_multiple_of(self.channels);
            for sample in out {
                *sample = self.next;
                self.next += 1.0;
            }
        }
    }

    #[test]
    fn cpal_chunking_reuses_fixed_scratch_without_frame_splits() {
        let channels = 2;
        let scratch_samples = 4 * channels;
        let mut scratch = vec![0.0f32; scratch_samples];
        let original_len = scratch.len();
        let mut data = vec![0.0f32; 11 * channels];
        let mut fill = Sequence {
            next: 1.0,
            calls: 0,
            maximum_len: 0,
            channels,
            frame_aligned: true,
        };
        fill_chunked(&mut data, &mut scratch, &mut fill);
        assert_eq!(fill.calls, 3);
        assert_eq!(fill.maximum_len, scratch_samples);
        assert_eq!(scratch.len(), original_len, "scratch must not resize");
        assert!(fill.frame_aligned);
        assert!(data.iter().all(|sample| *sample > 0.0));
        assert_eq!(
            data[data.len() - 1],
            22.0,
            "all requested samples are delivered once in order"
        );
    }
}
