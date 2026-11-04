# Flux

Flux is a native, cross-platform desktop music player written in Rust —
the successor to the Flow web prototype, keeping Flow's product ideas
while replacing the implementation with a native architecture: an Iced
graphical application and a dedicated terminal application sharing the
same core logic, library, and playback engine.

**Status: canonical library runtime integrated.**
Both applications now open, create and switch canonical self-contained Flux
libraries. The GUI and TUI share the same application controller; audio is
copied into the library, metadata is inferred from filenames and folders and
reviewed without reading tags, playback and queue identity use stable track IDs,
and queues persist per library. Legacy app-data indexes and schema-v3 portable
catalogues are migration sources only in the normal front-end workflows.

The GUI includes a review-first Import Manager with native pickers,
drag-and-drop, duplicate decisions, editable metadata, copy-only staging,
progress and cancellation. Existing playback, seeking, queue behavior and the
real-time oscilloscope continue to use the same engine. Managed artwork is
decoded and displayed in album and player views. Dedicated canonical playlist
management and an integrated lyrics library/editor/karaoke system are
implemented; rating UI remains incomplete and is called out below rather than
presented as finished.

## Canonical storage

The canonical library is the normal runtime backend. Its layout and authority
rules are documented in [`docs/canonical-library.md`](docs/canonical-library.md).
A library owns its managed audio, stable artist/album/track IDs, per-entity
`.flux` records, managed lyrics/cover primitives, playlists, ratings and
transaction journals. Every persisted managed path is relative to the library
root. `catalog.flux` is authoritative for identity/ownership/paths and is
published last; per-entity files are validated against it. Audio tags are not a
metadata source and are never rewritten.

Older formats remain reachable only through the explicit migration planner:
the legacy `library-index.json` and portable schema-v3 `.flux` catalogue are
read-only sources. The public Controller still contains compatibility
constructors for tests and migration integration; no GUI/TUI normal workflow
selects or writes those backends.

## Crates

