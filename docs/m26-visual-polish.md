# M26 visual polish, playback stability and identity

Status: implemented and automated-tested; runtime-inspected on Linux. Human
usability, screen-reader and native Windows/macOS interaction audits remain
pending.

## Typography and interface scale

- The GUI baseline was raised instead of making a uniform percentage change:
  hero 32 px, section 21 px, Track titles 17 px, body 13.5 px, metadata 11.5 px,
  labels 10.5 px. Hard-coded 8–10 px text was removed or raised.
- Settings now exposes **Interface scale**: 100% (Comfortable), 110%, and 120%.
  It scales text only, so artwork and player proportions stay coherent. The
  default remains 100%, and the preference is persisted in the shared settings
  file under `interface.text_scale`.
- Text scale is normalized to a supported level; unknown or out-of-range values
  snap to the nearest supported level instead of producing an unusable layout.

## Branding and native identity

- The sidebar now shows the canonical Flux application mark next to the wordmark,
  using the vendored `assets/flux-icon.svg`. The validated navigation hierarchy
  is unchanged.
- The existing identity pipeline remains authoritative: embedded window icon,
  Linux reverse-DNS `io.github.neonmc23.flux`, desktop entry, hicolor icons,
  AppStream metadata, Windows ICO/version resource and macOS ICNS/bundle.
- `packaging/linux/install-local-identity.sh` installs the desktop entry and
  hicolor icons into `XDG_DATA_HOME` for development binaries. Wayland resolves
  application icons through that desktop identity, not through the process image
  alone, so a source-checkout run needs this step to show the Flux icon.
- The installed/packaged resources are validated by tests; actual compositor icon
  rendering remains a native desktop validation item.

## Track artwork, durations and missing states

- The main Tracks table now renders a real 40 px artwork thumbnail for every row:
  explicit Track override first, then canonical Album artwork, then a neutral
  generated placeholder.
- Missing artwork uses a neutral surface with a music/disc/artist icon. It no
  longer uses Artist-name initials.
- Missing audio remains a separate state: a red **missing** label, `--:--`
  duration and normal artwork placeholder. Artwork absence never marks audio as
  missing.
- New canonical imports persist technical duration from the audio container.
  Older catalogue records lazily probe and cache duration for display when the
  record has no stored duration.
- Duration now appears in the main Tracks table, Artist Track lists, Album Track
  lists, Queue, Playlists, Search results and the player. Unknown values use a
  stable `--:--` label.
- Track artwork and durations refresh when the library is reopened; the GUI
  artwork cache continues to invalidate on Album/Track artwork changes.

## Player geometry

- The progress row is now a fixed 20 px container with fixed-width elapsed and
  total labels. Switching between the slider and the non-seekable progress bar no
  longer changes player height.
- The player information block uses truncated one-line title/subtitle text and a
  fixed-height centre column, so loading, unknown duration and Track changes do
  not move the visualizer or transport controls.
- `0:00` / `--:--` formatting is shared through one helper and covered by tests.

## Column sorting

- TITLE, ALBUM, RATE and TIME headers are buttons. Clicking a new column sorts
  ascending; clicking the active column reverses direction. The active column
  shows a directional arrow and accent colour.
- Sorting is stable and uses the existing deterministic sort implementation.
- The global sort picker remains at the far right for advanced modes (Artist,
  track/disc, favorites, play count, last played, date added). It drives the same
  state as the header buttons, so there is one effective sort state.
- Header clicks never trigger playback.

## Track context menu

- The action sheet now has a Track artwork/identity header, genuine action icons,
  grouped Playback / Library / Go-to sections, clearer rating labels and a
  distinct disabled state.
- Actions remain identity-based and dispatch only implemented operations.
- Escape and backdrop dismissal still close the menu; covered content remains
  inert.

## Scrollbars and content layout

- Scrollbars use a quiet neutral scroller at rest and the accent only while
  dragged. The rail is transparent.
- The Tracks table reserves a right inset so the scrollbar cannot overlap row
  actions at narrow widths.
- Settings is scrollable, which prevents the new appearance controls from being
  clipped at 900×650.
- Mouse-wheel scrolling remains available; scrollbars were not hidden.

## Accent configuration

- Five curated presets: Flux Pink (default), Violet, Blue, Teal and Amber.
- Presets are persisted under `theme.accent` in the shared settings file and
  applied to selection, navigation, buttons, sliders, scrollbars, the visualizer
  and player highlights.
- Existing settings without a theme field keep Flux Pink.
- Text contrast was checked against the dark surfaces; presets are not arbitrary
  user colours.

## Visualizer

- Existing waveform curves and modes are preserved.
- Trace, glow and guide colours now follow the active accent instead of remaining
  fixed pink.
- No normalization or signal behavior was changed in M26.

## Motion and surfaces

- No non-essential animation was added. Iced 0.14 exposes animation timing but not
  a general opacity/transform wrapper for arbitrary widget trees; adding a fragile
  custom rendering layer for cosmetic motion was not justified in this cycle.
- Backdrop blur/translucency was not added. The existing opaque dark-violet
  surfaces remain readable and cheap to render.
- Issue #2 remains the tracking item for restrained motion and reduced-motion
  support.

## Remaining limits

- Screen-reader names and complete focus rings are still tracked by issue #1.
- TUI import-participant selection is still tracked by issue #3.
- Interface scale scales text only; it does not enlarge every control dimension.
- Native Wayland/Windows/macOS icon rendering was not manually verified in this
  milestone.
