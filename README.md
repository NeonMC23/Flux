# Flux

Flux is a native, cross-platform desktop music player written in Rust —
the successor to the Flow web prototype, keeping Flow's product ideas
while replacing the implementation with a native architecture: an Iced
graphical application and a dedicated terminal application sharing the
same core logic, library, and playback engine.

**Status: local library-management foundation delivered.**
Both applications work end to end: scan or import a local music library
(the GUI includes a review-first Import Manager with native pickers,
drag-and-drop, duplicate decisions, copy/reference storage, and editable
metadata), browse/search it, edit Flux-owned track metadata and stable artists,
reconcile missing/moved files, edit a persistent user-managed play queue,
and play WAV, FLAC, MP3, AAC (ADTS streams and MP4/M4A files), Ogg
Vorbis, and Opus with play/pause/stop, next/previous, real seeking,
volume, repeat modes, and automatic advancement, all coordinated by one
shared controller. The GUI also has a real-time oscilloscope in the persistent
player bar, driven by the same post-gain samples sent to the output
device and reproducing Flow's Web Audio waveform processing. Portable
library catalogues (with stable track and artist IDs) and per-library
queues persist inside the library's `.flux/` folder; the app-data
version-1 index remains a compatibility backend. The project owner
has confirmed that real-device playback works on Nobara Linux; that is
one machine and setup, not a claim that every codec or device has been
independently verified — the development sandbox and CI have no audio
device, so see the verification checklist below and judge accordingly.

## Canonical storage foundation (in progress)

The library crate now contains a canonical, self-contained storage engine and an
isolated migration planner (`docs/canonical-library.md`). It is **not yet the
GUI/TUI runtime backend**: the Controller still opens the legacy index or the
schema-v3 portable catalogue, and existing import metadata still reads audio
tags. The canonical format owns relative managed audio, stable artist/album/
track IDs, per-entity `.flux` records, managed lyrics/cover primitives,
playlists/ratings, transactional publication and recovery. Do not treat the
migration as complete until the runtime integration listed in that document is
finished and tested.

## Crates