| Crate | Kind | Responsibility |
|---|---|---|
| `flux-core` | library | Backend-independent domain model: tracks, IDs, sources, the play queue with stable per-entry identities (duplicates allowed), repeat/playback states, and the shared error type. No UI, filesystem, database, audio-device, or network implementation. |
| `flux-library` | library | Canonical self-contained library engine: stable artist/album/track/playlist IDs, relative managed paths, per-entity `.flux` metadata, transaction journal, copy verification, playlists/ratings/lyrics, and isolated migration from legacy/schema-v3 sources. The older in-memory scanner and portable store remain for compatibility tests and migration inventory. |
| `flux-audio` | library | Playback, in two strictly separate layers: `Player`, the pure playback-control state machine and single authority for playback decisions; and a threaded engine that decodes WAV (hound), FLAC (claxon), MP3 (rmp3/minimp3), AAC in ADTS streams and MP4/M4A files (rusty_aac with the mp4 demuxer), and Ogg Vorbis and Opus (lewton and opus-pure over the ogg demuxer) on a worker thread and feeds an output backend (device-free `NullOutput`, or CPAL behind the non-default `cpal` feature) through a lock-free ring buffer. |
| `flux-app` | library | The shared application controller: owns the canonical library, playback model, engine, volume and queue persistence; implements canonical create/open/switch, filename-only import inference, duplicate decisions, copy-only publication, artist/album/track edits, playlist management, lyrics parsing/editing/import/export APIs, playlists/ratings/lyrics persistence, and the canonical migration workflow. Both front-ends use this one controller. |
| `flux-providers` | library | Data-only provider descriptors and capability vocabulary for future legally supported streaming integrations. Nothing networked exists yet. |
| `flux-platform` | library | OS integration: per-user data/config directory resolution and dependency-free native audio/image file and folder pickers for Linux/XDG, macOS, and Windows (standard library plus the platform's own dialog commands). |
| `flux-gui` | binary `flux` | Native [Iced](https://iced.rs) application over the canonical controller: review-first copy-only Import Manager (native pickers, Flux Browser, drag-and-drop, name-inferred metadata, duplicate decisions, progress/cancel), canonical library setup and migration workflow, searchable track/artist/album views, metadata and stable-artist editing, reviewed track move/audio replacement, decoded managed album artwork, dedicated playlist management/playback, lyrics library/editor/karaoke, dashboard/smart collections/rating controls, native artwork/audio/lyrics pickers, queue, transport, volume, repeat, status/errors, and the real-time oscilloscope. |
| `flux-tui` | binary `flux-tui` | Interactive terminal application (crossterm): track list with search, canonical create/open, copy-only quick import, folder import, track/artist edit keys, missing-file marker, canonical migration workflow, queue add/play-next/remove/reorder, and the same controller semantics as the GUI. |

Dependency direction: `flux-gui → flux-app + flux-core + flux-platform + iced`,
`flux-tui → flux-app + flux-core + crossterm`,
`flux-app → flux-core + flux-library + flux-audio + flux-platform`,
`flux-library → flux-core + lofty + walkdir + serde/serde_json`, and
`flux-audio → flux-core + claxon + hound + rmp3 + lewton + ogg +
opus-pure + rusty_aac + mp4 + ringbuf` (plus optional `cpal`). `flux-core`, `flux-providers`, and `flux-platform` have no
dependencies. The engine never scans or persists the library. The canonical catalogue
and queue store are separate files, and `Player` remains the only
playback-decision authority — the controller keeps the engine in sync
with it. Queue files are namespaced by active library and never by audio
tags or absolute audio paths.

## Using Flux

Build the applications **with** the `cpal` feature for real sound
(default builds are device-independent and will say audio is
unavailable instead of pretending):

```sh
cargo run -p flux-gui --features cpal            # binary name: flux
cargo run -p flux-tui --features cpal -- ~/Music # optional folder to scan
```

- **GUI:** create or open a canonical library in *Library setup*. Use
  **Import music** for the review-first copy-only Import Manager: native file
  or folder picker, Flux Browser, drag-and-drop, name-inferred editable review,
  duplicate decisions, and real progress/cancel. Click a track to play it; use
  the transport row for pause/stop/skip/repeat/volume. The search box performs
  a grouped, case-insensitive global search over canonical artists, albums and
  tracks with direct navigation. Track rows expose favorite and rating controls;
  track views support favorites/lyrics/artwork/release/rating filters,
  ascending/descending deterministic sorting by title/artist/album/track/rating/
  date-added/duration/play-count/last-played, and multi-selection by stable
  TrackIds. A Dashboard summarizes canonical counts plus locally recorded
  listening history; Smart Collections provide Favorites, Highly Rated,
  Recently Added, Recently Played, Most Played, Without Lyrics, Synchronized
  Lyrics and Without Artwork views. Play counts/history begin only after this
  feature is introduced and are stored separately from canonical metadata.
  Selection actions can set/unset favorites or create/append canonical
  playlists after an explicit review. A dedicated Playlists section supports
  empty or selection-based creation, rename, detail browsing, duplicate-safe
  position selection, removal, drag/arrow reordering, clear/delete confirmation,
  play/shuffle/enqueue, and multi-entry queue actions.
  A Lyrics section lists managed plain/synced lyrics, opens a reader with
  playback highlighting, a line/timestamp editor, LRC/TXT import with preview,
  UTF-8 export with overwrite confirmation, and a karaoke view. The player has a
  Lyrics entry point and can follow current playback without changing audio.
  Artist and album views
  group canonical stable identities. The queue panel supports stable
  per-entry duplicate disambiguation, drag/keyboard reordering, removal and
  direct entry playback; the seek bar moves the decoder. Playback modes cycle
  Normal → Repeat All → Repeat One → Shuffle → Shuffle + Repeat All, with a
  stable shuffle cycle that does not mutate playlist order. Previous restarts
  the current track after three seconds and otherwise moves backward. Queue
  order, current entry, mode, shuffle flag and position persist for canonical
  libraries and restore paused. The engine preloads the next queue entry and
  performs natural handoffs inside the audio pipeline at the exact
  source-frame boundary, without restarting the output stream. An optional
  equal-power crossfade of 0–5000 ms can overlap the outgoing and incoming
  sources; zero retains gapless playback. Crossfade is off by default and is
  configured in the GUI **Settings** view (`K`) or the TUI (`K` toggles,
  `[`/`]` adjusts by 250 ms). The shared setting is stored in
  `<config>/flux/settings.json`. The
  pencil edits canonical metadata and exposes **Transfer owner** with an
  explicit destination release title; the transfer copies and verifies audio,
  then removes old files through journal cleanup. The same editor supports
  ordered credits, reviewed album moves (source album remains, even when empty)
  and reviewed managed-audio replacement: new audio is staged and verified, the
  old audio stays until the catalogue commit, and the user source file is never
  modified. Missing managed audio stays listed with a `missing` marker. *Refresh & validate* reopens the canonical
  catalogue and validates its resource paths.
  Album artwork is rendered from the canonical managed cover (PNG/JPEG/WebP)
  asynchronously with bounded dimensions and a fallback glyph. Native pickers
  cover general import, album-target import, audio replacement and artwork
  selection; every selection still enters the existing review/confirmation
  flow. Cancellation leaves the workflow unchanged.
- **TUI:** `↑/↓` or `j/k` move, `Enter` plays, `Space` pauses, `s`
  stops, `n/p` skip, `+/-` volume, `R` repeat, `o` imports a folder
  (type the path), `r` refreshes, `/` searches tracks by title, artist or album,
  `q` quit. `i` quick-imports
  paths (separated by `;`) by copying them into the canonical library, `e`
  edits the selected track as `title|album|track|disc|artist-id|credit-ids`, `P` creates
  an artist, `Y` renames an artist (`id|new-name`), `V` lists artist IDs, `X`
  refreshes/validates the canonical library, and `M` runs the canonical
  migration workflow. `y` opens read-only lyrics, `F` toggles favorites-only
  browsing, and `H` reports most-played/local history statistics.
  Global artist/album/track search, filters, multi-selection, playlist
  management, lyrics editing/import/export/karaoke and batch playlist/favorite
  actions require the GUI.

  Track moves and
  managed-audio replacement also require the GUI Track-editor review. `D` shows the selected track's album ownership; `E` lists
  every catalogue album, including empty albums, with owner, track count and
  cover availability. `T` refuses ownership transfer and destructive artist deletion,
  directing the user to the GUI review/confirmation workflow. For the
  queue, `a` appends, `x` plays next, `Delete` removes the current entry, and
  `Ctrl+↑`/`Ctrl+↓` reorder. The terminal is always restored on exit.

### Import Manager

The GUI's **Import music** action opens a review-first workspace while the
player remains visible:

- choose files or a folder with the native OS dialog (zenity/kdialog on Linux,
  `osascript` on macOS, PowerShell WinForms on Windows), or use Flux Browser;
- drop audio files or folders (Iced/winit X11/Windows/macOS; Wayland file-drop
  is not supported by the toolkit);
- infer provisional artist, album, title, track number and disc number only
  from filenames and directory names — no embedded tag is read;
- edit ordered credits, add existing or named artists, remove/reorder credits,
  and choose the physical owner from the credits; the first credit is the
  default owner;
- review and edit every candidate, with incomplete metadata visibly marked;
- classify exact/repeated selection paths, identical content at different
  paths, and similar metadata with different content; similar tracks are never
  merged automatically;
- copy every selected file into the canonical library, verify the copy by size
  and content hash inside the catalogue transaction, and never expose a
  partially copied file as a catalogue entry;
- report progress and per-item failures, preserve successful items when later
  items fail, and cancel before commit.

Reference-only/external imports are not available in canonical mode. Review
edits become canonical Flux metadata; audio tags and source files are never
modified. The canonical transaction stages files inside the library and only
publishes `catalog.flux` after every copy verifies. Managed covers are decoded
asynchronously for display; no tag artwork extraction occurs. The TUI uses
the same filename inference and copy-only application path via its quick-import
keys; the full review UI is GUI-only.

Migration is explicit: *Library setup* accepts a legacy index file or portable
schema-v3 folder, presents a read-only inventory and disk estimate, builds and
validates a separate canonical destination, and only then offers activation.
The source is never modified or deleted. Migration execution and import
copying currently run synchronously at their commit boundary; responsive
background staging for very large migrations is still future work.

### Library storage

- **Canonical library.** `library.flux` identifies the library, `catalog.flux`
  is the authoritative committed catalogue, stable IDs are never reused,
  per-entity metadata lives in `.flux` files, and `transactions/` records
  recovery state. Queue state is stored at `<library>/queue.flux` and carries
  stable TrackIds, not paths. A malformed queue file is reported and preserved
  (queue writes disabled for that session) rather than replaced.
- **Legacy/portable sources.** `library-index.json` and the schema-v3 `.flux`
  catalogue are read only by the explicit migration workflow. The front-ends no
  longer switch to or write them as normal runtime backends.
- **Stable identities and credits:** track, artist, album and playlist IDs and
  counters persist. A track stores ordered credits plus one physical owner;
  featuring artists appear in their own discographies without duplicate audio.
  Artist/album renames are metadata-only because canonical directories derive
  from IDs, so case-only renames cannot break paths. Single-track ownership
  transfer uses an explicit destination release, copy-first verification and
  journal-owned cleanup; the GUI track editor exposes it, while the TUI refuses
  and points to the GUI. **Delete artist** opens the canonical multi-item
  deletion preview: transfers, credit-only retention and explicit permanent
  deletions are reviewed per track, with playlist/rating/lyrics consequences,
  stale-plan checks and one transactional commit. The artist is never removed
  while a track or credit reference remains. Selecting an album exposes
  **Transfer album…**: AlbumId, TrackIds, order, cover, lyrics, ratings,
  playlists and queue references are preserved; missing destination credits
  require an explicit per-track decision, and merging into another AlbumId is
  intentionally blocked. Albums are catalogue-first: empty albums remain
  visible in the album list with a dedicated detail view for metadata, cover,
  ordering, transfer and safe empty-album deletion. Populated album deletion is
  blocked until tracks are transferred or explicitly deleted. **Add tracks…**
  opens the existing Import Manager in fixed-album mode: the AlbumId and owner
  are locked through review, inferred metadata/credits remain editable, and all
  selected new tracks are appended in one canonical transaction while existing
  metadata, cover, order and references are preserved. General and fixed-album
  imports stage proposed artists and albums in the same transaction; a failed
  import cannot leave behind a newly created artist.
- **Import:** every canonical import copies into the library and preserves the
  original extension; sources are never moved, overwritten or deleted.
  Identical content is deduplicated by fingerprint unless the user explicitly
  chooses to import it as a separate track. The review classifies exact audio
  duplicates, incoming duplicates, metadata collisions, same-title/different-audio
  conflicts, probable duplicates and ambiguous matches. Users explicitly choose
  Skip, Import separately, or Update metadata for a unique exact-audio match;
  updates preserve TrackId, audio, owner, album, lyrics, ratings, playlists and
  queue references. Duplicate review itself never overwrites managed audio; use
  the separate Track editor replacement workflow for that operation.
- **Missing resources:** a managed audio file can disappear; the track record
  remains, playback fails clearly, and validation reports it. Missing audio
  blocks migration activation until resolved or explicitly omitted by a future
  policy.

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
import a folder containing one known-good file per format — FLAC, WAV,
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

Rating UI, EQ,
frequency/spectrum visualizers, more codecs, folder watching, streaming,
accounts, installers and updaters remain future work. External reference
imports are intentionally rejected in canonical mode. Rating storage APIs exist
but are not yet exposed as a screen.
Native dialogs and file-drop are implemented; cross-platform runtime
verification remains limited to Linux/X11. Nothing here makes network
requests, collects telemetry, or requires an account.

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
