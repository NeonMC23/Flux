# Canonical Flux library foundation

This document records the canonical storage design and the application
integration state. The canonical engine is now the normal GUI/TUI runtime
backend: both front-ends open, create and switch canonical libraries through the
shared Controller. Legacy and portable schema-v3 formats are explicit migration
sources; normal front-end workflows no longer select or write them. The public
Controller still retains compatibility constructors for migration/tests, so
legacy retirement at the API level is not complete.

## Audit findings

- The original runtime had two persistent backends: the legacy app-data
  `library-index.json` (`store.rs`) and the portable schema-v3 segmented
  catalogue (`portable.rs`, `.flux/`). The integrated Controller now defaults
  to canonical (`open_default`, `new_canonical`, `switch_to_canonical`); the old
  constructors remain compatibility/migration APIs.
- Schema v3 already has stable `TrackId`/`ArtistId`, sparse overrides,
  immutable track segments, catalogue backups, generation checks, path
  validation and an explicit legacy migration (`migrate.rs`).
- The legacy scanner still uses `lofty` (`scan.rs`), but canonical startup,
  import inference, editing and migration never call it. The review importer
  now derives metadata only from filenames/folders.
- Tracks are physically addressed by absolute paths in the runtime library and
  queue data. Portable records are relative, but the legacy index and runtime
  queue are not guaranteed to be library-owned.
- AlbumId, playlist, rating/favorite and managed-lyrics runtime types now exist
  in the canonical engine. The GUI implements playlist management and a full
  lyrics library/editor/karaoke workflow; a dedicated rating screen is not
  implemented.
- The GUI/TUI Import Manager is review-first and copy-only. Reference imports
  are rejected in canonical mode; the old portable import tests still cover
  their compatibility behavior.
- Managed artwork decoding uses Iced's raster image widget plus the `image`
  crate with only PNG, JPEG and WebP codecs enabled. The crate is already the
  decoder behind Iced's optional image widget; enabling that existing optional
  integration adds no separate image framework or network access.

## Canonical layout

```text
FluxLibrary/
  library.flux
  catalog.flux
  catalog.flux.bak
  artists/artist_000001/
    artist.flux
    albums/album_000001/
      album.flux
      cover.png                 optional
      tracks/track_000001.flac
      tracks/track_000001.flux
      tracks/track_000001.lrc   optional
  playlists/playlist_000001.flux
  ratings.flux
  transactions/<txn>/journal.flux
  backups/catalog-gen-<n>.flux
```

All persisted managed paths are generated, relative and UTF-8; source
filenames are not persisted as dependencies. IDs are stable, nonzero `u64`
values. Directory names derive from IDs, so renaming an artist or album changes
metadata, not identity or paths. Deleted IDs remain below the preserved
counters and are never reused.

### Authority

- `library.flux` identifies the library, schema version and catalogue file.
- `catalog.flux` is authoritative for entity existence, ownership, album
  membership, canonical paths, counters, generation and the committed
  transaction ID. It is published last and is the commit record. Every
  catalogue-affecting transaction—including metadata-only artist, album, track,
  cover and rating changes—publishes `catalog.flux` with the next generation and
  its transaction ID, so the same recovery decision applies to all writes.
- `next_track_id`, `next_artist_id`, `next_album_id` and `next_playlist_id` must
  each be greater than the maximum ID present in their namespace. A counter at
  `u64::MAX`, or any counter that would reuse a loaded ID, is rejected at open
  before allocation. Valid retired-ID gaps remain legal; the counter only has to
  exceed the largest live ID.
- `artist.flux`, `album.flux`, `track.flux`, playlist files and `ratings.flux`
  are authoritative for their metadata. Redundant owner/album/credit values are
  validated against the catalogue; the catalogue wins for relationships and
  paths. A mismatch is rejected rather than silently repaired.
- Album track order lives in `album.flux`; every referenced track must exist and
  belong to that album. Track owner must equal album owner and must be one of the
  ordered credited artists. The first credit is the default owner at import time;
  an explicit review decision may select a later credit as physical owner.
  Featuring/collaboration is represented by additional ordered credits, never by
  duplicate audio.
- Audio tags are never read or written. Stored duration comes from Flux
  metadata (or is absent); content fingerprints are streamed bytes.
- Cover and lyrics are optional managed files with relative, validated names.
  A missing cover does not invalidate an album.

## Credits and physical ownership

A track stores an ordered `credited_artist_ids` list plus exactly one
`owner_artist_id`. The owner must appear in the credits and must match the album
owner because the album directory is the physical container. The owner need not
be first after an explicit ownership choice. Artist discographies resolve both
owned and credited tracks from the same TrackId without copying audio.

Canonical import inference creates one provisional credit from the folder/file
name. The GUI review can add existing artists, add named credits, remove/reorder
credits, and choose the physical owner. Names are matched to existing identities
only when exactly one exact-name identity exists; ambiguous names require an
explicit selection. Tag data is never consulted.

## Ownership transfer

`plan_ownership_transfer` is read-only. It requires the destination artist to be
credited, requires an existing destination album owned by that artist or an
explicit new album title, and rejects collisions before any write. Execution
copies the audio and managed lyrics to new canonical paths, updates both album
files and the track record, and publishes the catalogue last. Old audio/track
records are listed as journal-owned cleanup and are removed only after the
catalogue commits. If cleanup is interrupted, the committed journal removes the
old files on the next open. TrackId, ordered credits, album order, lyrics,
ratings, playlists and queue references are preserved. A retry against the
committed state is an explicit no-op.

## Album ownership and whole-album transfer

Album ownership is the physical directory owner. Every track owner must equal
the album owner and must appear in its ordered credits. `albums_owned_by` and
`albums_credited_for` expose the distinction between owned releases and albums
containing only collaborative/credited tracks.

`plan_album_transfer` creates a generation-aware immutable plan for moving a
whole album to a new AlbumId-preserving location under a destination artist.
The source album remains one identity: AlbumId, TrackIds, track order, title,
release type, cover, lyrics, ratings, playlists and queue references are
preserved. The destination artist becomes physical owner of every track. If the
destination is already credited, existing credits are untouched. If not, every
such track requires an explicit per-track credit-addition decision; unresolved
credit additions block the whole operation.

Merging into a different existing AlbumId is intentionally unsupported because
one AlbumId denotes one release; silently merging would violate stable album
identity. The operation therefore rejects an existing destination directory and
uses a new location under the destination artist. Destination collisions,
missing cover/lyrics, stale generation, changed metadata, changed ratings,
changed credits and changed resource fingerprints are rejected before commit.

`execute_album_transfer` stages the album file, cover, every track `.flux`, audio
and lyrics, verifies copies, publishes the catalogue last, and removes the old
album-owned files only as journal cleanup. It is one transaction, not a loop of
single-track transfers. Failure injection covers the same 18 boundaries and real
child-process aborts before journal, after catalogue publication and during
cleanup; recovery is complete-old or complete-new and idempotent.

The GUI exposes **Transfer album…** from the selected album view. The review
shows source/destination owner, track order, credits, cover/lyrics/rating and
playlist impact, credit blockers and explicit credit decisions. Cancellation
writes nothing; stale plans are rejected. The TUI shows album ownership and
directs album transfer to the GUI rather than offering an unsafe shortcut.

## Independent album lifecycle

Albums are first-class catalogue entities even with zero tracks. `create_album`
writes `album.flux`, registers the stable AlbumId in `catalog.flux`, and never
creates placeholder audio. Album title/release type can be edited without
changing AlbumId or paths. Duplicate owner/title combinations are rejected.

The GUI album list is catalogue-derived, not track-derived, so empty albums are
visible. Selecting one opens a dedicated album detail view with AlbumId, owner,
release type, track count, ordered track list, cover status and actions for
metadata, artwork, transfer, reorder and empty-album deletion. Empty albums show
a deliberate empty state rather than disappearing. The TUI `E` command lists all
catalogue albums with owner, track count and cover availability; `D` shows the
selected track's album.

### Catalogue search, filtering and batch selection

The GUI search field performs a case-insensitive partial match over canonical
artist names, album titles and track titles. Results are grouped by entity type
with stable IDs and direct navigation: artist and album results open their
canonical group/detail, and track results offer both playback and a stable
location in the track list. Empty results show a clear state and a Clear action.
Search performs no catalogue writes and does not use audio tags.

Search results are cached per query and catalogue generation to avoid rebuilding
the same match set on every repaint. Results are limited to 20 items per entity
type; the empty/partial query behavior and grouped type indicators stay
deterministic. Artist and album renames explicitly invalidate the cache.

Track views support these filters:

- favorites only;
- managed lyrics present, absent, or any;
- managed artwork present, absent, or any (based on the album cover reference);
- release type: album, EP, single, compilation, other, or any.

Sorting options are catalogue order, title, artist, album, track/disc number and
favorites-first. Ties fall back to stable TrackId order. Album detail always
preserves canonical album order regardless of the general sort setting. Reset
filters clears every filter and returns sorting to catalogue order. Filters are
not applied to global search results.

Track multi-selection uses stable TrackIds, never row indices. Tracks have an
individual selection control; Select all visible selects the TrackIds in the
current page or the currently displayed search-track results, and Clear removes
the selection. Changing search, filters, view or group clears the selection;
sorting keeps the same stable IDs selected. Navigating away cannot retarget a
batch to different tracks because operations carry IDs and revalidate them.

