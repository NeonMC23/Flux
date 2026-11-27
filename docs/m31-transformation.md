# M31 — Overnight reliability, personalization and audio-reactive UI

This report covers the import reliability rebuild, multi-source import, entity
organization, centralized theme system, audio-reactive visualizers, playlist card
polish and related validation.

## Import reliability

### Reproduced root cause

M30 introduced immediate Artist/Album creation during Organize. Those actions
commit to the canonical library and advance its catalogue generation. The
reviewed import plan still held the generation captured during analysis, and the
final canonical batch validates that generation before publication. Prepared,
well-organized imports were therefore rejected as stale by Flux's own Organize
edits. The draft could be left in a failed terminal view, making the work appear
lost.

### Corrections

- `refresh_import_baseline()` updates `plan.generation` and the fixed Album
  target generation after intentional import-time catalogue mutations:
  Artist creation, Artist portrait assignment, Album creation, participant
  changes and cover assignment.
- The final batch can now succeed after creating multiple Artists and Albums
  from the same import session. Regression coverage completes an import after
  creating a collaborative Album.
- Structured preflight runs before preparation/copying:
  - active library session and root;
  - fixed destination Album snapshot;
  - source files present;
  - selected Artists, credits and Album participants still exist;
  - selected artwork files still exist;
  - ambiguous normalized Album titles under one owner.
- Blocking preflight issues keep the plan, edits, source list and decisions and
  display all issues in the current workflow. Nothing is copied or published.
- Commit failures retain the plan and prepared result. The failed result screen
  offers **Back to Organize**, **Retry Import**, **Discard draft** and **Close**.
  It explicitly states that the draft is preserved.
- A deterministic test forces a post-review generation change, verifies the
  failed state retains candidate metadata, returns to Organize, refreshes the
  baseline and completes the retry without redoing organization.

### Guarantees and boundaries

The draft is never cleared by validation, preparation or commit failure. It is
cleared only by successful completion or an explicit discard/reset/close by the
user. Temporary copies are retained on failure for recovery inspection; retry
re-prepares from sources. Publication remains the existing canonical batch
transaction. Filesystem copy + catalogue publication are not one universal
filesystem transaction; the catalogue batch is atomic and failed publication
does not claim success.

Crash-to-disk draft recovery is not implemented. Draft state is in-memory for
the import session.

## Multi-source import

Source selection now accumulates instead of auto-advancing:

- native **Choose folder…** and **Choose files…** append to the selected source
  list;
- Flux Browser files/folder selections append;
- drag-and-drop appends;
- manual path entry appends;
- duplicate paths are skipped;
- selecting a parent folder replaces narrower already-selected children to
  avoid double discovery;
- selecting a child of an already-selected folder is skipped as covered;
- each source can be removed individually or cleared;
- **Analyze N source(s)** explicitly begins analysis;
- the same explicit Analyze action is available in the footer.

Tests cover non-advancing accumulation and folder/file overlap deduplication.

## Artist, Album and collaboration organization

- The primary Artist selector shows the current stable Artist rather than a
  placeholder, filters by search, and keeps the selected value visible even when
  it does not match the current filter.
- Artist creation remains immediate and duplicate-name checked.
- Album selection shows the current Album, filters by text, and keeps the
  selected Album visible. Album choices include owner/relationship labels.
- Album creation supports title, release type, physical owner, artwork and
  participants.
- Album assignment applies only to Tracks whose physical owner matches the
  Album owner; others are skipped with an explicit warning rather than causing a
  late partial failure.
- Bulk Artist/Album organization remains visible when Tracks are checked.
- Pick lists use the shared Flux select style instead of default framework
  borders.
- Track credits and Album participants remain separate controls.

## Theme and personalization

### Central token model

GUI colors now flow through runtime token functions:

- `bg_color`
- `sidebar_color`
- `surface_color`
- `surface_hi_color`
- `border_color`
- `text_color`
- `muted_color`
- `accent`, `accent_soft`, `accent_border`

Existing hardcoded token references were migrated in the GUI. Iced's custom
theme palette uses the same functions. Individual widgets no longer own a
separate fixed copy of the base surface palette.

### Surface presets

Four persisted palettes share one model and one source of truth:

