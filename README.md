# Flux

Flux is a native, cross-platform desktop music player written in Rust —
the successor to the Flow web prototype, keeping Flow's product ideas
while replacing the implementation with a native architecture: an Iced
graphical application and a dedicated terminal application sharing the
same core logic, library, and playback engine.

**Status: Milestone M5 delivered; M6 reliability and queue work in
progress.**
Both applications work end to end: scan a local music folder,
browse/search the library, edit a persistent user-managed play queue,
and play WAV, FLAC, MP3, AAC (ADTS streams and MP4/M4A files), Ogg
Vorbis, and Opus with play/pause/stop, next/previous, volume, repeat
modes, and automatic advancement, all coordinated by one shared
controller. The GUI also has a real-time oscilloscope in the persistent
player bar, driven by the same post-gain samples sent to the output
device and reproducing Flow's Web Audio waveform processing. The
library index (with stable track IDs) and the per-library queue persist
between runs. The project owner
has confirmed that real-device playback works on Nobara Linux; that is
one machine and setup, not a claim that every codec or device has been
independently verified — the development sandbox and CI have no audio
device, so see the verification checklist below and judge accordingly.

## Crates

| Crate | Kind | Responsibility |
|---|---|---|
| `flux-core` | library | Backend-independent domain model: tracks, IDs, sources, the play queue with stable per-entry identities (duplicates allowed), repeat/playback states, and the shared error type. No UI, filesystem, database, audio-device, or network implementation. |
| `flux-library` | library | Local library: recursive folder scanning (tags via lofty, filename fallback, symlinks never followed) into an in-memory, insertion-ordered collection with path deduplication and search, plus save/load of a versioned JSON index with stable IDs and a separate atomic, versioned queue-file store. Scanning never prunes missing files. |
| `flux-audio` | library | Playback, in two strictly separate layers: `Player`, the pure playback-control state machine and single authority for playback decisions; and a threaded engine that decodes WAV (hound), FLAC (claxon), MP3 (rmp3/minimp3), AAC in ADTS streams and MP4/M4A files (rusty_aac with the mp4 demuxer), and Ogg Vorbis and Opus (lewton and opus-pure over the ogg demuxer) on a worker thread and feeds an output backend (device-free `NullOutput`, or CPAL behind the non-default `cpal` feature) through a lock-free ring buffer. |
| `flux-app` | library | The shared application controller: owns the library, `Player`, engine, volume, queue persistence, and index path; syncs the engine to the player's decisions; runs scans on a background thread; turns unsupported/missing/corrupt files into clear user-facing errors. Both front-ends drive this one controller and its queue operations. |
| `flux-providers` | library | Data-only provider descriptors and capability vocabulary for future legally supported streaming integrations. Nothing networked exists yet. |
| `flux-platform` | library | OS integration: per-user data/config directory resolution for Linux/XDG, macOS, and Windows (standard library only). |
| `flux-gui` | binary `flux` | Native [Iced](https://iced.rs) application: folder scanning, searchable track list, interactive queue panel (play, add, play-next, remove, reorder), transport controls, volume, repeat, status/errors, and a real-time oscilloscope fed by actual post-gain output samples. |
| `flux-tui` | binary `flux-tui` | Interactive terminal application (crossterm): track list with search, queue add/play-next/remove/reorder keys, full keyboard control, same controller semantics as the GUI. |

Dependency direction: `flux-gui → flux-app + flux-core + iced`,
`flux-tui → flux-app + flux-core + crossterm`,
`flux-app → flux-core + flux-library + flux-audio + flux-platform`,
`flux-library → flux-core + lofty + walkdir + serde/serde_json`, and
`flux-audio → flux-core + claxon + hound + rmp3 + lewton + ogg +
opus-pure + rusty_aac + mp4 + ringbuf` (plus optional `cpal`). `flux-core`, `flux-providers`, and `flux-platform` have no
dependencies. The engine never scans or persists the library; the
catalogue store and queue store are separate files, and `Player` remains
the only playback-decision authority — the controller just keeps the
engine in sync with it. Queue files are namespaced by active library,
never by audio tags or catalogue records.

## Using Flux

Build the applications **with** the `cpal` feature for real sound
(default builds are device-independent and will say audio is
unavailable instead of pretending):

```sh
cargo run -p flux-gui --features cpal            # binary name: flux
cargo run -p flux-tui --features cpal -- ~/Music # optional folder to scan
```

- **GUI:** type a music folder path and choose *Scan folder*
  (*Refresh* re-scans this session's folders). Click a track to play
  it; use the transport row for pause/stop/skip/repeat/volume; the
  search box filters by title, artist, or album. The queue panel shows
  the whole queue with the current entry highlighted. Drag a row by its
  grip to reorder it; the pink insertion line shows the drop position.
  `Ctrl+↑`/`Ctrl+↓` moves the current entry from the keyboard, `×`
  removes an entry, and clicking a row plays it. `+` on a library row
  appends it to the queue; `→` inserts it after the current entry.
- **TUI:** `↑/↓` or `j/k` move, `Enter` plays, `Space` pauses, `s`
  stops, `n/p` skip, `+/-` volume, `R` repeat, `o` scan a folder
  (type the path), `r` refresh, `/` search, `q` quit. For the queue,
  `a` appends the selected library track, `x` plays it next,
  `Delete` removes the current entry, and `Ctrl+↑`/`Ctrl+↓` reorder
  the current entry. The terminal is always restored on exit,
  including on errors.
- The library index lives at `library-index.json` in the per-user data
  directory (e.g. `~/.local/share/flux/` on Linux,
  `%LOCALAPPDATA%\flux\` on Windows, `~/Library/Application
  Support/flux/` on macOS) and is saved after every scan. A legacy
  queue is stored beside it as `<index>.queue.json`; a portable queue
  lives at `<library>/.flux/queue/queue.json`. Queue files are written
  atomically, survive restarts, and stay isolated per library. A
  malformed queue file is reported and preserved (queue writes are
  disabled for that session) rather than replaced with an empty queue.
- A queue that has never been structurally edited follows newly scanned
  tracks automatically; adding, removing, or reordering an entry turns
  it into a fully user-managed queue that scans no longer modify.
  Entries whose tracks disappear from the library are retained and
  shown as missing until explicitly removed.
- The oscilloscope is reproduced from Flow's
  `src/javascript/effect.js` Web Audio time-domain analyser. Flux uses
  a bounded lock-free tap of the real post-gain output and the same
  4096-sample, 8-bit processing: RMS smoothing (`0.85/0.15`), a
  `0.45 / RMS` gain target clamped to `1.0..=2.6`, attack/release
  interpolation (`0.25/0.05`), a 48 px rounded canvas, dark background,
  glow, and a `#ff66cc` 1 px line. Flux taps device-rate
  post-resampling output, so sample values are not bit-for-bit identical
  to a browser's Web Audio graph. The waveform algorithm, gain envelope,
  colors, dimensions, and playback-state behavior mirror Flow; native
  rasterization and pixel-level antialiasing necessarily differ.

### Playable vs. indexed formats

Being **indexed** does not mean **playable** — neither app pretends
otherwise: unplayable tracks are marked `[indexed — not playable yet]`
and trying to play one is a clear error, not silence.

| | Extensions |
|---|---|
| **Playable** | `aac`, `flac`, `m4a`, `m4b`, `mp3`, `mp4`, `oga`, `ogg`, `opus`, `wav` |
| **Indexed, not yet playable** | `aif`, `aiff`, `ape`, `mpc`, `spx`, `wv` |

Playability is decided by content, not extension alone: `.m4a`/`.mp4`
files must contain an AAC track (ALAC does not play), `.aac` must be an
ADTS stream, and Opus support covers mono/stereo (channel mapping
family 0), not surround layouts. The `flux_audio::decode` module docs
carry the exact container/codec matrix.

Files that disappear or turn out corrupt produce an error message and
a clean stop; they are never silently pruned from the library.

### Audio notes

Device output is via CPAL behind each application's non-default
`cpal` feature (pass-through to `flux-audio/cpal`). CPAL picks the
default output device, preferring `f32`; files are channel-remapped
and linear-resampled to the device format when needed. On Linux the
feature requires ALSA development files (`libasound2-dev`); Windows
and macOS need no extra packages. If no device is available (or the
build lacks the feature), the apps still run for browsing/scanning
and say exactly why playback is unavailable.

One build prerequisite joined with MP3 support: the minimp3 decoder
is C (bundled by the `rmp3` crate) and is compiled at build time, so
building `flux-audio` needs the platform C toolchain (gcc/clang on
Linux/macOS, MSVC on Windows) that a normal Rust development setup
already has. All other decoders are pure Rust. The C code runs only
on the engine's worker thread, never in the audio callback, and is
driven through a thin checked FFI wrapper of our own (see
`crates/flux-audio/src/decode.rs` for why).

**Manual hardware verification** (compilation is not playback): on a
machine with working audio, build either app with `--features cpal`,
scan a folder containing one known-good file per format — FLAC, WAV,
MP3, M4A (iTunes/ffmpeg AAC), Ogg Vorbis, Opus, and an ADTS `.aac` if
you have one — ideally including 44.1 kHz files on a 48 kHz device and
a stereo Opus at 48 kHz. Play each fully: expect correct pitch/speed,
no dropouts, position reaching the track duration, and automatic
advancement at end of track. Also verify an ALAC `.m4a` and an `.aif`
produce the unsupported-format error with no sound, that a renamed
file (e.g. an MP3 named `.ogg`) errors instead of crashing, and that
with all output devices disabled the app reports the missing device.
A minimal no-UI check is also available:
`cargo run -p flux-audio --features cpal --example play_file -- /path/to/file.mp3`.

## What does not exist yet

Seeking, playlists, artwork, gapless playback, crossfade, EQ,
frequency/spectrum visualizers (the current oscilloscope is a
time-domain waveform), still-more formats/codecs (AIFF, APE, WavPack,
ALAC, surround Opus), folder watching, streaming providers, accounts,
lyrics, installers, and updaters are future milestones. Nothing in
this repository makes network requests, collects telemetry, or
requires an account.

## Building

The Rust toolchain is pinned in `rust-toolchain.toml` (Rust 1.99.1,
with `rustfmt` and `clippy`); with [rustup](https://rustup.rs) it is
installed automatically on first use. The workspace declares
`rust-version = 1.89`. On Linux, the GUI additionally needs fontconfig,
libxkbcommon, and Wayland development libraries, plus ALSA
development files for the `cpal` feature (Debian/Ubuntu:
`pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev
libasound2-dev`).

```sh
cargo build --workspace            # device-independent default build
cargo test --workspace             # tests (engine/controller: headless)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check licenses          # license policy (see deny.toml)
cargo run -p flux-gui --features cpal
cargo run -p flux-tui --features cpal -- ~/Music
```

## Supported targets and CI

Flux targets Linux, Windows 10/11, and macOS on x86_64 and ARM64. CI
(`.github/workflows/ci.yml`) runs formatting, Clippy, license checks,
tests, and builds natively on Linux x86_64/ARM64, Windows x86_64/ARM64,
and macOS ARM64; macOS x86_64 gets a compile-only cross-check (no
standard GitHub runner exists for it). The `cpal` feature is
compile-checked (and Clippy-linted on Linux) on every platform — CI
runners have no usable audio device, so that never constitutes a
playback test.

## License

Flux's own code is MIT-licensed (see `LICENSE`). Dependencies are
governed by `deny.toml`, which allows only permissive licenses (MIT,
Apache-2.0, Zlib, BSD, ISC, BSL-1.0, CC0-1.0, Unlicense,
Unicode-3.0); the stack — Iced, CPAL, the format decoders (claxon,
hound, rmp3, lewton, ogg, opus-pure, rusty_aac, mp4), ringbuf, lofty,
crossterm — stays within that permissive set (MIT, Apache-2.0, and,
among the decoders, BSD-3-Clause and CC0-1.0). Verify with
`cargo deny check licenses`.

No assets from the Flow prototype (recordings, artwork, photographs,
or fonts) are included in this repository, and the Flow repository
itself is unchanged.