Batch favorites/unfavorites write all selected tracks in one ratings transaction
and preserve existing rating values. Playlist actions open an explicit review
showing the affected count, target playlist and selected tracks. Creating a
playlist from the selection or appending to an existing playlist uses one
canonical playlist transaction; existing order is retained, selected tracks
append in selection order, and duplicate entries are preserved. Missing or stale
TrackIds and missing playlists reject the whole operation without writes and
leave the review open with an actionable error. Successful playlist/favorite
operations preserve queue entries, current playback and all unrelated canonical
references.

Batch queue mutation and batch album moves are intentionally not exposed. The
former would require a separate atomic queue-policy decision, and the latter
requires a reviewed ordering/ownership plan across every selected track. They
are not simulated through repeated independent writes.

### Playlist management

The canonical playlist model has a stable `PlaylistId`, a name and an ordered
`Vec<TrackId>`. It intentionally has no description, artwork or timestamp
field; the GUI does not invent those. Playlist names are not unique: duplicate
names are allowed and identity remains the stable PlaylistId, so no rename or
create silently overwrites another playlist. Duplicate TrackIds are allowed and remain
meaningful by position. There are no separate playlist-entry IDs: operations
that target one or more duplicate entries capture the complete reviewed
playlist order and reject the plan if that order changed before publication.

The GUI has a dedicated Playlists library with name, entry count and a short
track preview, plus empty/create states. A playlist detail view provides:

- rename with a stable PlaylistId and preserved contents;
- ordered entries with title, artist, album, duration when known and current
  playback indication;
- adding one/multiple selected library tracks through the existing reviewed
  batch playlist action;
- single or multi-entry removal by reviewed positions;
- arrow reordering and drag-handle reordering with insertion feedback;
- explicit clear and delete confirmations;
- play from the beginning, play a specific entry, shuffle play, add the whole
  playlist to the queue and add selected entries to the queue.

Removing entries or clearing a playlist never deletes canonical tracks, audio,
albums, artwork or artist identities. Deleting a playlist removes only its
catalogue entry and playlist file through journal cleanup. Rename, content edits,
clear and delete are one canonical transaction each; stale reviewed order/name
snapshots are rejected. Delete/open recovery leaves either the complete old or
complete new catalogue, and unrelated queue entries/playback remain valid.
Playlist operations never change album track order.

Playlist playback uses a queue made from canonical TrackIds, including duplicate
entries. Play/shuffle replace the queue explicitly; shuffle does not mutate the
canonical playlist order. Enqueue operations append and preserve existing queue
order/current playback. Editing canonical playlist contents does not touch the
queue. Missing tracks are shown as missing entries and can be removed; playback
or enqueue operations that require missing tracks fail before changing the queue.

Drag reorder is implemented inside the playlist detail view using stable
reviewed positions. Dragging library tracks or selections into a playlist is not
implemented; use the selection-bar/album/search playlist action and explicit
review instead. A drag snapshot captured at drag start is revalidated before the
one-transaction reorder.

The TUI does not expose playlist management; its help and documentation direct
these operations to the GUI.

### Advanced queue and playback modes

Queue entries already have stable `QueueEntryId` identities, so duplicate
TrackIds remain distinct and the active entry is never confused with another
occurrence. Existing operations support append, insert-next, explicit reorder,
single removal and duplicate-safe selection. Queue entry identity and TrackId
are different concepts.

Playback modes are a combination of the existing repeat model and controller
shuffle state:

- Normal;
- Repeat One;
- Repeat All;
- Shuffle;
- Shuffle + Repeat All.

`R` in the GUI/TUI cycles all five modes. Repeat One disables shuffle and
replays the current entry when it ends. Shuffle builds a seeded traversal of
queue-entry IDs with the current entry first, avoids adjacent duplicate TrackIds
where an alternative exists, and does not mutate canonical playlist order.
Queue changes retain the existing shuffle order and append newly added entry IDs;
removed entries are dropped. Explicit skips use the traversal, while Repeat All
wraps and Repeat Off stops at the end. Previous restarts the current track when
the actual playback position is at least three seconds; otherwise it moves to
the previous queue/shuffle entry.

For canonical libraries, the queue file persists ordered entries, stable
entry IDs, current entry, repeat mode, shuffle flag, playback position and
whether playback was active. Restoration always installs the queue paused and
does not start audio implicitly. Position is applied only when the user presses
Play. Queue files without the optional playback fields remain readable with
defaults. Saves remain atomic and malformed files are preserved. Non-canonical
legacy/portable backends retain their existing queue workflow.

### Audio engine, gapless handoff and crossfade

The audio crate keeps policy and mechanism separate. [`Controller`] decides
which stable `QueueEntryId` should play; `flux-audio::engine` owns decoding,
format conversion, the bounded sample ring and device output. Decoding and file
I/O run on one worker thread. The CPAL callback only pops from a lock-free SPSC
ring, emits silence on underrun, and publishes the oscilloscope tap; it performs
no file I/O, locking or allocation. The output stream stays open across loads
and across natural handoffs.

The worker tracks: the audible/current source; at most one prepared decoder; and
— once the current decoder reaches EOF — one incoming source whose decoder has
already been installed in the ring but whose queue entry becomes current only
at the output boundary. Both current and prepared sources carry `TrackId`,
`QueueEntryId`, path and decoded stream facts. A prepared decoder is consumed
only when the later `Load` or natural handoff still matches both identity and
path. Duplicate tracks therefore remain distinct queue entries.

For a natural transition:

1. The active decoder reaches EOF and its converter tail is pushed.
2. The prepared decoder is installed into the same generation.
3. Its frames are appended directly after the old source's final output frame.
4. The worker records the boundary as the number of output frames pushed for
   the generation.
5. Once the callback reports that the device consumed up to that boundary, the
   worker promotes the incoming source, resets source-relative position to zero,
   and emits exactly one `EngineEvent::Transitioned` with both queue entries,
   track IDs, generation, output boundary and new stream facts.
6. The Controller updates the queue's current entry, engine ledger, history
   boundary and UI state. It does not issue another load for that transition.

Load, seek, stop and restart each bump the output generation and make the
callback flush stale buffered samples. A natural handoff deliberately does not
bump it: the old tail and new head share one output sequence, so seek/stop/
replacement invalidate audio explicitly while adjacent tracks remain
sample-contiguous. The callback still rejects samples from older generations.

Natural transitions can optionally use equal-power crossfade. The engine API is
`set_crossfade_ms(u64)` / `crossfade_ms()`; the Controller exposes the
enabled/duration pair through `crossfade_enabled()`,
`crossfade_duration_ms()`, `set_crossfade_enabled()` and
`set_crossfade_duration_ms()`. The default is disabled with a remembered
duration of 3000 ms. Values are clamped to `0..=5_000` ms. A change applies to
the running Controller without reloading the current source, but affects only
sources loaded afterwards; it does not retroactively resize an already-loaded
source's tail schedule.

For crossfade, the engine keeps a bounded tail of outgoing output frames. The
target overlap is:

- `fade_frames = duration_ms * output_rate / 1000`;
- at most 1 second of outgoing audio is normally pushed before holding begins;
- `overlap = min(fade_frames, outgoing_output_frames - 1 second)`;
- crossfade is only attempted when the outgoing duration is known, the overlap
  is non-zero, and the prepared incoming duration is known to contain at least
  the overlap. Otherwise the verified gapless/fallback path is used.

At EOF, the worker decodes exactly the incoming head needed for the overlap,
matches it with the held outgoing tail, and mixes in the worker. The envelope is
equal-power:

```text
t = frame / (overlap_frames - 1)
outgoing_gain = cos(t * pi / 2)
incoming_gain = sin(t * pi / 2)
mixed = clamp(outgoing * outgoing_gain + incoming * incoming_gain, -1, 1)
```

For a one-frame overlap the incoming frame wins (outgoing gain 0, incoming gain
1). Endpoints are exactly outgoing-only and incoming-only. Correlated sources
can exceed unity under equal-power gains, so the mixed sample is clamped to the
nominal interleaved f32 output range; the tests cover this headroom policy.

The Controller-visible `Transitioned` event is emitted at the **start** of the
overlap, at the first mixed output frame. The incoming `QueueEntryId` becomes
current there, its source-relative position is zero, and lyrics/artwork/history
follow the incoming entry. The outgoing tail continues fading out after that
event but is no longer the visible/current source. Crossfade completion emits no
second event. A queue mutation before the start event invalidates the queued
mixed audio, flushes that unpromoted generation and reopens the audible current
source at its reported position before preparing the newly authorized entry;
this recovery path may contain silence and is not advertised as gapless.
Seek, stop, explicit load, cancellation and replacement likewise invalidate an
obsolete overlap.

### Crossfade settings

The GUI exposes **Settings** in the sidebar (keyboard shortcut `K`) with an
enable/disable toggler and an equal-power duration slider from 0 to 5 seconds in
0.1-second steps. The slider commits on release rather than saving on every
pointer movement. While disabled, the remembered duration is shown as a
secondary value.

The TUI uses `K` to toggle crossfade and `[` / `]` to decrease or increase the
duration by 250 ms. The now-playing line shows `Xfade off` or the active
duration, and the help bar lists both controls. The TUI uses the same
Controller methods and the same settings file as the GUI.

Settings are loaded by `Controller::open_default` before playback starts. The
shared JSON file is located at `<platform config>/flux/settings.json`; on Linux
that is normally `$XDG_CONFIG_HOME/flux/settings.json` (falling back to
`~/.config/flux/settings.json`). The current schema is:

