//! Development example: plays one local audio file through the
//! default CPAL output device and prints engine events until the track
//! ends. Any file `flux_audio::decode::open_decoder` supports works
//! (WAV, FLAC, MP3, AAC, Ogg Vorbis, Opus, M4A/MP4-AAC).
//!
//! This is the manual hardware-verification entry point; it is not
//! part of either application. It only builds with the `cpal` feature
//! enabled:
//!
//! ```sh
//! cargo run -p flux-audio --features cpal --example play_file -- /path/to/file.flac
//! cargo run -p flux-audio --features cpal --example play_file -- /path/to/file.flac alsa:hw:1,0
//! ```
//!
//! The optional second argument selects an enumerated device ID; otherwise the
//! current system default is used.

use std::path::PathBuf;
use std::time::Duration;

use flux_audio::engine::{EngineEvent, EngineHandle};
use flux_core::TrackId;

fn main() -> std::process::ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(path) = args.next() else {
        eprintln!(
            "usage: play_file <audio-file (wav|flac|mp3|aac|ogg|oga|opus|m4a|m4b|mp4)> [device-id]"
        );
        return std::process::ExitCode::FAILURE;
    };
    let path = PathBuf::from(path);
    let requested_device = args
        .next()
        .map(|value| value.to_string_lossy().into_owned());

    match flux_audio::cpal_output::list_output_devices() {
        Ok(devices) => {
            println!("detected {} output device(s):", devices.len());
            for device in &devices {
                println!(
                    "  {}{}{}",
                    device.name,
                    device
                        .id
                        .as_deref()
                        .map_or_else(String::new, |id| format!(" [{id}]")),
                    if device.is_default { " · default" } else { "" }
                );
            }
        }
        Err(error) => println!("device enumeration failed: {error}"),
    }

    let (engine, active) = match EngineHandle::spawn_cpal_device(requested_device.as_deref()) {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("could not open audio output: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let config = engine.output_config();
    println!(
        "output: {} [{}] · {} Hz, {} channel(s); playing {}",
        active.name,
        active.id.as_deref().unwrap_or("no stable id"),
        config.sample_rate,
        config.channels,
        path.display()
    );

    let id = TrackId::new(1);
    engine.load(id, path);
    engine.play();

    loop {
        match engine.events().recv_timeout(Duration::from_millis(250)) {
            Ok(EngineEvent::Loaded { id, info, .. }) => {
                println!(
                    "loaded track {id}: {} Hz, {} channel(s), duration {:?}",
                    info.sample_rate, info.channels, info.duration
                );
            }
            Ok(EngineEvent::TrackEnded { id, .. }) => {
                println!("track {id} ended at position {:?}", engine.position());
                break;
            }
            Ok(EngineEvent::Error { id, error, .. }) => {
                eprintln!("engine error (track {id:?}): {error}");
                return std::process::ExitCode::FAILURE;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    engine.shutdown();
    std::process::ExitCode::SUCCESS
}
