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
progress and cancellation, including explicit Album/Track artwork and lyrics
attachment before the atomic commit. Existing playback, seeking, queue behavior
and the real-time oscilloscope continue to use the same engine. Managed album
and artist artwork plus optional per-Track artwork overrides are decoded and
displayed throughout the library. Artists and Albums are first-class editable
entities with detail pages, relationships and navigation to their Tracks.
Album membership is optional: standalone Tracks, singles and incomplete imports
remain fully functional without a placeholder Album. Import review supports
multi-candidate selection, shared Artist/Album assignment and shared Album
artwork while preserving per-Track overrides. Dedicated canonical playlist
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
published last; per-entity files are validated against it. A Track's Album
reference is optional: Album members live under the Album directory, while
standalone Tracks use an owner Artist `standalone/tracks/` container. Audio tags
are not a metadata source and are never rewritten.

Older formats remain reachable only through the explicit migration planner:
the legacy `library-index.json` and portable schema-v3 `.flux` catalogue are
read-only sources. The public Controller still contains compatibility
constructors for tests and migration integration; no GUI/TUI normal workflow
selects or writes those backends.

## Crates

| Crate | Kind | Responsibility |
|---|---|---|
| `flux-core` | library | Backend-independent domain model: tracks, IDs, sources, the play queue with stable per-entry identities (duplicates allowed), repeat/playback states, and the shared error type. No UI, filesystem, database, audio-device, or network implementation. |
| `flux-library` | library | Canonical self-contained library engine: stable artist/album/track/playlist IDs, relative managed paths, per-entity `.flux` metadata, transaction journal, copy verification, playlists/ratings/lyrics, album/artist images, per-Track artwork overrides, atomic enriched imports, and isolated migration from legacy/schema-v3 sources. The older in-memory scanner and portable store remain for compatibility tests and migration inventory. |
| `flux-audio` | library | Playback, in two strictly separate layers: `Player`, the pure playback-control state machine and single authority for playback decisions; and a threaded engine that decodes WAV (hound), FLAC (claxon), MP3 (rmp3/minimp3), AAC in ADTS streams and MP4/M4A files (rusty_aac with the mp4 demuxer), and Ogg Vorbis and Opus (lewton and opus-pure over the ogg demuxer) on a worker thread and feeds an output backend (device-free `NullOutput`, or CPAL behind the non-default `cpal` feature) through a lock-free ring buffer. |
| `flux-app` | library | The shared application controller: owns the canonical library, playback model, engine, volume and queue persistence; implements canonical create/open/switch, filename-only import inference, enriched import review, duplicate decisions, copy-only publication, artist/album/track edits, owned/credited relationships, playlist management, lyrics parsing/editing/import/export APIs, playlists/ratings/lyrics persistence, and the canonical migration workflow. Both front-ends use this one controller. |
| `flux-providers` | library | Data-only provider descriptors and capability vocabulary for future legally supported streaming integrations. Nothing networked exists yet. |
| `flux-platform` | library | OS integration: per-user data/config directory resolution and dependency-free native audio/image file and folder pickers for Linux/XDG, macOS, and Windows (standard library plus the platform's own dialog commands). Dialog output preserves spaces and Unix non-UTF-8 path bytes; Windows PowerShell output is forced to UTF-8. |
| `flux-gui` | binary `flux` | Native [Iced](https://iced.rs) application over the canonical controller: review-first copy-only Import Manager (native pickers, Flux Browser, drag-and-drop, name-inferred metadata, enriched artwork/lyrics attachment, duplicate decisions, progress/cancel), canonical library setup and migration workflow, searchable track/artist/album views, dedicated Artist and Album detail pages, metadata/relationship/artwork editing, reviewed track move/audio replacement, decoded managed Album/Artist/Track artwork with override semantics, dedicated playlist management/playback, lyrics library/editor/karaoke, dashboard/smart collections/rating controls, native artwork/audio/lyrics pickers, queue, transport, volume, repeat, status/errors, and the real-time oscilloscope. |
| `flux-tui` | binary `flux-tui` | Interactive terminal application (crossterm): track list with search, canonical create/open, copy-only quick import, folder import, track/artist edit keys, Artist/Album relationship inspection, inline artwork/lyrics markers, missing-file marker, canonical migration workflow, queue add/play-next/remove/reorder, and the same controller semantics as the GUI. Rich artwork/lyrics/import review remains GUI-only and is documented below. |

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

- **GUI:** create or open a canonical library in *Library setup*. Each
  successful open or same-path reopen establishes a fresh in-memory library
  session; failed opens leave the active library untouched, and scans must
  finish before switching. Stale import, artwork, lyrics and file-picker
  results are dropped, and the previous library's selection, queue, editor,
  caches and pending playback are cleared while global preferences remain. The
  sidebar groups **Library**, **Curated** and **Tools** destinations in a
  scrollable list at small window heights, and shows the active library's
  storage mode, folder name and full path on hover. Use **Import music** for the
  review-first copy-only Import Manager: native file or folder picker, Flux
  Browser, drag-and-drop, name-inferred editable review, duplicate decisions,
  and real progress/cancel. Final result rows identify each reviewed source path
  and retain per-item failure details; Flux Browser warns when entries vanish or
  symbolic links are skipped instead of silently omitting them. Click a track to
  play it; use the transport row for pause/stop/skip/repeat/volume. The queue is
  inline on wide windows and opens as a closable overlay on narrower ones via
  **Queue** or `Q`, so it remains available at the minimum width. The search box
  performs a grouped, case-insensitive global search over canonical artists,
  albums and tracks with direct navigation. Track rows expose favorite and
  rating controls and label missing versus inaccessible audio distinctly;
  track views support favorites/lyrics/artwork/release/rating filters,
  ascending/descending deterministic sorting by title/artist/album/track/rating/
  date-added/duration/play-count/last-played, and multi-selection by stable
  TrackIds. A Dashboard summarizes canonical counts plus locally recorded
  listening history; Smart Collections provide Favorites, Highly Rated,
  Recently Added, Recently Played, Most Played, Without Lyrics, Synchronized
  Lyrics and Without Artwork views. Play counts/history begin only after this
  feature is introduced and are stored separately from canonical metadata.
  Only delivered playback advances listening time; paused, gated, recovering
  and seek-skipped intervals are excluded, transitions finalize the outgoing
  entry at the handoff boundary, and one contiguous queue-entry attempt can
  count at most one play.
  Selection actions can set/unset favorites or create/append canonical
  playlists after an explicit review. A dedicated Playlists section supports
  empty or selection-based creation, rename, detail browsing, duplicate-safe
  position selection, removal, drag/arrow reordering, clear/delete confirmation,
  play/shuffle/enqueue, and multi-entry queue actions.
  A Lyrics section lists managed plain/synced lyrics, opens a reader with
  playback highlighting, a line/timestamp editor, LRC/TXT import with preview,
  UTF-8 export with overwrite confirmation, and a karaoke view. The player has a
  Lyrics entry point and can follow current playback without changing audio.
  Artist and album views group canonical stable identities. An Artist detail
  page edits display name, description and managed image, lists owned and
  credited Albums plus related Tracks, and navigates directly to those entities.
  Album detail edits title/release metadata and cover artwork, shows owner and
  ordered membership, and supports reorder, fixed-target import, transfer and
  safe empty deletion. The Albums view includes a **Standalone tracks** group for
  Tracks with no Album. Track editing can add a standalone Track to an Album,
  move a member between Albums owned by the same Artist, convert a member to
  standalone while retaining the Track and its empty source Album, correct
  ordered credits/owner and assign or remove a managed Track artwork override;
  removing the override restores Album artwork or the standalone fallback. The
  queue panel supports stable
  per-entry duplicate disambiguation, drag/keyboard reordering, removal and
  direct entry playback; the seek bar moves the decoder. Playback modes cycle
  Normal → Repeat All → Repeat One → Shuffle → Shuffle + Repeat All, with a
  stable shuffle cycle that does not mutate playlist order. Previous restarts
  the current track after three seconds and otherwise moves backward. Queue
  order, current entry, mode, shuffle flag and position persist for canonical
  libraries and restore paused. Restored and engine positions are bound to the
  stable queue entry, so changing selection cannot carry a position onto a
  duplicate entry or another track. Closing the GUI saves the index and a
  coherent queue snapshot before an idempotent Controller shutdown. The engine
  preloads the next queue entry and performs natural handoffs inside the audio
  pipeline at the exact
  source-frame boundary, without restarting the output stream. An optional
  equal-power crossfade of 0–5000 ms can overlap the outgoing and incoming
  sources; zero retains gapless playback. Crossfade is off by default and is
  configured in the GUI **Settings** view (`K`) or the TUI (`K` toggles,
  `[`/`]` adjusts by 250 ms). The shared setting is stored in
  `<config>/flux/settings.json`. A local `AudioDiagnostics` snapshot exposes
  callback/underrun/ring/transition counters without callback logging or
  telemetry; deterministic stress coverage is documented in the canonical
  audio documentation. The
  pencil edits canonical metadata and exposes **Transfer owner** with an
  explicit destination release title; the transfer copies and verifies audio,
  then removes old files through journal cleanup. The same editor supports
  ordered credits, reviewed album moves (source album remains, even when empty)
  and reviewed managed-audio replacement: new audio is staged and verified, the
  old audio stays until the catalogue commit, and the user source file is never
  modified. Missing managed audio stays listed with a `missing` marker and can
  be repaired through the reviewed replacement flow, preserving TrackId and all
  references; inaccessible paths are reported and never silently overwritten.
  The same editor offers a separately reviewed single-track deletion that stops
  playback if needed, removes duplicate queue entries, playlist references,
  ratings, history and managed track resources, while retaining the album and
  artist. *Refresh & validate* reopens the canonical catalogue and validates its
  resource paths. Album artwork is rendered from the canonical managed cover
  (PNG/JPEG/WebP) asynchronously with bounded dimensions and a fallback glyph;
  missing covers can be replaced or removed explicitly, with image validation
  before publication. Track artwork is an optional managed override stored by
  stable TrackId; Track rows and the player use it first and fall back to Album
  artwork. Album cover replacement does not alter Track overrides, and removing a
  Track override restores the inherited Album image. Native pickers cover
  general import, album-target import, audio replacement, Album/Artist/Track
  artwork and lyrics selection; every selection still enters the existing
  review/confirmation flow. Cancellation leaves the workflow unchanged.
- **TUI:** `↑/↓` or `j/k` move, `Enter` plays, `Space` pauses, `s`
  stops, `n/p` skip, `+/-` volume, `R` repeat, and `q`/`Ctrl-C` quit; `Ctrl-C`
  quits from input, help, lyrics and queue views too. `/` searches tracks by
  title, artist or album, `F` toggles favorites-only, and `H` reports local
  play statistics. `?` opens the full scrollable command/safety reference
  instead of relying on a clipped one-line help bar. The header identifies the
  active library, view, queue length, playback mode and active search/favorite
  filter; long paths/inputs are bounded and input lines keep their trailing
  path visible. `c` clears a transient status/error notice without changing
  playback or library state.
  `o` recursively copy-imports a folder, and `i` copy-imports
  paths separated by `;` into the canonical library. Import summaries include
  added, updated, already-indexed, copied and issue counts; any per-path errors
  open a scrollable result panel containing every issue rather than only the
  first. This quick flow remains synchronous and has no review or cancel phase.
  `e` edits the selected track as
  `title|album|track|disc|artist-id|credit-ids`, `P` creates an artist, `Y`
  renames an artist (`id|new-name`), `V` opens a scrollable Artist relationship
  view, `E` opens the Album/order view, `D` shows the selected Track's Album,
  `X` refreshes/validates the canonical library, and `M` runs the canonical
  migration workflow. `y` opens read-only managed lyrics. Rows mark managed
  Track artwork and lyrics; Album detail labels inherited versus Track-specific
  artwork.
  The TUI is inspection/limited-edit by design. Album metadata, cover,
  reorder/membership/move and Track artwork pickers, rich import review with
  artwork/lyrics attachment, description/image editing and ownership transfer
  require the GUI. The TUI never fabricates a destructive shortcut for them.
  Global artist/album/track search, filters, multi-selection, playlist
  management, lyrics editing/import/export/karaoke and batch playlist/favorite
  actions also require the GUI.

  `Q` opens a queue-entry view: `Enter` plays the selected stable
  `QueueEntryId`, `Delete`/`d` removes it, and `Ctrl+↑`/`Ctrl+↓` reorders it.
  Duplicate tracks remain visibly distinct by position and `#entry` id. From
  the track view, `a` appends and `x` plays next. Missing managed audio and
  inaccessible paths are labelled separately; the TUI never rewrites them or
  treats an inaccessible path as missing.
  Track moves, managed-audio repair/replacement, artwork repair and
  single-track deletion require the GUI review workflow. Rows mark standalone
  Tracks. `D` shows the selected Track's Album ownership or standalone state;
  `E` lists every catalogue album, including empty albums, plus the standalone
  Track group, with owner, order, cover and artwork/lyrics markers. `T` refuses
  ownership transfer and destructive artist deletion, directing the user to the
  GUI.
  `U` preserving a protected index and `A` acknowledging portable recovery both
  require an explicit `y` confirmation. The terminal is always restored on exit.

### Import Manager

The GUI's **Import music** action opens a review-first workspace while the
player remains visible:

- choose files or a folder with the native OS dialog (zenity/kdialog on Linux,
  `osascript` on macOS, PowerShell WinForms on Windows), or use Flux Browser;
- drop audio files or folders (Iced/winit X11/Windows/macOS; Wayland file-drop
  is not supported by the toolkit);
- infer provisional artist, album, title, track number and disc number only
  from filenames and directory names — no embedded tag is read; a missing Album
  remains a standalone Track and never creates an “Unknown Album” placeholder;
- edit ordered credits, add existing or named artists, remove/reorder credits,
  and choose the physical owner from the credits; the first credit is the
  default owner;
- select an existing stable Artist and an existing Album owned by that Artist,
  create a new Album by title, or leave Album empty for a standalone Track;
  ambiguous/fuzzy names are never merged;
- check multiple candidates and apply one existing Artist or shared new Artist
  name, one Album title (or standalone state), and one Album artwork asset to the
  checked set; per-candidate Artist/Album edits override the bulk value
  afterward;
- attach a validated managed Album cover, a Track-specific artwork override, and
  an LRC/TXT lyrics file to individual candidates before commit; Album artwork
  never overwrites individual Track artwork overrides;
- review and edit every candidate, with incomplete metadata visibly marked and
  inherited Album artwork distinguished from Track overrides;
- validate each candidate by opening its decoder before review and again while
  preparing the copy; unsupported extensions, malformed audio, missing files
  and unreadable paths are itemized while unrelated valid files remain
  importable;
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
modified. The canonical transaction stages audio, cover/override artwork and
lyrics inside the library and only publishes `catalog.flux` after every copy
verifies. A changed artwork/lyrics source fingerprint or conflicting cover for
one Album rejects the batch with complete-old state; no artist, Album, Track or
managed asset is left partially published. Managed covers are decoded
asynchronously for display; no tag artwork extraction occurs. The TUI uses the
same filename inference and copy-only application path via its quick-import
keys; rich review with artwork/lyrics attachment is GUI-only. Bulk Artist,
Album/standalone assignment and shared Album artwork are explicit actions on
checked candidates; lyrics and Track artwork remain per-candidate overrides.

Migration is explicit: *Library setup* accepts a legacy index file or portable
schema-v3 folder, presents a read-only inventory and disk estimate, builds and
validates a separate canonical destination, and only then offers activation.
The source is never modified or deleted. Migration execution and import
copying currently run synchronously at their commit boundary; responsive
background staging for very large migrations is still future work.

### Library storage

- **Canonical library.** `library.flux` identifies the library, `catalog.flux`
  is the authoritative committed catalogue, stable IDs are never reused,
  per-entity metadata lives in `.flux` files, ratings/playlists are catalogue
  entities, and `transactions/` records recovery state. Queue state is stored at
  `<library>/queue.flux` and carries stable TrackIds, not paths. Listening
  history is derived application state in `<library>/history.flux`. Malformed
  queue/history files are reported and preserved (their writes are disabled for
  that session) rather than replaced.
- **Legacy/portable sources.** `library-index.json` and the schema-v3 `.flux`
  catalogue are read only by the explicit migration workflow. The front-ends no
  longer switch to or write them as normal runtime backends.
- **Shared preferences.** Crossfade settings live in
  `<config>/flux/settings.json`. Unknown fields are preserved across known
  updates; malformed, invalid-version, or newer-schema files are preserved and
  write-protected.
- **Atomic publication.** Legacy indexes, queue, history and settings all use the
  shared `flux-library` atomic writer: unique sibling temporary file, file
  flush, atomic replacement, Unix parent-directory sync, and failure cleanup.
  Canonical journals and portable generations retain their specialized
  journal/backup/no-overwrite protocols.
- **Stable identities and credits:** track, artist, album and playlist IDs and
  counters persist. A track stores ordered credits plus one physical owner;
  featuring artists appear in their own discographies without duplicate audio.
  Artists own editable display name, optional description and managed image.
  Albums own title/release/cover and canonical order. Track artwork is an
  optional override keyed by TrackId; Album artwork is the display fallback and
  replacing it never rewrites Track overrides. Artist/album renames are
  metadata-only because canonical directories derive from IDs, so case-only
  renames cannot break paths. Single-track ownership
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

Runtime output failures are handled separately from decoder errors.
Nonfatal CPAL notifications such as a route change do not stop
playback. A fatal stream failure preserves the queue, current track
identity and play/pause intent, releases the failed stream, and
attempts up to four fresh opens of the current default device in the
background with bounded backoff. A successful replacement gets a new
ring, generation, device callback and prebuffer; it reloads the same
queue entry and restores the last delivered position. Pause during
recovery preserves the paused position; Stop clears the recovery
restore point, so a later Play restarts from the beginning. If every
attempt fails, the UI reports the output as unavailable and exposes
an explicit **Retry audio output** action in Library setup; Play also
starts a fresh bounded retry. The GUI player bar distinguishes
recovering, retryable failure and build-without-backend states, and
transient error banners can be dismissed after reading them.

Library setup now lists the current output device and available CPAL outputs.
**Follow system default** remains a distinct policy; selecting a named device
turns that backend ID into an explicit preference. Refreshing the list never
changes the selection. Duplicate display names remain distinct by their
backend IDs, and a device that cannot currently be opened is shown as
unavailable rather than silently chosen. Selecting a device opens it on a
worker before stopping the previous stream; if opening fails, the previous
output and playback remain active and the error is reported. A successful
switch preserves the current queue entry/TrackId and playback intent, reapplies
the delivered position, and does not add listening time. Because the new stream
opens before the old one is stopped, exclusive-access backends may reject a
same-device switch; Flux preserves the previous stream and reports it. Device
switching is a controlled interruption, not a claimed gapless transition.

The explicit device ID is stored in `settings.json` using the backend-provided
CPAL identifier, never a device handle or display name, and is written through
the existing schema-protected atomic settings path. If that saved device is
missing at startup, Flux keeps the explicit preference, starts in a truthful
unavailable state and retries it through bounded recovery instead of silently
falling back to another output; the user can choose the system default or
another device in Library setup. Builds without CPAL expose no fake device
list. The TUI reports the active/default policy and recovery guidance but keeps
device selection in the GUI.

On the Linux validation host (PulseAudio on PipeWire 1.6.9), CPAL enumerated 37
output endpoints, identified `alsa:default` as the system default, opened it at
48 kHz stereo, and completed a one-second WAV. Explicit initialization on a
second endpoint (`alsa:pipewire`) also succeeded and completed playback; an
invalid explicit ID failed without falling back. These were stream
initialization/completion checks, not listening or acoustic-switching tests.

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
Append an enumerated `host:device-id` argument to verify explicit device
initialization instead of the default output.

An automated low-amplitude Linux CPAL probe validated real 48 kHz stereo
device delivery, pause/seek/stop/restart, gapless/crossfade transitions and
duplicate-entry queue playback on one PipeWire/ALSA host. A later 30-minute
session added monitor capture of gapless/crossfade envelopes and resource
sampling. That run exposed one transient ~10 ms underrun accumulation during
queue/action churn. The cause was isolated to output-gate opening before the
ring was refilled after generation invalidation; the gate now waits for a
prebuffer sized from the observed device callback (minimum 1,024 frames). A
post-fix 30-minute rerun reported zero underruns with zero decoder/output
errors and stable memory/threads/fds. Low-amplitude real-backend runs with
device callbacks from 256 through 16,384 frames also reported zero underruns.
These are single-host observations, not perceptual or cross-platform guarantees;
see the canonical audio documentation for the device, conditions and exact
limitations.

## What does not exist yet

Rating UI, EQ,
frequency/spectrum visualizers, more codecs, folder watching, streaming,
accounts, installers and updaters remain future work. External reference
imports are intentionally rejected in canonical mode. Rating storage APIs exist
but are not yet exposed as a screen.

Optional Album membership is canonical, but Album owner transfer and Track
ownership transfer still require an Album destination; standalone ownership
changes therefore need an Album first. Cross-owner Track moves use the reviewed
ownership-transfer workflow. The import manager applies shared Artist/Album and
Album artwork to checked candidates, while Track artwork and lyrics remain
per-candidate; release-type selection, a featuring boolean and one-action
metadata templates beyond that scope remain future work. The TUI exposes
inspection and safe rename/create operations, not Album/Track artwork pickers,
rich import review, membership changes or ownership transfer. Lyrics must be
valid UTF-8 LRC/TXT; embedded audio tags are neither read nor written.
Native dialogs and file-drop are implemented; cross-platform runtime
verification remains limited to Linux/X11. Native picker parsing preserves
leading/trailing spaces, Unicode, and non-UTF-8 Unix names; Windows PowerShell
dialogs force UTF-8 stdout, and GUI picker selections retain the original
`PathBuf` rather than round-tripping through a lossy display string. Nothing
here makes network requests, collects telemetry, or requires an account.

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

Local interactive/runtime verification in this repository is Linux/X11 only.
The Windows and macOS CI jobs execute native tests/builds when GitHub
providers are available, but no Windows/macOS native dialog, window-manager,
terminal-emulator, audio device or filesystem crash-recovery behavior is
claimed from Linux testing. macOS x86_64 is compile-only. Cross-platform
runtime results are limited to what the CI platform actually reports.

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