```json
{
  "version": 1,
  "crossfade": {
    "enabled": false,
    "duration_ms": 3000
  }
}
```

Missing and empty files use defaults. Unknown root/crossfade fields are
preserved when crossfade is saved. Invalid durations are clamped or replaced by
the default; malformed or unreadable files are preserved byte-for-byte, reported
to the user, and write-protected for the session, matching the queue/history
persistence convention. Saves use a sibling temporary file, flush, and atomic
rename. A save failure leaves the previous settings file intact, applies the
runtime setting for the current session, and reports the failure instead of
claiming it was persisted. No background settings watcher or multi-process
synchronization is used; GUI/TUI launches read the same file independently.

The output ring remains the same bounded SPSC ring and the callback is unchanged.
During an overlap there are at most two decoders: the outgoing decoder until its
tail has been extracted and the prepared incoming decoder. The retained tail,
predecoded incoming head, mixed pending audio and ring are all bounded by the
5-second maximum plus normal decode chunks. No PCM accumulates without a bound,
and the outgoing decoder is released once its tail has been mixed.

At the engine's current support level there is only one prepared successor.
An extremely short successor may end before the next preload arrives and falls
through to the normal `TrackEnded` load path. Codec/container priming and
intentional source silence remain part of the decoded material; crossfade does
not fabricate or remove them.

The Controller preloads the intended successor for Normal, Repeat One, Repeat
All, Shuffle and Shuffle + Repeat All. A queue change or mode change replaces or
cancels the obsolete preparation; an unchanged target is not reopened. If the
successor was already installed in the ring but has not yet reached the output
boundary, replacement/cancellation flushes that unpromoted generation and
reopens the audible current source at its reported position before preparing
the newly authorized entry. This invalidation path is deliberately not
advertised as gapless. The session, current entry and `to_entry` are checked
before the Controller adopts a transition event, so a stale event from an
earlier user action cannot overwrite the current model. If a queue mutation
wins a boundary race after the event, the Controller stops the pipeline and
reconciles to the authoritative current entry rather than silently keeping the
unauthorized successor.

Playback position remains source-relative: the engine subtracts the active
source's output-frame start from consumed frames. Preparation and buffered
decode never advance visible position. Pause freezes consumption; seeking
targets the current audible source and invalidates an unpromoted handoff.
Meaningful-play history is keyed by queue entry and is reset only when the
Controller accepts a transition, so preloading creates no play and duplicate
tracks count as distinct occurrences.

If no prepared successor exists when EOF is reached, the old path remains:
`TrackEnded` fires only after the ring drains, then the Controller advances and
performs an explicit load. That fallback may contain silence and is not
presented as gapless. Preload failure never interrupts the active source; the
actual load reports the decode/IO error when reached.

Format changes are handled by the existing `FormatConverter` (sample-rate
conversion, mono downmix and channel mapping), so the successor can have a
different rate or channel count without changing the output configuration or
device stream.

The prepared window is one entry. The Controller can authorize the successor
after it accepts the preceding transition, so an extremely short successor may
end before the following preload arrives and falls back to `TrackEnded`. Real
hardware gapless behavior has not been measured; deterministic frame continuity
is verified with synthetic sources and the null backend.

[`Controller`]: ../crates/flux-app/src/lib.rs

### Audio reliability audit, diagnostics and stress coverage

The audio pipeline has been audited against the existing null backend and
core-level conversion code. The following table distinguishes what is covered
from what remains structurally or physically unverified.

| Area | Status |
|---|---|
| Decoder lifecycle, prepared cancellation, gapless/crossfade handoff | Confirmed by deterministic engine tests |
| Queue authorization, duplicate `QueueEntryId`, transition identity | Confirmed by Controller/engine tests |
| Command burst handling and preload scheduling | Confirmed defect fixed; regression tested |
| Real-time callback content (ring pop, gain, silence) | Audited structurally, exercised through `NullOutput`, and exercised through a real CPAL stream on one Linux host |
| CPAL stream construction, sample formats, error propagation | Real 48 kHz stereo stream validated on one Linux/PipeWire host; fatal/notice classification tested deterministically; no physical hotplug validation |
| Device callback-size compatibility | Adaptive gate verified with simulated and real callbacks from 256 to 16,384 frames on one Linux host; sizes above that, hardware callback-size changes and callbacks larger than the output ring are unverified |
| Device loss and bounded recovery | Default-output recovery implemented and tested with injected stream failures, blocked opens, repeated loss, pause/stop/reselection, configuration change and bounded exhaustion; no physical unplug or audio-server restart tested; no explicit device selection |
| Hardware underrun, latency and long-duration CPU/memory | Real callback-size runs, a ~60 s probe and post-fix 5/30-minute runs reported zero underruns; latency and multi-hour CPU/memory not measured |
| Non-finite decoded samples | Confirmed defect fixed at conversion boundary |

Confirmed fixes from this audit:

- The CPAL callback previously grew a scratch `Vec` from inside the
  real-time callback when a device buffer was larger than the prior one.
  It now allocates a frame-aligned scratch window before starting the stream
  and reuses it, splitting larger device buffers into fixed-size chunks.
- A fast decoder could reach EOF before a queued `Load`/`Play`/`Preload`
  burst was processed, committing to a fallback transition even though the
  intended successor was already waiting in the command channel. The worker
  now drains every queued command before pumping audio.
- A stale queued `Seek` could overwrite the immediate position of a newer
  seek after the second command was issued but before the worker processed
  it. The newest requested load/seek session is published atomically, older
  seeks are ignored, and position is locked to the new base until the
  callback flushes/acknowledges the generation.
- Non-finite decoded samples could propagate through interpolation and
  crossfade into the output backend. `FormatConverter` now converts
  NaN/infinity to zero at the conversion boundary while preserving finite
  values and existing clipping/headroom semantics.
- Opening the output gate immediately after a new generation could expose the
  first CPAL callback to an empty ring. Load/seek/preload-invalidation paths
  now keep the gate closed until the callback has acknowledged the generation,
  at least one full device-callback request has been observed, and the ring
  holds `max(1,024, full_callback_frames)` output frames (or all remaining
  source audio is queued). The adaptive part prevents a callback larger than
  1,024 frames from starving immediately after the gate opens. The fix was
  triggered by the underrun investigation below.

`EngineHandle::diagnostics()` (also exposed as
`Controller::audio_diagnostics()`) returns a local, lock-free snapshot:
callback fill count, last/max full device-callback frames, gate re-prebuffer
events and frames, requested/delivered/underrun frames, current ring occupancy
and capacity, generation, pushed/played frames, gapless/crossfade counts,
preload failures, decoder errors, output errors, output failure/notice state
and output rate/channels. The
callback increments bounded atomics only; there is no logging, allocation,
locking, telemetry or network access in the callback. `callbacks` counts
`FillCallback` invocations, so a very large CPAL device buffer filled in
multiple chunks counts multiple times. `device_callback_frames` and
`max_device_callback_frames` are the full device request before that chunking.
Ring occupancy is the most recent worker/callback observation and is
diagnostic, not a scheduling barrier. Callback duration is intentionally not
measured: doing so safely would require additional per-callback clock reads and
was not justified.

`underrun_frames` is the sum, while the output gate is open, of requested minus
actually popped output frames. It does not count intentional silence while the
gate is closed for pause/stop, nor the generation-flush callback after
load/seek/stop. It does count gate-open startup/rewind starvation, end-of-stream
gate-open shortfall, and CPAL chunk-fill shortfalls. One periodic diagnostic
sample can aggregate multiple callback events; the counter does not identify
which callback or whether the shortfall was transient startup versus
established playback. This limitation motivated the gate-prebuffer correction.
`gate_reprebuffers` and `gate_reprebuffer_frames` separately record output
paused intentionally when a variable-size device callback grows beyond what
the ring can serve; those frames are not underruns. A stable callback size does
not produce them.

Deterministic stress coverage:

- 2,000 gapless transitions with two short WAVs under Repeat All.
- 100-transition crossfade stress with 2 s WAVs under Repeat All; at least
  96 overlaps are required, with a small bounded fallback allowance under
  scheduler contention.
- 100 gapless Repeat One transitions, 40 Shuffle + Repeat All transitions and
  100 transitions with duplicate queue entries for the same TrackId.
- Alternating gapless/crossfade configuration during a running session.
- 50 consecutive failed preloads while the active source continues.
- Rapid bursts of pause/resume/seek/next/previous/direct selection, queue
  add/remove/reorder and crossfade setting changes.
- Existing coverage for truncated successors, decoder failures, seek/stop/
  load cancellation, pause during overlap, empty/short sources and stale
  transition events.

The stress harness is frame-driven through `NullOutput`; it paces drain
operations using observed ring occupancy and acknowledges generations with
zero-frame fills. It asserts zero underrun frames, zero decoder/preload errors
where applicable, bounded transition counts, bounded generation changes, and
correct current-entry/engine-ledger agreement. It is not a wall-clock
real-time simulation and does not prove multi-hour hardware stability.

#### Linux CPAL hardware validation (2026-11-04)

A safely low-amplitude hardware probe was run on one Linux host:

- Nobara Linux 44, kernel `7.3.0-203.nobara.fc44.x86_64`, PipeWire 1.6.9
  with PulseAudio 15 compatibility, Rust 1.99.0 and CPAL 0.18.
- Default output: Built-in Audio Analog Stereo, Realtek ALC295 on HDA Intel
  PCH, front-left/front-right. PipeWire exposed the sink as `s32le 2ch
  48000Hz`; the application stream was `float32le 2ch 48000Hz`.
