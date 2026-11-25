# Flux product design system (M25)

Status: implemented design language for the Iced GUI and the crossterm TUI.
This document describes the shared product hierarchy. It is not a claim of
human usability validation.

## Identity

Flux is a local-first music library and player. The interface should read as a
calm desktop application: dark, spacious, restrained, and centered on artwork,
titles, and listening state. The pink accent is a signal for selection, focus,
primary actions, and playback. It is not a background decoration. Icons are a
vendored Lucide set, not typographic approximations or emoji.

- Product name: **Flux**
- Accent: `#ff66cc`
- Foundation: dark neutral violet, not pure black
- No gradients, glow effects, emoji application iconography, or decorative charts

## Color roles (GUI)

| Role | Value | Use |
|---|---|---|
| Canvas | `#0c0a10` | Window background, player surround |
| Navigation surface | `#121016` | Sidebar, player bar |
| Surface | `#18151e` | Cards, filter bar, queue rows |
| Raised surface | `#221e2a` | Hover, pressed, menu field |
| Border | `#2e2838` | Card and control boundaries |
| Primary text | `#f6f1f8` | Titles and selected labels |
| Muted text | `#9c93a8` | Metadata, hints, inactive navigation |
| Accent | `#ff66cc` | Selection, primary buttons, active destination, current Track |
| Accent soft | `rgba(255,102,204,0.13)` | Selected row and active navigation background |
| Accent border | `rgba(255,102,204,0.38)` | Selected row and focus boundary |
| Danger | `#ff788c` | Destructive actions and failures |
| Success | `#7de2b4` | Completed operations |

State must never rely on color alone. Selected rows also use a marker/background,
current Tracks use a play glyph, active destinations use a filled surface and
label weight, and errors include words.

The TUI uses the terminal palette plus the same pink accent for active
destinations, current Tracks, labels, and visualizer output. It relies on
reverse-video selection and text markers in addition to color.

## Typography

GUI scale (logical pixels):

| Token | Size | Use |
|---|---|---|
| `FONT_HERO` | 28 | Page title |
| `FONT_SECTION` | 18 | Hero pane title, major section |
| `FONT_TITLE` | 15 | Entity and Track titles |
| `FONT_BODY` | 12 | Navigation, actions, primary metadata |
| `FONT_META` | 10 | Secondary metadata, counts |
| `FONT_LABEL` | 9 | Section labels, compact hints |

TUI hierarchy is textual rather than proportional:

- Header: product, library, destination.
- Context line: accent-colored current view or Track scope.
- Rows: current selection uses reverse video; current playback uses `▶`.
- Status and help occupy dedicated bottom lines.
- Secondary detail is dropped before primary content at narrow widths.

## Spacing and surfaces

The GUI spacing scale is 4, 8, 12, and 18 logical pixels (`GAP_XS`…`GAP_LG`).
Main content uses 24 px padding. Navigation rows use consistent 7–9 px vertical
padding. Cards use 12 px internal padding and 12 px corner radius. There is one
surface family; nested decorative borders are avoided.

The sidebar is divided into **Home**, **Library**, **Collections**, and a
bottom Settings destination. Import is the single primary sidebar action.

## Navigation

GUI destinations:

- **Home** — listening overview and library health.
- **Tracks** — canonical Track list, search, filters and sorting.
- **Artists** — canonical Artist index and detail.
- **Albums** — canonical Album index and detail.
- **Folders** — source-path browsing.
- **Playlists** — user playlists.
- **Smart Collections** — dynamic views.
- **Lyrics** — Tracks with managed lyrics.
- **Settings** — playback and library preferences.

Both interfaces open on Home. The GUI keyboard shortcuts `1`–`0` and `K` remain
available. The sidebar width adapts at approximately 1150 and 940 logical
pixels; labels are never removed, so destinations remain understandable.

TUI destinations are Home, Tracks, Artists, Albums, Playlists, Collections,
Lyrics, and Settings, reachable with `F1`–`F8`, `Tab`/`Shift+Tab`, or the
persistent sidebar. Home exposes the shared Recently Played, Recently Added,
Most Played, Favorites and Highly Rated collections. A persistent sidebar appears at 92×18 or larger while
browsing. Modal workflows and smaller terminals use the compact tab header.
Artist/Album details keep the same open/back semantics in both interfaces.

## Artist and Album presentation

- Artist index rows use Artist artwork when managed, otherwise a deterministic
  initial tile.
- Album index rows use the managed Album cover, otherwise a deterministic
  fallback.
- Artist detail labels physical ownership, explicit Album participation and
  Track-credit-only association distinctly.
