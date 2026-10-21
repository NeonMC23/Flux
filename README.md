# Flux

Flux is a planned native, cross-platform desktop music player written in
Rust. It is the successor to the Flow web prototype, keeping Flow's product
ideas and dark visual identity while replacing the implementation with a
native Rust architecture: an Iced graphical application and a dedicated
terminal application sharing the same core logic.

**Status: Milestone M3 — real local audio playback engine.** This
repository contains the Cargo workspace, a backend-independent domain
model, a working local-library layer (recursive scanning with embedded
metadata reading, an in-memory collection, and a versioned JSON index
with stable track IDs), and a threaded playback engine that decodes WAV
and FLAC files and plays them through CPAL behind an explicit Cargo
feature. The GUI and TUI are **not** wired to the library or the engine
yet, so the applications themselves still do not browse or play music —
that integration is a later milestone. Real-device playback has not been
hardware-verified in the development sandbox or CI (neither has an audio
device); see the manual verification checklist below.

## What exists at M3

| Crate | Kind | Responsibility |
|---|---|---|
| `flux-core` | library | Backend-independent domain model: tracks, IDs, sources, play queue, repeat/playback states, and the shared error type. No UI, filesystem, database, audio-device, or network implementation. |
| `flux-library` | library | Local library: recursive folder scanning (tags via lofty, filename fallback, symlinks never followed) into an in-memory, insertion-ordered collection with path deduplication and search, plus save/load of a versioned JSON index with stable IDs. |
| `flux-audio` | library | Playback, in two strictly separate layers: `Player`, the pure playback-control state machine (play/pause/stop/skips/repeat) over the queue and the single authority for playback decisions; and a threaded engine (`engine` module) that decodes WAV (hound) and FLAC (claxon) on a worker thread, converts channel layout and sample rate, and feeds an output backend through a lock-free ring buffer. Backends: a deterministic device-free `NullOutput` (always) and real device output via CPAL behind the non-default `cpal` feature. |
| `flux-providers` | library | Data-only provider descriptors and capability vocabulary for future legally supported streaming integrations. No traits, networking, or integrations yet. |
| `flux-platform` | library | OS integration: per-user data/config directory resolution for Linux/XDG, macOS, and Windows (standard library only). |
| `flux-gui` | binary `flux` | Native [Iced](https://iced.rs) application. Currently shows a static startup view only. |
| `flux-tui` | binary `flux-tui` | Terminal application. Currently prints a startup message and exits. |

Dependency direction: `flux-gui → flux-core + iced`, `flux-tui → flux-core`,
`flux-library → flux-core + lofty + walkdir + serde/serde_json`, and
`flux-audio → flux-core + claxon + hound + ringbuf` (plus optional `cpal`
under its `cpal` feature). `flux-core`, `flux-providers`, and
`flux-platform` have no dependencies. `flux-library` never chooses OS
directories itself — the persistence path is injected by the caller
(typically resolved via `flux-platform`), and scanning never prunes
tracks whose files have disappeared. The playback engine never scans or
persists the library: its caller hands it a `TrackId` and a file path.

## Audio: playable vs. indexed formats

Being **indexed** by the library scanner does not make a file **playable**.
The scanner recognizes 16 extensions; the engine can currently decode
exactly two of them:

| | Extensions |
|---|---|
| **Playable now (M3)** | `flac`, `wav` |
| **Indexed, not yet playable** | `aac`, `aif`, `aiff`, `ape`, `m4a`, `m4b`, `mp3`, `mp4`, `mpc`, `oga`, `ogg`, `opus`, `spx`, `wv` |

Asking the engine to load a not-yet-playable file fails immediately with a
typed `UnsupportedFormat` error (the file is not even opened); malformed
WAV/FLAC data and I/O failures are likewise typed errors, surfaced as
engine events rather than panics or silent skips.

Engine summary (details in the `flux-audio` crate documentation):
commands (load/play/pause/stop/volume) never block the caller; decoding
happens on a worker thread; the audio callback only pops a lock-free ring
buffer and emits silence on underrun; generation counters ensure rapid
track changes never leak stale samples or end-of-track events;
`TrackEnded` fires only after the decoded output has fully drained
through the device. Files are channel-remapped and linear-resampled to
the device format when rates differ.

### The `cpal` feature

Real device output is behind the **non-default** `cpal` Cargo feature of
`flux-audio`. Default builds compile anywhere and exercise the engine
headlessly through `NullOutput`; nothing in a default build claims audio
is available. With the feature enabled, CPAL picks the default output
device, preferring an `f32` stream and converting to the device's integer
formats when needed.

Platform prerequisites for `--features cpal`:

- **Linux:** ALSA development files (Debian/Ubuntu: `libasound2-dev`,
  which provides `alsa.pc`). Without them, the build fails in `alsa-sys`
  with `Package 'alsa' ... not found` — a missing prerequisite, not a
  code error.
- **Windows / macOS:** no extra system packages (WASAPI / CoreAudio).

### Manual hardware verification checklist

Compilation is not playback. To verify audible playback on a real
machine with working speakers or headphones:

1. Install the platform prerequisites above (Linux: `libasound2-dev`).
2. Prepare a known-good FLAC and a known-good WAV (ideally short, e.g.
   10–30 s, including one 44.1 kHz file if your device defaults to
   48 kHz, to exercise resampling).
3. Run the development example against the FLAC:
   `cargo run -p flux-audio --features cpal --example play_file -- /path/to/test.flac`
4. Expect: it prints the output device rate/channels and a `loaded
   track` line with the file's sample rate, channels, and duration;
   the full file is **audible** at the correct pitch and speed, without
   dropouts or distortion; it finishes with `track 1 ended at position …`
   close to the file duration; exit code 0.
5. Repeat step 3–4 with the WAV, and with the 44.1 kHz file on a
   48 kHz device (pitch must stay normal — that is the resampler path).
6. Negative checks: passing an `.mp3` must print an unsupported-format
   engine error, exit with failure, and produce no sound; a nonexistent
   path must print an I/O error; disabling all output devices must print
   `could not open audio output: … no default audio output device …`.
7. If anything deviates (silence, wrong speed, truncation, hang),
   record the OS, device, file properties, and full console output —
   do not treat the milestone as hardware-verified.

## What does not exist yet

GUI/TUI wiring to the library and playback engine, seeking, gapless
playback, crossfade, EQ, visualizers, folder watching, artwork,
playlists, streaming providers, accounts, lyrics, installers, and
updaters are all future milestones. The interfaces do not browse or
control the library yet. Nothing in this repository makes network
requests, collects telemetry, or requires an account.

## Building

The Rust toolchain is pinned in `rust-toolchain.toml` (currently Rust
1.99.1, with `rustfmt` and `clippy`). With [rustup](https://rustup.rs)
installed, the correct toolchain is installed automatically on first use.
The workspace declares `rust-version = 1.88`, the minimum required by Iced
0.14; the pinned toolchain is what CI and contributors are expected to use.

On Linux, the GUI also needs system development libraries for fontconfig,
libxkbcommon, and Wayland, plus `pkg-config`; the CPAL audio feature
additionally needs ALSA development files. On Debian/Ubuntu:

```sh
sudo apt-get install pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev libasound2-dev
```

Common commands:

```sh
cargo build --workspace          # build everything (device-independent)
cargo test --workspace           # run tests (engine tests are headless)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check licenses        # dependency license policy (see deny.toml)
cargo run -p flux-gui            # GUI startup view (binary name: flux)
cargo run -p flux-tui            # TUI startup message (binary name: flux-tui)

# Real-device audio (requires the platform prerequisites above):
cargo check -p flux-audio --features cpal --all-targets
cargo run -p flux-audio --features cpal --example play_file -- /path/to/file.flac
```

## Supported targets and CI

Flux targets Linux, Windows 10/11, and macOS on x86_64 and ARM64. The CI
workflow in `.github/workflows/ci.yml` runs formatting, Clippy, dependency
license checks, tests, and builds. Tests and builds run natively on Linux
x86_64 and ARM64, Windows x86_64 and ARM64, and macOS ARM64. macOS x86_64
has no standard GitHub-hosted runner, so CI performs a compile-only
cross-check (`cargo check --target x86_64-apple-darwin`) from an ARM64
macOS runner; it is not executed or tested there. The `cpal` feature is
compile-checked (and Clippy-linted on Linux) on every platform; CI
runners have no usable audio device, so this never constitutes a
playback test.

## License

Flux's own code is licensed under the MIT License (see `LICENSE`).

Third-party dependency licenses are governed by `deny.toml`, which allows
only permissive licenses (MIT, Apache-2.0, Zlib, BSD, ISC, BSL-1.0,
CC0-1.0, Unlicense, Unicode-3.0) and denies copyleft obligations. Iced
itself is MIT-licensed; the audio stack (cpal, claxon, hound, ringbuf)
is Apache-2.0 / MIT. Run `cargo deny check licenses` to reproduce the
check against `Cargo.lock`.

No assets from the Flow prototype (recordings, artwork, photographs, or
fonts) are included in this repository, and the Flow repository itself is
unchanged.