- Existing PipeWire sink inputs were active during the probe. The Flux stream
  was mixed normally into the default hardware sink; no device was taken
  exclusively, no system audio setting was changed, and no existing stream was
  stopped.
- Two generated 3-second, 44.1 kHz stereo WAV files with 220 Hz and 660 Hz
  tones were used. Source peak amplitude was 0.05 and the Flux controller
  volume was 0.02, keeping the probe intentionally quiet while still exercising
  real resampling, conversion, buffering and device delivery.
- PipeWire showed the stream as `PipeWire ALSA [hardware_probe]` connected to
  the default hardware sink.

Observed over the approximately 60-second probe:

- 5,620 callback fill invocations and 2,852,864 delivered frames at 48 kHz
  (about 59.4 seconds of device frames).
- 2,877,440 requested frames; **0 underrun frames**.
- 0 decoder errors, 0 preload failures and 0 output-stream errors.
- Pause retained `490.666667 ms`; resume/seek and stop/restart produced the
  expected position and generation changes.
- Gapless mode produced gapless transitions with no reported underrun.
- Crossfade settings 0/500/1000/3000/5000 ms were exercised. Steady-state
  extended playback with 500 ms produced 8 consecutive crossfades. A few
  phase-boundary transitions around stop/reconfigure fell back to gapless; no
  underrun or false transition was observed.
- Repeat One, Shuffle + Repeat All and duplicate queue-entry playback ran on
  the real device. Duplicate entries retained distinct `QueueEntryId`s.
- A rapid pause/resume/seek/next/previous/direct-selection/crossfade burst
  recovered to `Playing` with no reported underrun or decoder error.
- Process RSS was approximately 18.7 MiB after startup, increasing to about
  19.9 MiB after roughly 49 seconds; process CPU was roughly 4–6% in that
  sample. These are single-host observations, not guarantees.

This validates the real CPAL callback/device path and transition bookkeeping
under the stated conditions. It does **not** provide perceptual/audible
verification: the probe was deliberately quiet, the sink monitor contained
other active streams, and no isolated acoustic capture was performed. Device
unplug/replug, audio-server restart, device format changes, exclusive-access
behavior and multi-hour hardware stability remain untested.

#### Extended Linux quality and long-run probe (2026-11-04)

A second, non-exclusive probe was run on the same host and default
PipeWire/ALSA sink. Sources were two 4-second, 44.1 kHz stereo WAVs with
15,317 Hz and 18,311 Hz tones at source peak 0.05. The high frequencies kept the
test distinguishable from unrelated stream content while remaining low-level.
No system audio configuration was changed.

Loopback capture used `pw-record` against the default sink's monitor source
(explicit source serial `423`), 48 kHz stereo float32. This is an
output-server monitor capture, not a microphone or speaker/headphone acoustic
measurement.

- A 16-second capture with gapless transitions (crossfade 0 ms) showed
  three gapless handoffs. A 5 kHz high-pass RMS envelope across each boundary
  had a minimum around 0.00326 versus a median near 0.00366 (about 89%),
  with no silent interval at 20 ms resolution. Left/right high-band RMS ratio
  was 1.0001. Application diagnostics reported 3 gapless transitions,
  0 underruns and 0 errors.
- Capture analysis at 500 ms, 1,000 ms and 3,000 ms crossfade settings
  measured overlap regions of approximately 0.405 s, 0.811 s and 2.424 s
  respectively (threshold/smoothing limited; the 3,000 ms case was shortened
  by the 4-second sources and 1-second prebuffer). Normalizing against the
  steady A/B levels, the median `outgoing_gain² + incoming_gain²` was
  approximately 1.000 in all cases, consistent with the equal-power mixer.
  Endpoints were outgoing-only and incoming-only. A 3,000 ms first handoff
  was followed by gapless fallback on the very short remaining source, as
  documented by the eligibility rules.
- The loopback captures contained other active system audio. They are useful
  for envelope/continuity checks but are not isolated acoustic measurements.
  One capture contained near-full-scale samples from that unrelated mix;
  no clipping or non-finite sample was attributable to the low-level Flux
  probe.
- A focused hardware pause/resume test ran 200 cycles (20–60 ms pauses) with
  crossfade enabled: 0 underruns and 0 decoder/output errors.

The 30-minute long-run session used crossfade/gapless/Repeat One/Shuffle All
changes, queue append/removal, pause/resume, seek, next/previous and direct
track selection at 30-second intervals, with diagnostic, RSS, thread and fd
snapshots every 30 seconds.

- Elapsed: 1,800.00 s.
- RSS: 18,108 KiB shortly after startup, 18,340 KiB at 10 minutes,
  18,384 KiB at 20 minutes, 18,504 KiB at 30 minutes.
- Threads: stable at 6. File descriptors: 27–29.
- Process CPU after warm-up: approximately 3.7–3.8% in sampled observations.
- Callbacks: 168,752. Requested frames: 86,401,024. Delivered: 82,034,704.
- Underruns: **496 frames total (≈10.3 ms)**, all accumulated in one observed
  reporting interval; the counter did not grow after that interval.
- Generations: 44. Gapless transitions: 190. Crossfades: 259.
- Preload failures: 0. Decoder errors: 0. Output errors: 0.
- Final Controller state: `Playing`, current TrackId(2), QueueEntryId(2).

The single underrun occurred around 1,470 s during the queue/action-churn
portion, after earlier steady gapless/crossfade periods had zero. This was
subsequently investigated and traced to output-gate opening before the ring
had been refilled after a generation change. The exact 496-frame event was not
replayed callback-for-callback, but the mechanism was reproduced deterministically
on the real device as 512-frame shortfalls during rapid queue append/remove
churn. See the underrun investigation below. The session was not a perceptual
listening test, and the process was stopped after 30 minutes rather than
extrapolated to multi-hour or cross-platform guarantees.

#### Underrun investigation and gate-prebuffer correction

Controlled hardware experiments isolated the failure:

- 30-second continuous restart, direct-selection, pause/resume, next, previous,
  seek and crossfade-change workloads each reported zero underruns.
- A 30-second rapid queue append/remove workload reported **2,048 underrun
  frames** (four 512-frame shortfalls). Diagnostic samples showed generation
  changes, an empty ring, and playback still logically `Playing`.
- A detailed 8-second queue-churn run reproduced shortfalls with generation
  bumps while the ring was zero; the underrun appeared on the callback
  immediately following a preload-invalidation flush.

Root cause: queue mutations can change the authorized successor while an
incoming source is already installed. The engine must invalidate that installed
handoff, flush the generation and reopen the audible source. The old recovery
path reopened the output gate as soon as playback intent was set, even though
the generation flush had just emptied the ring. The next CPAL callback could
therefore request a full buffer with only a few or zero frames available. This
is the same startup/gate-opening class counted by `underrun_frames`.

Correction: load, seek and preload-invalidation paths now close the output gate.
The worker opens it only after the generation is acknowledged and either:

- at least 1,024 output frames are buffered; or
- all remaining source audio has been queued (short/end-of-stream sources).

Play with an already-prefilled ring still opens immediately; resume latency is
not changed for a normal pause. A regression test verifies that Play does not
open the gate before prebuffering and that the gate opens after the ring has
been primed.

Post-fix hardware results on the same device:

- 5-minute baseline: 0 underruns, 85 crossfades, 28,131 fills, exact requested
  and delivered frame counts.
- 30-second rapid queue append/remove: **0 underruns** (previously 2,048).
- 30-minute combined queue/mode/pause/seek/crossfade workload: **0 underruns**;
  184 gapless transitions, 257 crossfades, 166,025 fills, 43 generations,
  0 preload/decoder/output errors. RSS was 17,904 KiB shortly after startup and
  11,336 KiB at the end; threads stayed at 6 and fds at 26–29.

The original single 496-frame event was therefore explained as the same
gate-open starvation class with high confidence. Because the original metric
did not tag event boundaries, the exact callback cannot be reconstructed from
the old logs; the post-fix workload is the decisive validation.

#### Adaptive gate prebuffer and callback-size compatibility (2026-11-04)

A follow-up audit checked whether the fixed 1,024-frame gate threshold was safe
for every device callback. It was not guaranteed to be. CPAL 0.18's
`OutputCallbackInfo` exposes timestamps but no buffer length; the only reliable
callback-size signal is the data slice passed to the stream callback. A CPAL
device can produce callbacks larger than 1,024 frames (for example through
`BufferSize::Fixed`), and a callback larger than the prebuffer could consume
the entire ring immediately after the gate opened. The engine's internal
4,096-frame chunking does not remove this: all chunks in one device callback
run before the worker gets another turn.

Correction:

- `FillCallback::note_device_request` now reports the full device-callback
  frame count before the CPAL wrapper splits it into 4,096-frame internal
  chunks. `CpalOutput` calls it once per device callback; `NullControl` reports
  a simulated callback size for tests. The callback implementation stores the
  last and maximum values in atomics and reads the lock-free ring occupancy.
- The worker requires a generation acknowledgment and at least one observed
  device callback before opening the gate. The required prebuffer is
  `max(1,024, max_observed_device_callback_frames)`. The first callback after
  startup is therefore silent until its true demand is known; this avoids a
  race where the worker opens the gate at 1,024 frames just before a larger
  first callback arrives.
