# M29 — Product redesign, playback reliability and native windows

This milestone covers two playback-critical fixes, the shared interaction
system, library browsing redesigns, the guided import workflow and the native
secondary Import window. Only implemented and locally verified behavior is
described here; native-OS and human validation remain tracked as manual work.

## Artwork loading

The GUI requests artwork from what is actually on screen, not from playback or
selection alone.

- `visible_album_targets()` collects Album covers for the visible Track page,
  the Home shelves, the visible queue page, the current Track, search results,
  Album groups/detail and Artist album relations.
- `visible_track_targets()` collects Track artwork overrides for the same rows.
- `visible_artist_image_targets()` collects portraits for the visible Artist
  page.
- `ensure_artwork_tasks()` removes cache entries for deleted entities, keeps a
  content-addressed `ArtworkKey` (`path + modified time + length`), reuses
  cached entries, and bounds concurrent decode tasks to six Album covers plus
  six additional Artist/Track overrides per update. Completed entries free the
  next visible items, so large pages load progressively.
- A changed cover file changes the key, invalidating the old entry. Failed
  decodes retain a stable neutral fallback with the error retained for detail
  screens.
- The artwork cache remains bounded by `ARTWORK_CACHE_LIMIT` and clears at the
  cap before reinserting.

Precedence is unchanged: Track override → Album cover → neutral fallback.
Missing artwork and missing audio remain distinct states.

Regression coverage:

- visible Track rows request Album and override artwork without playback;
- Artist grids request portraits without selection;
- the decode request budget is bounded;
- cached entries are not decoded again;
- replaced covers invalidate and re-request;
- decode validation for PNG/JPEG/WebP, missing, invalid, oversized and
  unsupported files.

## Crossfade and seek position

`EngineHandle::position()` projects device frames into the current source's
timeline:

```
frames = base_frames + played_frames - source_start_frame
```

`source_start_frame` already includes the seek baseline. The previous
implementation subtracted it from `played_frames` alone, so after a near-end
seek `played < source_start` saturated to zero and the incoming Track was pinned
at the old seek position. The fix sums `base_frames + played_frames` first and
then subtracts the source start. The same correction is applied when a handoff
is invalidated and the current source is reopened.

The transition event is emitted only when the device has consumed up to the
mix boundary. At that point the Controller switches the active Track and stable
QueueEntryId; the engine position is already relative to the incoming source.
The transition's `from_position` remains source-relative for history
accounting.

Regression coverage:

- crossfade after a near-end seek starts the incoming Track at zero;
- gapless handoff after a near-end seek does the same;
- a late seek inside the crossfade tail hands off at the exact track end;
- a second crossfade after seeking the second Track keeps the third position
  correct;
- Controller identity and QueueEntryId stay stable across the transition;
- outgoing audio is credited at the boundary without fabricating a play.

## View modes and persistence

`flux_app::settings` stores independent `list`/`grid` modes for Tracks, Artists
and Albums under `interface.view_modes`. Defaults are Track list, Artist grid
and Album grid. The settings writer preserves unknown fields, clamps invalid
identifiers to defaults and reuses the existing atomic writer.

`Controller::set_view_mode` applies the choice immediately even when the
settings file is write-protected, reporting the persistence failure without
losing the session preference.

## Library surfaces

- **Tracks** has a unified toolbar: search, filters, sort direction, view
  selector and collection actions. The table keeps sortable columns and adds
  distinct hover, focused, selected and playing states. Grid cards keep artwork
  as the focus with title, artist, album, duration, favorite and playing state.
- **Artists** defaults to circular profile cards with a Play action and
  switches to a dense list. Duplicate display names remain distinct by
  ArtistId.
- **Albums** defaults to square-cover cards with owner/participant context and
  a Play action, switching to the existing dense list.
- **Smart Collections** uses coherent cards with real descriptions, counts and
  preview titles.
- **Home** shelves wrap responsively; Recent/Added artwork is larger and the
  Continue Listening card remains the action focus.
- All Play actions reuse the existing Controller queue and identity model.

## Import workflow

The existing review-first import engine is retained. The UI now presents five
steps:

1. Source — native file/folder pickers or the in-app browser.
2. Analyze — background scan with cancellation.
3. Organize — artist/album/credits/artwork/decision editing.
4. Review — counts for new, updated, skipped and unreadable files, duplicate
   and conflict warnings, destination and identity guarantees.
5. Import — progress and result reporting with retry/choose-again paths.

Back navigation preserves the plan and decisions. Escape in the Review step
closes the workflow; in Review-confirm it returns to Organize; in the native
window it cancels/closes the window. The catalogue transaction remains the only
publication path; cancelling before commit removes unpublished copies.

## Native Import window

The GUI entry point uses `iced::daemon` so multiple native windows share one
`FluxReady`/Controller state.

- The main window ID is created in `Flux::boot` with `window::open`.
- Opening the import workflow while its window is absent calls `window::open`
  with an independent native window (`Import music — Flux`), requests focus and
  marks `import_window_active` so the main window omits the in-app overlay.
- Closing the import workflow closes only that window. Closing the native window
  closes the workflow and honours the existing cancellation semantics.
- Keyboard events are routed by window ID: main-window shortcuts continue to
  work while Import is open; Import-window keys are handled only by the import
  state machine.
- Resize events are filtered to the main window so resizing Import cannot
  change the main layout.
- Closing the main window exits the daemon.

Platform limitation: Iced 0.14 window settings expose no cross-platform
transient-parent or modal-flag API. The Import window is therefore an
independent native window that shares application state; it is not an
OS-level modal sheet. The in-app overlay remains only as the fallback used by
single-window tests and blocked/legacy paths.

## Motion and reduced motion

Iced 0.14 has no general arbitrary-widget opacity or transform wrapper. Flux
uses the supported `iced::Animation<bool>` type with a short 120 ms ease-out
curve and 16 ms motion ticks while an animation is in flight.

- Track action menus slide in near the invoking row and fit their contents
  instead of reserving a large empty list.
- Queue overlay slides in from the right; the inline queue width grows from
  zero and clips during the transition.
- Closing reverses the same progress before the overlay is removed.
- The menu is anchored using the visible row index and clamped to the window;
  it opens beside the row instead of at the top-right corner.
- When **Reduce motion** is enabled, progress resolves immediately to 0/1 and
  no motion ticks are subscribed. No animation delays interaction.

## Scrolling conventions

All page and panel scrolling goes through the shared `scrollable()` helper and
its accent-aware scrollbar style. Lists and grids scroll inside the available
content area with a footer outside the scroll region; headers and toolbars stay
fixed. Overlays use bounded scroll regions for their lists and details. Long
settings and import bodies use a single outer scroll container. Page content
keeps horizontal insets so scrollbars do not cover text.

## Known limitations

- No native Windows/macOS runtime validation has been performed for the Import
  window, motion or layout.
- Iced 0.14 cannot animate arbitrary opacity/transform; animations are limited
  to layout progress and supported widget state.
- The secondary window is not an OS-level modal/parented window on platforms
  where Iced does not expose that API.
- Screen-reader naming and full layout scaling remain tracked in issues #1 and
  #5.
- Artwork and crossfade were validated with deterministic tests and local GUI
  startup; real audio-device hotplug validation remains #7.