- **Flux** — established neutral-violet.
- **Midnight** — deep blue-black.
- **Slate** — cool graphite.
- **Wine** — dark wine-plum.

Selecting a palette updates background, sidebar/player, surfaces, raised
surfaces, borders, primary text and muted text together. Settings, sidebar,
cards, menus, queue, import and player use the same tokens.

### Accent

- Existing curated accent presets remain supported and migrated.
- A custom `#rrggbb` accent can be entered and persists.
- Custom accents below a readable relative-luminance threshold are rejected;
  the previous valid accent remains active.
- Selecting a curated preset clears the custom override.
- Accent affects controls, selection, sliders, logo, scrollbar drag and
  visualizer colors.

Individual surface tokens are not independently editable; presets provide the
controlled surface palettes while accent is freely customizable.

## Audio-reactive visualizers

### Data path

The audio callback remains unchanged and real-time safe. The GUI continues to
consume the existing bounded post-gain waveform tap at `WAVE_TICK`. It derives:

- RMS energy;
- peak;
- low-frequency estimate from coherent block averages;
- high-frequency estimate from adjacent-sample differences;
- smoothed energy/peak/low/high and a bounded visual phase.

Smoothing and phase advance occur only while real playback samples are present.
No FFT, allocation or analysis runs in the audio callback. The GUI clamps and
smooths values so the visuals respond organically.

### Modes

Persisted `VisualizerMode`:

- **Wave** — the existing line scope, now with configurable height support and
  accent-derived glow.
- **Ambient orbs** — three glowing rings across four layered orbs. Orb position,
  radius and alpha respond to energy/low frequency and a slow phase.
- **Pixel wave** — two mirrored layers of quantized, rounded pixel cells driven
  by sampled waveform amplitude, energy and water-like phase.

The selected mode is used in the player strip and in a Home
**Audio Atmosphere** card with a 96 px visual representation and live energy
percentage when playing.

Pause freezes the last captured window and derived values because ticks stop.
Stop/silence resets the waveform state and hides the player visualizer. Reduce
Motion hides the visualizer area rather than animating it.

No physical audio-device listening validation was possible.

## Playlists

The existing persistent playlist model remains authoritative: stable
`PlaylistId`, ordered `TrackId` entries, duplicate entries where intentional,
schema migration and Controller queue integration.

Presentation improvements:

- Playlist collection is now a responsive card grid.
- Each card uses a deterministic 2×2 composition from the first four Track
  artworks (or neutral fallback), name, track count, total known duration and a
  direct Play action.
- Playlist detail shows the same composition, track count and total duration in
  its header.
- Playlist entry rows now show 32 px artwork while preserving checkbox, play,
  enqueue, reorder, remove, drag and keyboard behavior.
- Playlist artwork requests participate in the same visible-artwork cache.

## Typography, icons and cards

- Numeric durations and player timestamps use monospace for stable alignment.
- List/Grid layout icons remain dedicated Lucide-style layout icons.
- Existing vendored Lucide stroke family remains consistent.
- Artist cards use circular portraits, Album cards square artwork, playlist
  cards deterministic compositions and Smart Collections themed icon cards.
- No new font dependency was added. The system Iced font remains the readable
  default; a distinctive bundled typeface would require a separately licensed
  and validated asset and is not claimed.

## Validation summary

- Workspace tests: see final report.
- New tests: stale-generation import recovery/draft retention, source
  accumulation/overlap deduplication, theme preset/custom accent persistence and
  contrast rejection, visualizer mode persistence, playlist card/artwork paths.
- GUI runtime: Home with Wine theme, teal custom/preset accent and Ambient Orbs
  atmosphere rendered from the actual application; player/queue/catalogue
  layouts inspected.
- No physical audio reactivity validation; synthetic waveform data and code
  paths are tested only at the data/state boundary, not by ear.

## Known limitations

- No disk-backed crash recovery for an in-progress import draft.
- No custom per-token surface editor; presets plus custom accent.
- No bundled display typeface.
- No FFT/spectral analyzer; visualizer bands are bounded time-domain estimates.
- No physical audio/device or native Windows/macOS runtime validation.
- Shortcut customization and live embedded-artwork fallback remain tracked in
  issues #9 and #10.