- If a variable-size backend later grows beyond the size the gate was prepared
  for and the ring cannot serve it, the callback closes the gate before
  consuming. The worker refills to the new maximum and reopens. The skipped
  callback is counted in `gate_reprebuffers`/`gate_reprebuffer_frames`, not in
  `underrun_frames`. With a stable callback size this path is not taken.
- Short sources still open once all remaining audio is queued. Pause/resume
  with an already prebuffered ring still opens immediately. Generation
  invalidation additionally checks that the new generation has been
  acknowledged before Play can reopen the gate, preventing stale ring
  occupancy from satisfying the threshold.
- The prebuffer is bounded by the normal roughly two-second output ring. A
  device callback larger than that ring cannot be prebuffered and is not
  supported; no unbounded growth was added.

Hardware callback-size probe on the same Linux/PipeWire host used a temporary,
uncommitted override of CPAL's `BufferSize::Fixed` in the Flux engine. The
override was reverted and is not part of the production configuration. A
6-second, 44.1 kHz stereo source at low amplitude was resampled to 48 kHz and
played through the real backend; diagnostics were sampled after 2, 4 and 6
seconds.

| Requested CPAL fixed size | Observed full callback | Internal fill chunks | Final requested frames | Final delivered frames | Underruns |
|---|---:|---:|---:|---:|---:|
| default | 512 | 1 | 288,768 | 286,720 | 0 |
| 256 | 256 | 1 | 288,256 | 286,720 | 0 |
| 512 | 512 | 1 | 288,768 | 286,720 | 0 |
| 1,024 | 1,024 | 1 | 289,792 | 286,720 | 0 |
| 2,048 | 2,048 | 1 | 290,816 | 286,720 | 0 |
| 4,096 | 4,096 | 1 | 294,912 | 286,720 | 0 |
| 8,192 | 8,192 | 2 | 303,104 | 286,720 | 0 |
| 16,384 | 16,384 | 4 | 311,296 | 278,528 | 0 |

Each run reported zero decoder, preload and output errors, one generation and
no gate re-prebuffers. The 16,384-frame run reached about 5.8 seconds of
delivered audio at the final sample, including end-of-source silence. Ring
occupancy at the exact gate-opening instant was not separately instrumented;
post-startup diagnostic samples were near capacity for every run. Additional
runs with 4,096- and 16,384-frame callbacks performed a seek, pause/resume and
load/replay sequence; they completed with generation changes 1→2→3 and zero
underruns and zero gate re-prebuffers at every sampled stage. A separate direct-CPAL probe run
before the engine tests observed a 1,024-frame default callback on the same
host; the default callback size is therefore not assumed constant across runs
or server configurations. Sizes above 16,384 frames, variable callback sizes
on hardware, and devices whose callback exceeds the output ring were not
exercised. These are single-host backend observations, not perceptual or
cross-platform guarantees.

#### Audio device lifecycle and bounded recovery (2026-11-04)

A lifecycle audit found that a runtime CPAL failure previously produced only a
string error event: the worker retained the dead stream, the Controller stopped
playback and cleared its engine ledger, but `AudioStatus` still said `Ready`.
Later Play commands could therefore appear to be accepted by an unusable
stream. CPAL 0.18.2 also distinguishes conditions that must not be treated as
stream death.

Implemented semantics:

- Backend reports are classified. `Xrun` and `RealtimeDenied` are audio-quality
  conditions and are not fatal; `DeviceChanged` is a nonfatal, automatically
  rerouted route change and is surfaced as `EngineEvent::OutputNotice`. Other
  errors (including `DeviceNotAvailable`, `StreamInvalidated`,
  `HostUnavailable`, configuration errors and unclassified backend failures)
  are fatal.
- On the first fatal report the callback side closes the output gate and sets
  an atomic failure flag, so it cannot reopen on the dead stream. Repeated
  fatal reports increment `output_errors` but do not flood the event channel or
  restart recovery.
- Flux's normal default-device entry point (`Controller::open_default`) enables
  bounded recovery. Caller-supplied engines keep the prior explicit-failure
  behavior. No device-selection preference exists; recovery always opens the
  current default output device.
- A fatal output failure preserves the `Player` queue, current `TrackId` and
  stable `QueueEntryId`, and playback intent (`Playing`/`Paused`/`Stopped`)
  while `AudioStatus::Recovering` separately reports that no valid output is
  currently audible. The old engine is stopped and dropped, releasing its
  stream, ring, callbacks and worker. A background thread opens a fresh engine
  so a slow device open cannot block the UI event loop.
- Attempts are bounded to four, with 250 ms, 750 ms and 1,500 ms backoff between
  failures. Each attempt creates a fresh output configuration, ring, decoder
  pipeline and generation; no callback or buffered sample from the failed
  stream is reused. The replacement stream observes its own callback demand and
  waits for its own prebuffer before the gate can open.
- On success the latest engine is installed with the current volume/crossfade
  settings, the queue selection is reloaded, and the captured delivered-frame
  position is reapplied through Seek before Play when playback intent is
  `Playing`. Paused intent loads the track paused at the captured position;
  Stopped intent loads nothing. The recovery gap is not bridged: playback does
  not claim gapless continuity across a device loss.
- A pause or stop issued during recovery changes the preserved intent, so a
  later successful recovery does not start unexpected audio. Track selection
  during recovery resets the restore point to zero; queue edits preserve the
  selected stable entry; seeking during recovery updates the restore point.
- If all attempts fail, the Controller reports `AudioStatus::DeviceUnavailable`,
  stops the model, preserves the queue/current selection, and emits one final
  error. A later user Play starts a fresh bounded cycle, allowing a returning
  device to be reconsidered without an unbounded background retry loop.
- Shutdown joins any pending recovery thread. A recovered engine that arrives
  after the Controller is gone is dropped and cleanly shut down by the channel
  owner. The real-time fill callback still performs only ring/atomic work; all
  device opening, failure classification, and retry scheduling happen off the
  callback.

Deterministic lifecycle tests cover initial start failure, fatal runtime
reports, duplicate fatal reports, nonfatal notices, successful replacement,
repeated loss/recovery, pause/stop/reselection/queue edits during recovery,
seek/position policy, configuration changes (44.1 kHz mono to 48 kHz stereo),
four-attempt exhaustion, recovery after a later device becomes available, and
shutdown during a blocked recovery open. These tests use injected backends and
threads; they do not prove physical unplug, audio-server restart, Windows/macOS
behavior, or default-device changes on hardware.

A separate low-amplitude hardware smoke run used the same Linux host's ALSA
default device (`Default Audio Device`, PipeWire ALSA plugin), F32, 48 kHz,
stereo. Stream creation succeeded, a 4-second stereo WAV was resampled and
played for two seconds, and shutdown joined cleanly. Diagnostics reported a
512-frame callback, 96,768 requested and 94,720 delivered frames, zero
underruns, zero decoder/preload/output errors, zero notices and no gate
re-prebuffers. No physical disconnect, default-device switch or audio-server
restart was performed; recovery itself was exercised only with injected
backends.

Known lifecycle limits: Flux supports only the backend default output device and
does not persist a device identifier or offer device selection. CPAL/PipeWire
may report `DeviceChanged` and reroute a default-device stream; that is
surfaced as a notice and the stream is retained. On backends such as Linux ALSA
that do not follow default-device changes, a still-valid stream continues on
the old device; Flux does not poll and reopen just because the system default
changed. Recovery therefore occurs after a reported fatal stream failure, not
proactively. Engine diagnostics are per engine instance and reset when the
failed engine is replaced; Controller status/last-error events retain the
lifecycle outcome. Recovery restores the last delivered position, not the
elapsed wall-clock time during the outage, and never claims gapless continuity
across the disconnection.

Resource bounds remain structural: at most one active decoder plus one prepared
decoder; one bounded output ring (approximately two seconds); retained
crossfade tail, incoming overlap and mixed pending audio bounded by the
5-second configured maximum plus decode chunks; one pending transition record.
No decoder/resource count is exposed as a live metric, so the bound is audited
in code rather than measured as a runtime gauge.

See the device-lifecycle section below for fatal/notice separation, bounded
default-device recovery, and the remaining physical-hotplug limitations.

### Discovery, ratings, history and dashboard

Ratings/favorites remain canonical track data. Track rows expose a favorite
toggle and deterministic rating cycle (none/1–5★); the batch bar applies
favorites and ratings to selected stable TrackIds in one canonical ratings
transaction. Existing unrelated rating/favorite values are preserved; invalid
or stale selections reject without writes. Ratings remain track-only.

Smart Collections are dynamic queries over canonical data plus local derived
history, not editable playlists:

- Favorites and Highly Rated (4–5★);
- Recently Added using `TrackEntityDto.created_unix_ms`;
- Recently Played and Most Played using local meaningful-play history;
- Without Lyrics, Synchronized Lyrics and Without Artwork.

Membership recomputes from current catalogue/rating/history state. Collection
track lists reuse normal playback/queue/search/filter/selection actions. Album
detail retains canonical album order. The dashboard shows track/artist/album
totals, favorites/rated counts, lyrics/synchronized/artwork counts, recent and
most-played previews, and highly-rated tracks, with direct collection links.
Dashboard summaries are cached by catalogue generation plus history generation.

New canonical imports persist `created_unix_ms` in `track.flux`. Existing or
migrated records lacking a reliable timestamp keep zero and are excluded from
Recently Added; filesystem modification time is not substituted. Creation time
is preserved through metadata edits, moves, audio replacement and album transfer.

