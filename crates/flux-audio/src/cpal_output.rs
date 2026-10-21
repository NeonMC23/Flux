//! Real device output via CPAL. Compiled only under the crate's
//! non-default `cpal` feature; see the crate-level docs for the
//! feature rationale and platform prerequisites.
//!
//! Device selection prefers an `f32` stream (the engine's native
//! format) and falls back to the device default, converting to the
//! device's integer/float sample type in the callback via
//! [`cpal::FromSample`]. The callback itself only forwards to the
//! engine's lock-free [`FillCallback`]; the one allocation (a scratch
//! buffer) grows to the device's buffer size during warm-up and is
//! then reused. Underruns degrade to silence — the callback never
//! blocks, locks, or performs I/O.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SizedSample, StreamConfig, SupportedStreamConfig};

use crate::engine::EngineError;
use crate::output::{FillCallback, OutputBackend, OutputConfig};

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
        let mut scratch: Vec<f32> = Vec::new();
        let stream = self
            .device
            .build_output_stream::<T, _, _>(
                stream_config,
                move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
                    // Grows once to the device buffer size, then reuses.
                    if scratch.len() < data.len() {
                        scratch.resize(data.len(), 0.0);
                    }
                    let buffer = &mut scratch[..data.len()];
                    fill.fill(buffer);
                    for (out, sample) in data.iter_mut().zip(buffer.iter()) {
                        *out = T::from_sample(*sample);
                    }
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
