# M30 — Product transformation report

This milestone addressed the second broad product pass: library lifecycle,
import organization, catalogue presentation, playback presentation, queue
semantics, display scaling and shared input routing. Only verified behavior is
described; manual/native validation still required is listed at the end.

## Library lifecycle

### Creation lock location

`LibraryLock::acquire_creation` previously placed
`.<name>.flux-create.lock` beside the selected library root. That kept locking
correct but exposed a technical zero-byte file inside or next to the user's
music folder.

Creation leases now use a stable per-user cache directory:

```
<XDG_CACHE_HOME|HOME/.cache|LOCALAPPDATA|temp>/flux-library-locks/<name>-<hash>.lock
```

The lock remains a real OS advisory lock (`File::try_lock`) held for the whole
create/validate/publish transaction. It is released on success, failure,
cancellation and process termination by the OS. Leaving the lock file in the
cache directory is intentional: unlinking a held lock would allow two processes
to lock different inodes. Equivalent root spellings hash to the same lease.
No creation artifact is created beside the music library.

### Missing/deleted library

`Controller::active_library_missing()` reports when the active canonical root no
longer exists. The GUI checks this on its periodic tick, stops playback once,
opens Library setup and explains that the in-memory catalogue is retained
read-only. This prevents stale tracks from being presented as a usable library
after deletion/unmount while preserving the data for recovery. Successful
Open/Create clears the state. Switching libraries still validates the candidate
before install, so a failed open cannot partially replace the active library.

## Artwork inheritance

Effective artwork is now resolved deterministically in `flux-app`:

- **Track:** explicit Track override → Album artwork (explicit or derived) →
  neutral fallback.

Embedded pictures are still read during import review and, when accepted, are
materialized as managed Album/Track artwork. A live embedded-picture fallback
for legacy catalogue rows that never stored managed artwork is not implemented
yet; such rows use Album inheritance or the fallback. This limitation is tracked
separately.
- **Album:** explicit Album cover → first Track in canonical Album order with
  an explicit override → physical owner's explicit portrait → other
  participant portraits in stored order → fallback.
- **Artist:** explicit Artist portrait → associated Albums in stable ID order
  (explicit cover or suitable Track override) → fallback.

Album-to-Artist fallback is not consulted while resolving Artist artwork, so no
cycle is possible. Derived images are never converted into stored overrides or
copied. Cache keys include the resolved path plus file metadata, so Track,
membership, order, portrait and cover changes invalidate naturally. The GUI
requests effective Album and Artist paths for visible entities and displays the
origin (explicit, Album, Track override, Artist, associated Album) in editors.

Covered by `artwork_inheritance_is_deterministic_and_reactive` and the existing
GUI artwork lifecycle tests.

## Import workflow

### Native workspace

The native Import window now uses the whole window: the popup border/shadow and
the in-window Close action are removed when hosted natively; the window content
fills the available area and the window chrome owns close. A five-step workflow
card explains Source, Analyze, Organize, Review and Import. The window remains
an independent iced native window with shared Controller state, per-window key
routing and safe close/reopen.

### Stable Artist and Album organization

The primary Artist field is now an entity selector:

- searchable Artist filter over canonical identities;
- stable-ID selection via `ImportArtistPicked`;
- immediate **Create Artist** surface with name, optional portrait, native image
  picker, validation and duplicate-name rejection;
- newly created Artists enter the selector immediately and can be applied to
  the selected Track or all checked Tracks.

Album organization now provides:

- the Album title field plus **No Album** (replacing the confusing “Single”
  clear action);
- searchable existing Album choice restricted to sensible relations;
- **Create Album** surface with title, release type, physical owner, participants,
  optional artwork, validation and immediate reuse;
- deterministic duplicate prevention: an Album with the same owner and
  case-insensitive title is reused rather than duplicated;
- participant selection is explicit and independent of Track credits;
- Album assignment is skipped for checked Tracks with a different physical
  owner, with a clear warning to set the Artist first. This prevents an invalid
  or partially failing canonical Album transaction.

The existing review, duplicate, cancellation, partial-failure, artwork, lyrics
and embedded-artwork behavior is retained. Organize now surfaces a bulk card
when tracks are checked (Artist, Album, create actions, skip/check). Artwork and
lyrics are collapsed by default; credits, participants, numbering and low-level
tools remain under Advanced Options.

## Catalogue and interaction

### Input routing and search focus

Keyboard events now reach application shortcuts only when the event status is
`Ignored`. Widgets that own text input report `Captured`, so Q, digits, letters
and editing shortcuts no longer fire while a search field or form is focused.
This is implemented in the shared `global_input_events` layer, not per screen.
`global_shortcuts_ignore_events_captured_by_text_inputs` covers both statuses.