Meaningful playback history is derived application state in a per-canonical-
library `history.flux`, separate from the catalogue and audio tags. It stores an
aggregated play count and last-played Unix milliseconds per stable TrackId using
atomic replace; it is not a second catalogue. A play is counted once per queue
entry after accumulated playing time reaches `clamp(duration/2, 5s, 30s)` (30s
when duration is unknown). Pause/resume does not add time, seek jumps over two
seconds are ignored as listening evidence, and restarting within the same queue
entry does not create another play. No historical plays are fabricated. Malformed
history is preserved and history writes are disabled for that session. History
survives TrackId-preserving metadata edits, moves and audio replacement.

Advanced sorting supports catalogue order, title, artist, album, track/disc,
favorites, rating, play count, last played, date added and duration with an
ascending/descending toggle and stable TrackId tie-breaks. Filters include
favorites, lyrics, artwork, release type and minimum rating. Reset clears all
filters/direction. Remove/clear actions never delete tracks. The TUI exposes
read-only `F` favorites-only browsing and `H` most-played history summaries;
Dashboard/Smart Collections/rating/batch editing are GUI-only.

### Lyrics

Lyrics remain a single managed UTF-8 text file referenced by the existing
`TrackEntityDto.lyrics` filename inside the track directory. Both plain and
synchronized lyrics use that managed `.lrc` filename; no schema migration or
second lyrics store was added. Audio tags are never read or written. Replacing
or moving audio preserves the managed lyrics file through the existing track
lifecycle/album-transfer transactions.

The app-level lyrics model parses and serializes:

- plain text with preserved line order, blank lines, UTF-8 and non-Latin text;
- LRC timestamps with 1–3 fractional digits, normalized to milliseconds;
- multiple timestamps on one source line, expanded into deterministic events;
- duplicate timestamps, retaining all text and ordering by source line;
- untimed lines and empty lines;
- standard/unknown metadata tags, preserved in source order and never used as
  authoritative track metadata;
- optional signed LRC `offset`, applied additively to timestamps for display
  (negative effective times clamp to zero).

Malformed timestamp syntax, out-of-range seconds/fractions and overflow are
rejected rather than silently downgraded to plain text. Serialization emits
millisecond precision (`[mm:ss.xxx]`), preserving line order and metadata.
Multi-timestamp source lines are normalized to one event/line per timestamp;
content and timing are preserved, but the original combined timestamp grouping
is not. No fuzzy/online lyrics lookup exists.

The GUI has a dedicated Lyrics library showing tracks with managed lyrics,
plain/synced/invalid classification, a title/artist/album filter, empty states
and navigation to Album, Artist and Track. The reader displays timed events with
the current line highlighted from `Controller::position()` (the actual playback
clock), not an independent timer. Scroll/paused/seeking/track changes update
from the same position; a player Lyrics entry follows explicit current-track
changes until the user opens another track/editor. Plain lyrics display without
synchronization. Karaoke mode shows the current line, previous/upcoming lines
and plain/absent fallbacks; shuffle/playback policy is unchanged.

The line-oriented editor supports Unicode, blank lines, add/remove/move lines,
manual `mm:ss.xxx` timestamps, assign-current-position, ±100 ms/±1 s nudges,
clear timestamps, save/cancel and explicit remove. Timestamps must be
non-decreasing in line order. Removing every timestamp from a previously synced
document requires a second explicit Save before conversion to plain text.
Unsaved changes require a second cancel/navigation action. Saving compares the
reviewed raw text to the current managed lyrics and rejects a stale edit. Saves
and removals use the canonical journal with `catalog.flux` as the commit point;
replaced/removed lyrics files are journal cleanup and recovery yields complete
old or complete new state. The track and its audio are never deleted.

Import uses the native platform picker for `.lrc`/`.txt`, previews content and
classification, rejects malformed input, preserves the source file and requires
a second confirmation before replacing existing lyrics. Export writes UTF-8 LRC
with timestamps or plain text without timestamps, requires `.lrc`/`.txt`
extensions, does not touch audio, and requires a second action to overwrite an
existing destination. Export path selection is a GUI path field, not a native
save dialog. All operations are offline; no remote services are used.

The TUI exposes read-only lyrics inspection with `y` (scrollable, Esc returns)
but no editing/import/export/karaoke. Those remain GUI workflows.

### Album artwork

`set_cover` assigns or replaces the single managed cover file and cleans the
previous cover only after commit. `remove_cover` clears the reference and
journal-cleans the file. Missing declared covers are reported in GUI detail and
block album transfer; PNG/JPEG/WebP extension and a bound of 16 MiB are checked
by the canonical assignment path.

The GUI decodes managed PNG, JPEG and WebP artwork off the UI thread for display.
Decoding rejects files over 32 MiB, dimensions over 4096 px on either side or
16 million pixels, zero-sized images and unsupported formats. A 512 px preview
is generated with preserved aspect ratio, rendered with cover cropping in
library/player thumbnails and contain fitting in album detail. Missing, invalid
or unsupported artwork falls back to the existing generated glyph; album detail
shows the decode error without changing the managed file. The cache key includes
album identity, cover path, modification time and length, so replacement or
removal invalidates the preview and a fresh decode is scheduled. The cache is
bounded to 128 album previews; crossing the bound clears it rather than growing
without limit. The cache is
bounded to 128 album previews; crossing the bound clears it rather than growing
without limit.

### Native file selection

The GUI uses the existing platform abstraction for native dialogs: zenity or
kdialog on Linux, AppleScript on macOS and PowerShell/WinForms on Windows.
Audio import offers audio-file and folder filters; managed-audio replacement
offers audio-file filters; album artwork offers PNG/JPEG/WebP filters. Selecting
a file only fills the existing path field. It never assigns artwork, replaces
audio or starts an import by itself. Review, validation, fingerprinting and
transactional confirmation remain mandatory, and cancellation is a no-op.
Fixed-target import keeps its AlbumId and owner because native selection reuses
the existing import manager rather than opening a fresh general import.

The Linux paths invoke zenity and fall back to kdialog; if neither is installed,
the caller gets an actionable error and the in-app Flux Browser remains
available. macOS and Windows dialog scripts are compiled but are not
runtime-verified by this Linux-based project. All file-dialog interaction in the
test suite is message-level; real native dialogs are not claimed as manually
exercised.

### Album ordering

`set_album_track_order` requires exactly the existing track set, validates album
membership, and writes the canonical album order. `reorder_album_track` moves one
stable TrackId without recreating it. Order persists in `album.flux`, survives
reopen and is never inferred from directory enumeration. Disc/track number fields
are preserved independently; no disc numbers are invented.

### Empty-album deletion and populated albums

`delete_empty_album` is transactional and refuses an album containing tracks.
Populated album deletion is blocked in the GUI; tracks must first be transferred
or explicitly deleted through the existing reviewed workflows. Empty deletion
removes `album.flux` and any cover as journal cleanup; the artist remains.

### Atomic import and existing-album import

Every canonical import path now stages the complete reviewed batch in one
transaction: proposed artists, proposed albums, tracks, ordered credits,
ownership, album ordering, catalogue references and approved existing-track
metadata updates. No artist or album is
created by a separate write before the import. If publication fails or the plan
is stale, recovery yields either the complete old catalogue or the complete new
catalogue; no proposed artist, album or track survives an uncommitted import.

Artist references distinguish existing stable IDs from proposed names. Exact
unique existing names can resolve to an existing ID; ambiguous names require an
explicit artist selection in review. Proposed artists with the same exact name
are allocated one new ID within the batch. The physical owner remains one of the
ordered credits, and an existing destination album must have the same owner.

The album detail view exposes **Add tracks…**, opening the existing Import
Manager in fixed-target mode. The target carries AlbumId, owner, title, release
type, cover reference, existing order and catalogue generation; owner and album
are fixed through review. New selected tracks are appended in reviewed order by
the same atomic batch used by the general Import Manager, preserving existing
album metadata, cover, track IDs, order, lyrics, ratings, playlists and queue
references. Cancellation before execution writes nothing. Change to the target
generation, owner, title, release type, cover or order rejects the stale plan.

### Duplicate and conflict review

The Import Manager classifies conflicts deterministically using managed content
fingerprints (size plus FNV-1a hash), file size, duration when available,
normalized title/artist/album, album membership and incoming-batch identity. It
distinguishes:

- exact audio duplicate at the same path;
- exact audio duplicate at a different path;
- identical incoming audio selected twice;
- metadata collision with different audio;
- same title with different audio;
- probable duplicate when size, title and (when both are known) duration agree
  but the fingerprint differs;
- ambiguous audio when several existing tracks share the fingerprint.

Each candidate exposes the matched TrackId, existing title/artist/album, album
membership, managed audio path and fingerprint evidence. A match in the fixed
target album is distinguished from a match in another album. The UI never
silently chooses between multiple matches.

Available decisions are **Skip**, **Import separately**, and **Update metadata**
for a unique exact-audio match. Skipping keeps the existing canonical track and
creates no proposed artist or album. Importing separately allocates a new
TrackId and requires an explicit choice even when the fingerprint is identical.
Updating metadata preserves the existing TrackId, managed audio, owner, album,
lyrics, ratings/favorites, playlists, queue entries and playback selection; it
changes only the approved title, ordered credits and track/disc numbers. The
existing physical owner is always retained in the credit list.

Duplicate review never deletes or overwrites managed audio. Audio replacement
is a separate reviewed Track-editor operation described below.

