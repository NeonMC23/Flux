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
//! ```
//!
//! For playback-glitch diagnosis it also reports the engine's
//! callback-underrun count (fills that ran dry mid-track and had to
//! emit silence) — both periodically and at the end. That number
//! separates "the device was starved" from "the samples themselves
//! were wrong", which no amount of listening can do reliably.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use flux_audio::engine::{EngineEvent, EngineHandle};
use flux_core::TrackId;

fn main() -> std::process::ExitCode {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: play_file <audio-file (wav|flac|mp3|aac|ogg|oga|opus|m4a|m4b|mp4)>");
        return std::process::ExitCode::FAILURE;
    };
    let path = PathBuf::from(path);

    let engine = match EngineHandle::spawn_cpal_default() {
        Ok(engine) => engine,
        Err(err) => {
            eprintln!("could not open audio output: {err}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let config = engine.output_config();
    println!(
        "output: {} Hz, {} channel(s); playing {}",
        config.sample_rate,
        config.channels,
        path.display()
    );

    let id = TrackId::new(1);
    engine.load(id, path);
    engine.play();

    let mut last_report = Instant::now();
    loop {
        match engine.events().recv_timeout(Duration::from_millis(250)) {
            Ok(EngineEvent::Loaded { id, info }) => {
                println!(
                    "loaded track {id}: {} Hz, {} channel(s), duration {:?}",
                    info.sample_rate, info.channels, info.duration
                );
            }
            Ok(EngineEvent::TrackEnded { id }) => {
                println!("track {id} ended at position {:?}", engine.position());
                break;
            }
            Ok(EngineEvent::Error { id, error }) => {
                eprintln!("engine error (track {id:?}): {error}");
                eprintln!("callback underruns so far: {}", engine.underrun_count());
                return std::process::ExitCode::FAILURE;
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if last_report.elapsed() >= Duration::from_secs(5) {
                    println!(
                        "position {:?}, callback underruns {}",
                        engine.position(),
                        engine.underrun_count()
                    );
                    last_report = Instant::now();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    println!(
        "callback underruns (mid-track silence fills): {}",
        engine.underrun_count()
    );
    engine.shutdown();
    std::process::ExitCode::SUCCESS
}