The Tracks toolbar is one horizontal composition on desktop: search, Filter,
sort, direction and List/Grid. Active filters appear as a second chip row.
Search no longer expands to consume the row or wraps the sort/view controls.

### Track list and grid

- Track rows use 48 px artwork, fixed padding, single-line truncated title and
  credits, stable Album/TIME/RATE columns and distinct checked/focused/current/
  hover treatments.
- The Track grid uses wider cards (216 px artwork, 248 px minimum column),
  responsive 2–6 columns, consistent metadata hierarchy, truncated single-line
  text, favorite and playing indicators.
- List and Grid use dedicated list/grid icons.

### Continuous browsing

Track, Artist and Album lists no longer expose Prev/Next pagination. They render
an initial page and extend it as the user scrolls near the end, with a quiet
“Showing N of M — scroll for more” footer. Queue and playlist detail also
render continuously. This keeps widget counts bounded while removing artificial
page stops.

### Queue

Queue is split into the active Now Playing card and an **Up Next** list that
excludes the active entry. Row/index/keyboard handling follows the pending list.
The panel fills its width (removing the right-side dead band), uses the shared
row component, scrolls continuously and labels clearing as **Clear pending**.
With active playback loaded, clearing removes only pending entries and playback
continues. With playback stopped/unloaded, clearing removes the ordinary
selection. TUI semantics and labels follow the same rule.

### Shared detail rows

Artist detail and Album detail now render Track rows with the shared `track_row`
component. Album detail keeps per-row reorder controls beside the shared row so
Album order remains editable without a competing visual language.

## Playback presentation

### Crossfade foreground

The engine reports `Transitioned` with the overlap length and then
`TransitionFinished` when the outgoing tail has been fully consumed.

During a crossfade overlap the Controller keeps the outgoing entry as the
foreground identity while the engine has already made the incoming source
audible. Displayed position is:

```
outgoing from_position + incoming engine position
```

clamped to the outgoing duration. Queue identity remains the outgoing entry.
When the overlap finishes, the Controller switches to the incoming entry using
its actual already-played position, preserving QueueEntryId and mixing behavior.
Gapless handoffs switch immediately. Seeks, stops and explicit queue actions
abort the deferred foreground before changing playback.

This differs intentionally from the previous implementable fix: progress was
correct but the foreground switched too early. Tests now cover overlap identity,
outgoing progress, finished position and engine completion.

### Oscilloscope

- Playing: live waveform.
- Paused after playback: the last waveform window is frozen exactly; no ticks
  are subscribed, so it neither animates nor flattens.
- Stopped or no audio: no visualizer.
- Reduce motion: no visualizer area at all, so no large empty band.

### Display scale

The interface supports any persisted value from 80% to 140%. Settings uses a
1%-step slider with live preview and 100/110/120 presets. The commit writes the
shared settings file; leaving Settings restores the committed value if a
preview was not released. Existing 100/110/120 values remain valid; older
out-of-range values normalize to the supported bounds.

## Visual and interaction system

- Setup moved into the sidebar between Import Music and Settings; the redundant
  top-right Setup button was removed.
- Filter chips use accent surfaces and readable padding.
- Scrollbars use stronger active/hover contrast while remaining quiet.
- Import uses a flat full-window surface with a workflow explanation.
- Queue open/close and menu open/close retain the M29 reduced-motion-aware
  120 ms transitions.
- Artist cards use larger circular artwork, stronger titles and a single
  unambiguous Play action; internal duplicate counts were removed.
- Album cards use square 216 px artwork and explicit release/participant
  metadata. Standalone groups remain navigable.
- Home shelves and collection cards remain shared and responsive.

## Known limitations

- No native Windows/macOS runtime was available. The native Import window,
  motion and resize behavior remain manually unvalidated there.
- X11 automation was interrupted by an unrelated fullscreen Steam application
  during this session; startup, layout screenshots, window open/close and daemon
  exit were validated before that interference. Pointer/keyboard automation of
  every workflow was not completed.
- No interactive TTY/PTY tool was available for a fresh TUI runtime session;
  TUI behavior is covered by its test suite.
- Shortcut customization was not implemented. The prerequisite shared
  text-input routing fix is complete and tested; remapping persistence,
  conflict detection and a safe editing UI remain future work.
- Live embedded-artwork fallback for legacy rows without managed artwork is not
  implemented; import-time extraction remains the path that materializes
  embedded pictures.
- Iced 0.14 still provides no cross-platform transient-parent/modal window flag,
  arbitrary opacity/transform wrapper, or configurable scrollbar geometry.
- No large-library benchmark was run. Continuous loading keeps widget counts
  bounded, but representative measurements remain pending.