Approved updates and new imports share one `import_review_batch` transaction, so
new artists required by updated credits are staged with new tracks and albums.
The plan records the catalogue generation, source fingerprint, existing
track/album/credit/metadata snapshots. A changed generation, source file,
fingerprint, album membership or reviewed metadata rejects the plan before
publication. In fixed-target mode a metadata update is allowed only for a
duplicate already in that album; an off-album duplicate is rejected rather than
silently redirected.

Newly created artists cannot survive a failed import: their `artist.flux` files,
catalogue entries, IDs and relationships are journal-owned batch writes and are
removed by rollback. They may survive only when the catalogue commit point was
reached, in which case the complete import is authoritative.


### Track lifecycle: metadata, moves and replacement

The canonical Track editor uses three immutable, generation-checked plans.

**Metadata edit.** A plan snapshots the TrackId, AlbumId, physical owner, title,
ordered credits, track/disc numbers, duration, audio fingerprint and album title.
Execution atomically writes the track record and, when approved, the album-title
rename in the same `album.flux` transaction. TrackId, AlbumId and physical owner
never change. Ordered credits must be non-empty, unique, reference known artists
and include the owner; the owner is never silently replaced. Lyrics, ratings,
favorites, playlists, queue entries and playback selection stay keyed by the
stable TrackId. Any catalogue-generation change or track/album snapshot change
rejects the plan before publication. The TUI supports the same safe edit form as
`title|album|track|disc|artist-id|credit-ids`; move and replacement review remain
GUI-only.

**Track move.** A plan names the target TrackId, source AlbumId, destination
AlbumId, explicit insertion index, source/destination order snapshots, metadata,
audio path, lyrics path and fingerprints. The destination album must have the
same physical owner as the track; ownership changes remain a separate transfer
workflow. Execution copies audio (with fingerprint verification), writes the
track record under the destination album, writes both album orders, updates the
catalogue track's album/audio/record references and commits once. The source
album is left in place, including when it becomes empty. Old audio, record and
lyrics are journal-owned cleanup after the catalogue commit point. TrackId,
owner, credits, ratings, playlists, queue entries and playback selection are
preserved.

**Managed-audio replacement.** A plan snapshots the TrackId, current managed
audio path and fingerprint, replacement source path and fingerprint, album,
owner, metadata, record path and generation. The replacement is installed at a
new canonical path of the form
`tracks/track_<id>.replacement-<version>.<extension>`; the old managed audio is
never overwritten. Execution revalidates the catalogue generation, track
metadata, current audio fingerprint, source fingerprint and destination
collision, stages the new audio with copy verification, writes the new record
fingerprint, updates the catalogue audio path, and publishes `catalog.flux`
last. Only after that commit does journal cleanup remove the old audio. The
user-supplied source file is never moved, modified or deleted. TrackId, album,
owner, credits, lyrics, ratings, favorites, playlists, queue entries and playback
selection remain unchanged. A stale track, audio file, source file or destination
rejects the plan; recovery restores the complete old state or completes cleanup
for the complete new state. Validation is file existence, non-empty content,
fingerprint and extension safety; audio is not decoded and tags are not read.

Move and replacement use the same transaction engine as imports, album transfer
and deletion. Every meaningful boundary is failure-injected and reopened: before
and after journal creation, staging, copy/audio verify, metadata publication,
catalogue publication, cleanup and journal finalization. A committed catalogue
is authoritative; an uncommitted transaction restores replaced files and removes
only its journal-created paths. If the old managed audio is locked by an open
playback handle on a platform that forbids deletion, catalogue publication is
still complete and the journal-owned cleanup can be retried after the handle is
released.


## Artist deletion

Artist deletion is a complete explicit workflow, not a directory removal.
`plan_artist_deletion` builds a generation-tagged preview with:

- every owned track, album, audio and lyrics resource;
- every credited-only track owned by another artist;
- ordered credits, physical owners, ratings and playlist references;
- owned albums, including empty albums and managed covers;
- eligible surviving owners and all blocking/resource issues.

Each owned track must receive an explicit decision:

- **Transfer** to an eligible surviving credited artist, into an existing album
  owned by that artist or an explicitly named new release;
- **Delete** after an explicit irreversible confirmation.

Each credited-only track must receive **RemoveCredit**. Audio, TrackId, owner,
album and ratings remain unchanged; the deleted artist is removed from the
ordered credit list. The physical owner must remain credited.

Unresolved tracks, no surviving owner, empty resulting credits, missing
resources, invalid destinations, collisions, malformed playlists/ratings or a
changed catalogue block execution. The plan records the catalogue generation
and per-track album/owner/credit/audio/lyrics/rating snapshots; any relevant
change rejects the plan as stale.

Execution is **one** canonical transaction, not a sequence of transfers followed
by an artist delete:

1. revalidate the complete snapshot and every decision;
2. stage all transferred audio/lyrics and all updated track/album metadata;
3. stage playlist and ratings updates;
4. publish staged resources and entity metadata;
5. publish `catalog.flux` with the complete final state: transfers, credit
   removals, deleted tracks, removed artist/albums and new albums;
6. remove obsolete audio, lyrics, album files/covers and `artist.flux` only as
   journal-owned cleanup;
7. finalize the journal.

Transfers preserve TrackId, ordered surviving credits, playlist/rating
references, lyrics and queue identity. Explicit track deletion removes audio,
track `.flux`, lyrics, rating and playlist entries in the same transaction.
Queue entries are pruned by the application Controller after commit, and
playback stops if the current track was deleted. The artist is never removed
while an owned track or credit reference remains.

The GUI opens **Delete artist** from the Artists setup card, shows the complete
preview, edits per-track decisions and destinations, shows playlist/rating/lyrics
consequences, blocks unresolved plans, and commits with an explicit destructive
button. Cancellation before commit clears the preview and writes nothing. The TUI
intentionally exposes no destructive deletion shortcut; it directs users to the
GUI review workflow. No independent TUI transaction logic exists.

The multi-item deletion path runs the same 18-point injected interruption matrix
plus real child-process aborts at after-journal, metadata publication,
post-catalogue commit and cleanup. Recovery is verified as complete-old or
complete-new, never a partially deleted artist; a second recovery is idempotent.

## Transaction state machine and recovery

Ownership transfer uses the canonical batch journal. The persisted states are:

| State | Meaning | Recovery decision |
|---|---|---|
| `prepared` | Journal is durable; resources are not yet staged. | Uncommitted: roll back. |
| `copying` | Destination resources are being staged inside the transaction directory. | Uncommitted: roll back; staging is discarded. |
| `verified` | All staged copies passed size/content verification. | Uncommitted: roll back. |
| `publishing` | Staged resources may be replacing entity files; catalogue is not yet authoritative. | Uncommitted unless the catalogue records this transaction ID: roll back. |
| `committed` | The catalogue was atomically published and records this transaction ID. | Finish cleanup only if the catalogue records the ID. Legacy metadata-only journals from older builds may be completed when their `intended_generation` equals the active generation and their state is committed/cleaning/completed. |
| `cleaning` | Obsolete source resources are being removed. | Same catalogue-ID check, then finish cleanup idempotently. |
| `completed` | Cleanup finished; only journal removal remains. | Same catalogue-ID check, then finish cleanup/remove journal. |
| `rolled_back` | Reserved for an explicitly recorded rollback. | Treated as uncommitted; rollback is idempotent and the journal is only then removed. |
| `applying` | Legacy journals from the earlier implementation. | Treat as uncommitted unless the catalogue records its ID. |

`Planned` is the in-memory ownership plan and creates no persistent mutation.
The **single authoritative commit point is `catalog.flux` containing the
transaction ID**. Destination files existing alone never imply commitment.
A malformed, truncated, unknown-version, missing or ambiguous journal stops open
with a recovery-required error and preserves the transaction directory and
valuable files.

### Persistence sequence

1. Create `transactions/<txn>/`, synchronize the transactions directory, back up
   every replaced target, and synchronize the transaction directory. If setup
   fails before a journal exists, remove only that newly created transaction
   directory and report any cleanup failure together with the original error.
   A partially or fully published journal is preserved as a recovery authority.
2. Write `journal.flux` with `prepared`, using a synced temporary file, rename,
   and parent-directory sync.
3. Stage every byte payload and audio/lyrics copy inside the transaction
   directory, syncing each staged file. Verify expected content fingerprints.
4. Write `verified`, then `publishing`.
5. Publish staged entity files with same-filesystem renames and parent-directory
   syncs.
6. Publish `catalog.flux` last. Its transaction ID is the commit record.
7. Write `committed`, `cleaning`, remove only journal-owned obsolete resources
   (checking the active catalogue never references them), write `completed`, then
   remove the transaction directory.
8. On any error before catalogue publication, rollback restores backups and
   deletes only transaction-created targets. On interruption, no rollback runs;
   reopen follows the journal/catalogue decision above.

Staging inside `transactions/<txn>/` prevents partially copied resources from
appearing as destination files. Cross-filesystem source copies are performed as
verified copy-then-publish; target publication uses same-root renames. Files are
never removed merely because they are unreferenced by a partial catalogue.

### Transaction concurrency and pending work

Normal writes are single-transaction: before creating a journal the engine
checks `transactions/` and refuses to start if any transaction directory is
present. This prevents a retry from writing a second transaction while a failed
rollback still owns earlier backups. Recovery can process multiple pre-existing
journal directories independently (for example after external corruption or
copied crash artifacts); committed cleanup and uncommitted rollback are decided
per transaction and one transaction never uses another's resources. The
catalogue transaction ID remains the only commit authority.