| Crate | Kind | Responsibility |
|---|---|---|
| `flux-core` | library | Backend-independent domain model: tracks, IDs, sources, the play queue with stable per-entry identities (duplicates allowed), repeat/playback states, and the shared error type. No UI, filesystem, database, audio-device, or network implementation. |
| `flux-library` | library | Local library: recursive folder scanning (tags via lofty, filename fallback, symlinks never followed) into an in-memory, insertion-ordered collection with path deduplication and search, plus the versioned portable `.flux` catalogue with stable track/artist IDs, sparse metadata overrides, file fingerprints, availability/relink maintenance, and a separate atomic queue store. The legacy app-data JSON index remains readable/writable for compatibility; scanning never prunes missing files. |
| `flux-audio` | library | Playback, in two strictly separate layers: `Player`, the pure playback-control state machine and single authority for playback decisions; and a threaded engine that decodes WAV (hound), FLAC (claxon), MP3 (rmp3/minimp3), AAC in ADTS streams and MP4/M4A files (rusty_aac with the mp4 demuxer), and Ogg Vorbis and Opus (lewton and opus-pure over the ogg demuxer) on a worker thread and feeds an output backend (device-free `NullOutput`, or CPAL behind the non-default `cpal` feature) through a lock-free ring buffer. |
| `flux-app` | library | The shared application controller: owns the library, `Player`, engine, volume, queue persistence, and backend; syncs the engine to playback decisions; runs scans in the background; provides the typed review-first import domain (discovery, metadata, duplicate decisions, copy/reference planning, cancellation, publication) plus portable-library metadata editing, stable artists, reconciliation, and relinking. Both front-ends use this one controller. |
| `flux-providers` | library | Data-only provider descriptors and capability vocabulary for future legally supported streaming integrations. Nothing networked exists yet. |
| `flux-platform` | library | OS integration: per-user data/config directory resolution and dependency-free native file/folder pickers for Linux/XDG, macOS, and Windows (standard library plus the platform's own dialog commands). |
| `flux-gui` | binary `flux` | Native [Iced](https://iced.rs) application: review-first Import Manager (native pickers, Flux Browser, drag-and-drop, editable metadata, duplicate decisions, storage choice, progress/cancel), searchable track list, track metadata editor, stable-artist manager, missing-file/relink health panel, interactive queue, transport, volume, repeat, status/errors, and a real-time oscilloscope fed by actual post-gain output samples. |
| `flux-tui` | binary `flux-tui` | Interactive terminal application (crossterm): track list with search, copy/in-place import, track/artist edit keys, missing-file marker, unique-candidate relink, queue add/play-next/remove/reorder, full keyboard control, same controller semantics as the GUI. |

Dependency direction: `flux-gui → flux-app + flux-core + flux-platform + iced`,
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
  The bottom player’s progress bar is a real seek control: click it or
  drag the thumb to preview a position, then release to seek. When the
  search box is not focused, `←`/`→` seek by five seconds (hold
  `Shift` for thirty seconds); the `Space`, `N`/`P`, and `S` transport
  shortcuts remain available. Transport tooltips name each icon, and
  actions that cannot run are visibly disabled.
  Use **Import music** for the review-first Import Manager: native file or
  folder picker, integrated Flux Browser, drag-and-drop, editable review,
  duplicate decisions, copy/reference storage, and real progress/cancel.
  Open *Library setup* to create/open a portable library, create/rename
  stable artists, and run *Reconcile library*. The pencil on a track opens
  the metadata editor (title, album, track/disc numbers, artist identity);
  this writes only Flux-owned overrides. Missing tracks stay listed with a
  `missing` marker, and unique move candidates appear in *Library health*
  with an explicit *Relink* action.
- **TUI:** `↑/↓` or `j/k` move, `Enter` plays, `Space` pauses, `s`
  stops, `n/p` skip, `+/-` volume, `R` repeat, `o` scan a folder
  (type the path), `r` refresh, `/` search, `q` quit. `i` quick-copy-imports
  paths (separated by `;`), `I` quick-imports paths already inside the portable
  library, `e` edits the selected track as
  `title|album|track|disc|artist-id`, `P` creates an artist, `Y` renames
  an artist (`id|new-name`), `V` lists artist IDs, `X` reconciles, and
  `L` applies a unique relocation candidate. For the queue, `a` appends
  the selected library track, `x` plays it next, `Delete` removes the
  current entry, and `Ctrl+↑`/`Ctrl+↓` reorder the current entry. The
  terminal is always restored on exit, including on errors.

### Import Manager

The GUI's **Import music** action opens a modal, review-first import workspace
while the player remains visible and responsive:

- choose files or a folder with the native OS dialog (zenity or kdialog on
  Linux, `osascript` on macOS, PowerShell WinForms on Windows), or use the
  integrated Flux Browser;
- drop audio files or folders anywhere in the window (Iced/winit file-drop is
  available on X11, Windows, and macOS; it is not implemented on Wayland);
- choose **Copy into the library** or **Reference existing files**;
- review every candidate before writing: title, artist, album, duration,
  format, file size, source path, duplicate/conflict status, intended
  destination, and incomplete-metadata state;
- edit title, artist, album, track number, and disc number per track while
  keeping the source values visible and marking modified candidates;
- decide per track or in bulk to import, skip, or update an exact-path
  existing track while preserving its stable `TrackId`; exact paths,
  possible same-content files, and merely similar metadata are distinct;
- see real discovery/metadata/copy/catalogue-write progress, cancel at safe
  boundaries, and review partial failures or completion.

Review edits become sparse Flux-owned overrides; audio tags and files are
never modified. Copied files go into the library root with deterministic,
collision-free names and never overwrite or remove originals. Reference mode
only accepts files already inside the portable library, because the catalogue
stores library-relative paths; external sources are blocked with a clear
prompt to switch to Copy. No new Rust dependency is used for the native
dialogs or the integrated browser. Artwork extraction/preview and stable
album identities are not part of this stage; the existing album string model
is reused. The TUI retains its direct shared-controller import keys; the full
review manager is GUI-only.

Manual verification in this environment (Linux/X11 under Xephyr): the full
Flux Browser import, review edits, duplicate decisions, completion, and OS
file drag-and-drop were exercised against a real portable catalogue. The
native zenity command was launched, but this headless X server had no window
manager/portal that could map the GTK dialog, so native dialog completion was
not claimed; the Flux Browser fallback was used and verified. Windows and
macOS picker code compiles but was not runtime-tested. Wayland file-drop is
not provided by Iced/winit.

- The legacy compatibility index lives at `library-index.json` in the
  per-user data directory (e.g. `~/.local/share/flux/` on Linux,
  `%LOCALAPPDATA%\flux\` on Windows, `~/Library/Application
  Support/flux/` on macOS) and is saved after every scan. It cannot hold
  Flux metadata overrides or stable artist identities; those require a
  portable library. A legacy queue is stored beside the legacy index as
  `<index>.queue.json`; a portable queue lives at
  `<library>/.flux/queue/queue.json`. Queue files are written atomically,
  survive restarts, and stay isolated per library. A malformed queue
  file is reported and preserved (queue writes are disabled for that
  session) rather than replaced with an empty queue.
- **Library management (portable libraries).** The catalogue lives
  inside the library at `<library>/.flux/`; no Flux-owned metadata is
  written to app-data directories in portable mode. The commit record is
  human-readable JSON (`catalog.flux`, plus the previous generation as
  `catalog.flux.bak`) and references immutable, generation-qualified
  track segments. The current schema is version 3: it adds a stable
  artist table (ID, canonical name, optional aliases) and track-to-artist
  references to the version-2 track/path/tag/override records. Version-2
  stores remain readable and are upgraded on the first successful commit;
  malformed/unsupported primary catalogues are never overwritten, and a
  valid backup opens in an explicit read-only recovery state until
  acknowledged.
  - **Metadata precedence:** Flux-owned override → embedded tag →
    filename/folder fallback, with an always-non-empty display title.
    Editing a track writes only sparse overrides; audio tags and files
    are never modified.
  - **Stable identities:** track and artist IDs and their counters
    persist. Artist renames change the canonical name while every related
    track keeps its ID and overrides. Equal display names are allowed and
    never auto-merged; the GUI shows the ID where names collide.
  - **Import:** the GUI provides the review-first Import Manager and the
    TUI keeps direct copy/in-place import keys. *Copy* imports external
    files into the library root with collision-free names and never moves,
    overwrites, or deletes originals; *reference/in-place* accepts only
    files already inside the library. Repeated identical imports are
    detected and never silently create a duplicate. External absolute
    references without copying are rejected with a clear per-file error.
  - **Rescans and maintenance:** scans add/update by path and preserve
    IDs, overrides, artists, and queue references. Reconcile records
    changed size/mtime fingerprints, keeps missing records, and reports
    possible relocation candidates. Applying a candidate is explicit
    (GUI *Relink* / TUI `L` for a unique candidate) and revalidates the
    fingerprint; nothing is moved automatically.
- A queue that has never been structurally edited follows newly scanned
  tracks automatically; adding, removing, or reordering an entry turns
  it into a fully user-managed queue. New imports join only an
  auto-derived queue. Entries whose tracks disappear from the library
  are retained and shown as missing until explicitly removed.
- Seeking moves the actual decoder, not just the displayed timestamp.
  The engine reopens the current file and pre-rolls to the target on its
  worker thread, flushes buffered output from the old position, and
  preserves playing, paused, or stopped state. On long files the
  pre-roll is a short silent decode pass proportional to the target
  time; the UI shows the intended position immediately.
- The oscilloscope is reproduced from Flow's
  `src/javascript/effect.js` Web Audio time-domain analyser. Flux uses
  a bounded lock-free tap of the real post-gain output and the same
  4096-sample, 8-bit processing: RMS smoothing (`0.85/0.15`), a
  `0.45 / RMS` gain target clamped to `1.0..=2.6`, attack/release
  interpolation (`0.25/0.05`), a 48 px rounded canvas, dark background,
  glow, and a `#ff66cc` 1 px line. Flux taps device-rate
  post-resampling output, so sample values are not bit-for-bit identical
  to a browser's Web Audio graph. The native renderer samples the real
  window with linear interpolation per pixel column and adds a faint
  zero guide plus a translucent halo; these are presentation changes
  only, never a synthetic waveform. The waveform algorithm, gain
  envelope, colors, dimensions, and playback-state behavior mirror Flow;
  native rasterization and pixel-level antialiasing necessarily differ.

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

Playlists, artwork/cover management, gapless playback, crossfade, EQ,
frequency/spectrum visualizers (the current oscilloscope is a
time-domain waveform), still-more formats/codecs (AIFF, APE, WavPack,
ALAC, surround Opus), folder watching, streaming providers, accounts,
lyrics, installers, and updaters are future milestones. Native OS file
pickers. External references without copying are intentionally rejected;
album identities, artwork extraction/preview, and artist deletion are still
future work. Native dialogs and file-drop are implemented, but cross-platform
runtime verification is limited to the Linux/X11 environment described below. Nothing in
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