- Album detail presents one hero surface: cover, title, owner, participation
  count, release type, Track count, and cover state.
- Album participants are listed with stable-ID navigation. Add is filtered and
  entity-based; removal is an association edit, not a deletion.
- Track credits remain separate from Album participants wherever both appear.

## Track rows and contextual actions

Track rows prioritize title, credits, Album, rating, and duration. At narrower
windows, Album and rating columns and the favorite shortcut are removed before
text is shrunk. Each row retains selection, current-playback and focused states
and one overflow menu. The menu contains Play next, Add to queue, Edit, Favorite,
rating actions, Open lyrics, Open Artist and Open Album where applicable.

A modal freezes the covered layer. Covered rows cannot receive pointer events,
operations or hover painting; only the top overlay and its backdrop participate.

## Filters and search

Search is always visible in the page header. Filters are collapsed by default
into a compact bar. Active filters appear as chips and the reset action appears
only when something is active. Sorting and direction remain available without
occupying a second permanent panel. Filters are hidden on fixed-order contexts
such as Artist and Album detail.

## Playback

- Exactly one primary timeline exists.
- The player bar contains current Track, artwork, transport, seek, volume,
  queue access and status.
- Home may show a continue-listening action, but it does not duplicate the
  timeline.
- The TUI timeline sits above status/help; header, side panel and player page do
  not repeat elapsed/total time.
- Queue is optional contextual content on the right at wide GUI widths and an
  overlay below the breakpoint. The TUI queue remains a dedicated destination.
- Visualizers use real engine waveform samples only. Silence draws an inactive
  baseline; paused/stopped states reset scope activity.

## Import

The import workflow remains source → review → copy/prepare → atomic commit.
Review exposes metadata provenance, canonical Artist/Album selection, additive
Album participants, Track credits, artwork/lyrics, duplicate state, and explicit
update/skip/import decisions. A single is not a mandatory classification. No
tags are written and no artwork suggestion silently replaces an explicit cover.

## Responsive behavior

GUI:

- Sidebar 232 px above 1150 logical pixels, 204 px above 940, otherwise 184 px.
- Inline queue at 1240 logical pixels and wider; otherwise queue opens as an
  overlay.
- Track rows drop Album/rating/favorite before compressing the title.
- Home stacks the continue-listening pane below the overview at narrow widths.
- Below 720 logical pixels of height, the sidebar now-playing card is hidden so
  every primary destination remains reachable without scrolling.

TUI:

- Persistent sidebar at 92×18 and larger during normal browsing.
- Compact tab/header layout below that; minimum supported layout is 20×6.
- Side panel appears only when content width is at least 86 and height at least
  10, so an 80-column terminal keeps the full list width.
- Required inspection sizes: 120×36, 92×24, 80×24, 40×12. All render without
  panic, with one timeline and a usable Track list.

## Motion and accessibility

Motion is limited to real playback visualization. There is no perpetual
decorative animation, and no animation delays input or playback. The GUI cannot
honor OS reduced-motion settings for the waveform without replacing the audio
visualization contract, so the visualizer is opt-in in the TUI and reflects real
audio only.

Icons: the GUI uses one vendored Lucide SVG family at 9–20 px with a consistent
stroke weight. Navigation, transport, row actions, filters and Smart Collections
share it. Icon-only controls require a tooltip or adjacent label; tooltips are
not screen-reader names.

Navigation hierarchy: Home, Tracks, Artists and Albums lead the sidebar.
Playlists, Smart Collections and Lyrics follow. Import is a secondary action
above Settings, not a primary destination. Folders is shown only when the active
library references user source paths; managed Album `tracks` folders are not
navigation. Queue and Lyrics are player controls; the Queue stays inline on wide
windows and overlays on narrower ones. Artist and Album pages are detail-first,
with editing behind a discreet action.

Keyboard operation: Tab/Shift+Tab cycle primary GUI destinations; ↑/↓ move
Track focus; Enter plays; E edits; Y opens lyrics; A queues; F favorites; I
imports; / focuses Search; Q toggles queue. Global shortcuts are suppressed
while editors, Setup, import and other modal workflows own input. Queue close and
Track-action close restore focus to Search where available.

Known accessibility gaps: no screen-reader audit, no complete GUI button
focus-ring/tab-order audit, no native high-contrast theme audit, and tooltips are
not equivalent to assistive-technology labels. Iced buttons are not the focus
target used by the app-level focus operations, so complete keyboard traversal of
every button is not implemented. TUI selection/focus is visible via reverse video
and markers. This document does not claim those audits occurred.