### Partial journal writes

`write_atomic` creates `journal.flux.tmp`, writes and syncs content, renames it
to `journal.flux`, then syncs the parent. Tests cover the observable artifacts:
missing journal after directory creation, incomplete temporary journal, truncated
final journal, complete temporary journal before rename, and a complete journal
after rename before parent sync. Missing/truncated artifacts stop open and
preserve the directory; a complete uncommitted journal is rolled back. Low-level
write failures inside `write_atomic` itself are not injected because the
standard library does not expose a portable hook; the closest deterministic
filesystem states are tested instead.

### Real process termination

Dedicated child test processes abort without unwinding at:
- after journal preparation,
- during audio staging,
- after metadata/resource publication before catalogue publication,
- immediately after catalogue publication,
- during post-commit cleanup,
- before metadata-only journal finalization,
- during recovery cleanup.

Metadata-only transactions are additionally exercised across every journal
stage, including committed and cleaning states, and verify that the catalogue
records the transaction ID and advances exactly one generation.

The parent reopens the same disposable library, verifies old/new authority,
audio and lyrics bytes, TrackId/credits/ratings/playlists, then reopens again to
prove idempotence. The recovery child is aborted inside cleanup and the parent
resumes it; journal and file remain until the successful retry.

### Failure-injection matrix

Test-only, deterministic boundaries are injected through a scoped
`TransactionFailure` value; production has no user-facing control. The transfer
matrix exercises all 18 core points below with an abrupt interruption, then drops
the library, reopens/recoveries twice, and checks the old-or-new authority, exact
audio/lyrics bytes, owner, album, credits, ratings, playlists, TrackId and one
physical audio copy. Metadata-only tests additionally inject failure after the
transaction directory is created, after backups are complete, and immediately
before journal publication:

1. before journal creation;
2. after durable journal preparation;
3. before staging;
4. during generic staging;
5. during audio staging;
6. after audio staging before verification;
7. during lyrics staging;
8. after copy before verification;
9. after verification;
10. before metadata publication;
11. after a metadata/resource publication step;
12. before catalogue publication;
13. immediately after catalogue publication;
14. before cleanup;
15. during source-audio/record cleanup;
16. during source-lyrics cleanup;
17. after cleanup;
18. before journal finalization.

Additional tests cover rollback retry, referenced-path cleanup refusal, and
malformed/truncated/unknown-version/orphan journals. Recovery is idempotent and
preserves ambiguous resources.

### Durability limits

The implementation flushes journals and staged files and synchronizes parent
directories after critical renames on Unix. It cannot promise stronger
power-loss guarantees than the filesystem/device cache honors, nor atomic
multi-file visibility across independent renames. The catalogue is the sole
commit record, so those renames are reconciled by journal rollback/cleanup.
In-process setup errors before journal publication remove only the current
incomplete transaction directory. An abrupt process termination in that same
pre-journal window can still leave a journal-less directory; it is preserved as
ambiguous and requires explicit inspection rather than guessed deletion.
Unsupported or read-only storage fails visibly; no automatic repair is attempted
for a malformed journal. Catalog-only backup restoration remains disabled because
it is not generation-consistent with per-entity files.

Filesystem tests on Unix cover read-only destination directories, pending-write
rejection after a permission-blocked rollback, destination symlink replacement,
and missing source audio. Cross-device library roots and ENOSPC were not
available to test here; staging uses copies inside the transaction directory and
publishes with same-root renames, so a true cross-device source is copy-based,
but no second mounted filesystem test is claimed. Non-Unix directory-sync
semantics are not independently verified.

## Migration

`canonical_migrate` supports:

- Portable schema-v3: stable track/artist IDs, effective Flux metadata (override
  → stored tag snapshot → path fallback), album grouping, preserved counters,
  missing-file blocking, copy verification and source immutability.
- Legacy index: absolute source paths only, exact legacy metadata, deterministic
  new artist IDs from exact names, preserved track IDs/counter, and explicit
  blocking for missing, relative, non-file or duplicate paths.

Both build in an isolated sibling staging directory, reopen and validate the
canonical library (IDs, relationships, paths, copied fingerprints), then
activate with a same-parent rename. A failure never activates staging and never
modifies the source. Destination collisions and existing directories are
refused. The staging directory is retained on build/rename failure for explicit
cleanup.

## Known gaps

1. The public Controller still exposes the old legacy/portable constructors and
   switches for migration compatibility and existing tests. No GUI/TUI normal
   workflow uses them, but the API-level retirement is incomplete.
2. Import review supports ordered credits, owner selection, add/remove/reorder,
   and existing-artist selection. A separate featuring boolean and release-type
   picker are not exposed; the canonical model stores order only.
3. Import copy/publication runs synchronously at the canonical commit boundary;
   cancellation is safe before commit, but there is no background byte-copy
   progress for very large files.
4. Single-track ownership transfer, whole-album transfer and multi-item artist
   deletion are implemented with copy-first publication, journal cleanup and
   no-op retry. Album transfer intentionally preserves AlbumId and does not merge
   into a different existing AlbumId. The GUI exposes all three reviewed
   operations; the TUI refuses transfer/deletion and points to the GUI.
5. Artwork selection has native PNG/JPEG/WebP pickers in addition to path entry.
   Decoding is bounded as documented above; animated formats and tag artwork
   are not supported.
6. Ratings/favorites have canonical Controller APIs and persistence tests, but
   only favorites are exposed in a batch GUI action; no dedicated rating screen
   exists. Lyrics have dedicated GUI and read-only TUI views.
7. Automatic catalogue-backup rollback is not generation-consistent and is
   disabled; transaction-journal recovery is implemented and test-injected at
   every listed boundary. A recovery-required malformed journal requires manual
   inspection; it is preserved rather than destructively repaired.
8. Migration is application-level and bidirectional in the normal UI, but the
   execute phase is synchronous; very large migrations need background progress
   and cooperative cancellation.
9. Cross-platform native behavior and real GUI migration were not runtime
   verified in this stage.
10. Fixed-target album import is exposed in the GUI only; the TUI retains general
    quick import and has no target-selection review flow.
11. Atomic import deduplicates exact names only; approximate/fuzzy matching is
    deliberately not performed. Ambiguous exact names require explicit review.
12. Artist deletion execution is exposed in the GUI only. The TUI deliberately
    refuses deletion/transfer and points to the GUI; this is a product limitation,
    not a separate transaction implementation.
13. Move and managed-audio replacement review are GUI-only; the TUI supports
    safe metadata/credit editing and its help directs these two operations to
    the GUI review workflow.
14. Audio replacement validates file existence, non-empty content, extension
    safety and fingerprints only; it does not decode the audio or read tags.
15. On platforms that block deleting an open file, replacing or moving the
    currently playing track may leave old-file cleanup pending until playback
    releases the handle. The catalogue commit remains authoritative and recovery
    retries the journal-owned cleanup.
16. ENOSPC and a true second mounted filesystem were not available for testing.
    Low-level `write_atomic` interruption is represented by deterministic journal
    artifacts rather than an in-process portable filesystem hook. A real process
    abort is tested, but not a kernel/power failure.
17. Global search/filtering/multi-selection, playlist management and batch
    playlist/favorite actions are GUI-only; the TUI retains its existing track
    search and safe single-track operations. Range selection, library-to-playlist
    drag, batch queue mutation and batch album moves are not implemented.
18. Search is substring matching only, not fuzzy; results are capped at 20 per
    entity type. Track lists retain the existing page-based rendering rather than
    adding a new virtualization layer.
19. Lyrics are line/event synchronized, not word-by-word. The editor is
    line-oriented rather than a rich multiline text area. Multi-timestamp LRC
    lines are normalized on round-trip. Export uses a typed destination path,
    not a native save dialog. No remote lyrics lookup, tag extraction or TUI
    editing is implemented.
20. Playback history and play counts are collected only after this feature is
    introduced; pre-existing listening activity is not inferred. History is
    derived state in `history.flux` and is separate from the canonical catalogue.
21. Smart Collections are fixed built-ins; user-defined rule composition,
    dashboard persistence/settings, TUI dashboards and advanced history export
    are not implemented. Recently Added excludes legacy/migrated records with no
    reliable canonical creation timestamp.
22. Most Played exposes track rankings plus compact artist/album aggregates;
    there are no separate artist/album statistics screens. History storage is
    aggregated per TrackId, so individual play-event timestamps beyond the last
    play are not retained.
23. Detailed event-level listening history and explicit listening-session
    objects/boundaries are not implemented; the aggregate `history.flux`
    cannot reconstruct them without fabricating chronology. Recent/Most Played
    remain aggregate views.
24. Natural gapless playback and optional equal-power crossfade are implemented
    inside the engine. Crossfade is disabled by default, is configured in the
    GUI Settings view and through TUI `K` / `[` / `]`, and is persisted in the
    shared settings JSON file (3000 ms default duration, 0–5000 ms range). The
    outgoing tail, incoming head and mixed pending audio are bounded by the
    5-second maximum; sources whose duration is unknown or too short for a
    meaningful tail use the gapless fallback. Gapless and crossfade have been
    verified with deterministic synthetic sources and an instrumented null
    backend, not by measuring real hardware. One prepared successor is
    supported; a chain depends on the Controller preloading the following entry
    after each accepted transition.
25. Multi-entry queue removal and a dedicated queue clear action are not exposed
    in the GUI; individual removal/reorder and queue population from track/
    playlist actions remain available.
