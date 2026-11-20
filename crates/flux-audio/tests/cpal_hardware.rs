//! Opt-in hardware smoke test for the CPAL backend.
//!
//! This test needs a real output device and is ignored by ordinary CI. Run it
//! explicitly with a working audio server:
//!
//! ```sh
//! cargo test -p flux-audio --features cpal --test cpal_hardware -- --ignored --nocapture
//! ```
//!
//! It proves that a decoded WAV is consumed by a real output stream. It does
//! not prove that a human heard sound or that the physical route is correct.

#![cfg(feature = "cpal")]

use std::f32::consts::TAU;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use flux_audio::engine::{EngineEvent, EngineHandle};
use flux_core::TrackId;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_wav() -> PathBuf {
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "flux-cpal-hardware-{}-{unique}.wav",
        std::process::id()
    ))
}

fn write_tone(path: &PathBuf) -> Result<(), String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: 44_100,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).map_err(|error| error.to_string())?;
    let frames = 44_100 * 2;
    for frame in 0..frames {
        let phase = TAU * 440.0 * frame as f32 / 44_100.0;
        let sample = (phase.sin() * 4_000.0) as i16;
        writer
            .write_sample(sample)
            .map_err(|error| error.to_string())?;
        writer
            .write_sample(sample)
            .map_err(|error| error.to_string())?;
    }
    writer.finalize().map_err(|error| error.to_string())
}

#[test]
#[ignore = "requires a real audio output device; run with --ignored"]
fn decoded_wav_reaches_a_real_output_stream() {
    let path = temp_wav();
    write_tone(&path).expect("write WAV fixture");
    let (engine, device) =
        EngineHandle::spawn_cpal_device(None).expect("default CPAL output stream");
    let config = engine.output_config();
    assert!(config.sample_rate > 0);
    assert!(config.channels > 0);
    engine.set_volume(0.05);
    let id = TrackId::new(1);
    let session = engine.load(id, path.clone());

    let loaded = loop {
        match engine
            .events()
            .recv_timeout(Duration::from_secs(10))
            .expect("load event")
        {
            EngineEvent::Loaded {
                session: loaded_session,
                info,
                ..
            } if loaded_session == session => break info,
            EngineEvent::Error {
                session: failed,
                error,
                ..
            } if failed == Some(session) => {
                panic!("load failed: {error}")
            }
            _ => {}
        }
    };
    assert_eq!(loaded.sample_rate, 44_100);
    assert_eq!(loaded.channels, 2);

    engine.play();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut diagnostics = engine.diagnostics();
    while Instant::now() < deadline {
        diagnostics = engine.diagnostics();
        if diagnostics.output_failed
            || diagnostics.output_errors > 0
            || diagnostics.decoder_errors > 0
        {
            break;
        }
        if diagnostics.callbacks > 0 && diagnostics.played_frames >= u64::from(config.sample_rate) {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    engine.shutdown();
    let _ = std::fs::remove_file(&path);

    assert!(
        diagnostics.callbacks > 0,
        "the real stream delivered no callbacks: {diagnostics:?}"
    );
    assert!(
        diagnostics.delivered_frames > 0,
        "decoded audio never reached the output ring: {diagnostics:?}"
    );
    assert!(
        diagnostics.played_frames >= u64::from(config.sample_rate),
        "the device did not consume a sustained second of decoded audio: {diagnostics:?}"
    );
    assert_eq!(diagnostics.decoder_errors, 0, "{diagnostics:?}");
    assert_eq!(diagnostics.output_errors, 0, "{diagnostics:?}");
    assert!(!diagnostics.output_failed, "{diagnostics:?}");
    assert!(device.is_default || device.id.is_some(), "{device:?}");
}

#[test]
#[ignore = "requires a real audio output device; run with --ignored"]
fn explicit_output_device_id_round_trips() {
    let devices = flux_audio::cpal_output::list_output_devices().expect("enumerate devices");
    let target = devices
        .iter()
        .find(|device| {
            device.id.is_some()
                && device.can_open
                && !device.is_default
                && !device.name.to_ascii_lowercase().contains("discard")
        })
        .or_else(|| {
            devices
                .iter()
                .find(|device| device.id.is_some() && device.can_open)
        })
        .expect("at least one openable device");
    let id = target.id.as_deref().expect("stable id");
    let (engine, opened) =
        EngineHandle::spawn_cpal_device(Some(id)).expect("explicit device opens");
    assert_eq!(opened.id.as_deref(), Some(id));
    assert!(engine.output_config().sample_rate > 0);
    engine.shutdown();
}
