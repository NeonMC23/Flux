# M25 player experience and entity workflow

Status: implemented and automated-tested; runtime-inspected on Linux. Human
usability, screen-reader and native Windows/macOS interaction audits remain
pending.

## Product hierarchy

M25 moves Flux further from a catalogue editor and toward a music player:

1. Listening controls stay in the persistent player bar.
2. Home, Tracks, Artists and Albums are primary destinations.
3. Queue and lyrics open from the player as contextual side panels.
4. Import is a secondary action near Settings.
5. Metadata editing is reached from a Track, Album or Artist detail page.

The shared Controller, canonical schema 2, stable IDs, playback engine, queue
identity and transaction rules are unchanged.

## Navigation

- The GUI sidebar keeps Home, Tracks, Artists and Albums visible.
- **Import music** moved from a prominent primary button to a subdued action
  immediately above Settings. Import remains reachable from Home and from the
  Import shortcut (`I`).
- **Folders** is shown only for a legacy source-path library. Canonical and
  portable libraries keep audio under Flux-managed Album folders whose
  `tracks` directories are implementation detail; Tracks and Albums are the
  replacement navigation. Import source browsing is unaffected.
- The sidebar's NOW PLAYING card links to the Track but no longer duplicates
  the lyrics action.
- Primary keyboard destinations are `1` Tracks, `2` Artists, `3` Albums,
  `4` Playlists, `5` Smart Collections, `6` Lyrics, `7` Home, `8` Recently
  Added, `9` Recently Played, `0` Most Played, `K` Settings and `L` Setup.
  Tab/Shift+Tab follow the destinations that are actually visible. `G` opens
  Folders only where source-path browsing is meaningful.

## Player, queue and lyrics

- The player bar now owns the Queue and Lyrics entries as compact labelled
  icon buttons with tooltips.
- Queue keeps `QueueEntryId` identity, click-to-play, drag-handle reordering,
  explicit removal, pagination, overlay isolation and responsive inline/overlay
  placement.
- The queue overlay uses a smaller now-playing artwork block so upcoming
  entries remain visible at 900×650.
- The Lyrics player entry opens a contextual panel over the current view. It
  shows synchronized or plain text, highlights the current synchronized line,
  and offers **Open full lyrics** and **Edit lyrics**. It does not replace the
  full Lyrics destination or the karaoke mode.
- Only one contextual panel is shown at a time. Escape closes it and restores
  Search focus.

## Icon system

- The GUI uses a curated vendored subset of [Lucide](https://lucide.dev) SVG
  icons. Lucide is ISC-licensed; the license and pinned upstream commit are in
  `crates/flux-gui/assets/icons/`.
- Icons are compiled into the binary and rendered with Iced's SVG backend.
  There is no runtime network dependency.
- Sidebar destinations, transport controls, repeat/shuffle, favorite, row
  actions, filter/sort, queue drag/remove, artwork actions and all eight Smart
  Collections use the icon family. Smart Collections no longer use initial
  letters.
- Icon-only controls have tooltips or adjacent text labels. Tooltips are not a
  substitute for screen-reader names; that limitation is unchanged.
- The TUI keeps terminal-safe Unicode where it is readable and intentional.

## Track editing

- **PRIMARY ARTIST** is a stable-ID selector with disambiguation for same-name
  Artists. It edits the first Track credit; the physical owner remains separate.
  Changing it preserves the other credits and never transfers the Album owner.
- A discreet **New Artist** action opens an inline name field and refreshes the
  selector after creation without leaving the editor.
- **ALBUM** is a stable-ID selector filtered by the selected Artist. It includes
  Albums owned by the Track's physical owner where that Artist participates or
  is credited. Album labels identify a different owner and duplicate titles
  receive an ID. An Album owned by another Artist is not offered as an immediate
  move target: canonical ownership requires **Transfer ownership** first. This
  is stated in the editor rather than simulating a cross-owner move.
- A discreet **New Album** action creates an Album owned by the selected Artist
  and selects it as a move destination.
- Choosing a different Album exposes Review move/Confirm move; saving metadata
  while a move is pending is blocked with a clear message. Standalone conversion
  remains available. The source Album is retained even if it becomes empty.
- Album title editing belongs to the Album detail editor, not the Track editor.

## Artist and Album pages

- Artist detail is now detail-first: artwork, identity, description, aliases,
  discography, participating Albums, appearances and Tracks. Metadata and image
  controls live behind **Edit details**.
- The discography uses compact responsive artwork cards (two to five columns,
  based on available width) with title, release type/relation and Track count.
  No release year or other unavailable metadata is fabricated.
- Album detail shows artwork, owner, participants, Track count and artwork
  status first. Participants are visible read-only; metadata, participant
  editing and cover management live behind **Edit details**.
- Album detail now has **Play album** and per-Track play controls, in addition
  to Track reorder/edit actions.
- Stable IDs, owner/participant separation, credits and deletion rules are
  unchanged.

## Artwork

- Extraction still uses `lofty` read-only tag inspection with existing bounds:
  16 MiB per image, 512 MiB audio container for inspection, 4096 px and 16 MP
  decoded limits.
- Tests now cover FLAC/Vorbis pictures, ID3 APIC in MP3 and MP4/M4A `covr`
  pictures using small generated fixtures.
- Album artwork suggestions for several files in one Album group are resolved
  deterministically: the first manual choice wins, otherwise the first
  discovered suggestion is reused for the group. Explicit manual choices are
  not overwritten. One consistent cover is published instead of several
  identical files or a conflict.
- A Track without an override inherits its Album cover. Track overrides remain
  independent; removing one restores Album inheritance and never removes the
  shared Album cover. Tests cover inheritance, replacement and removal.
- Album artwork continues to propagate to Artist discography, Album browsing,
  Track lists, player, queue and the contextual lyrics panel.

## Browsing, filters and Folders

- The Tracks toolbar is no longer a permanent card. Filters stay behind a compact
  Filters control with active count; active chips and Reset remain visible when
  collapsed. Sorting remains in the toolbar and is unchanged.
- Album and Artist lists already use managed artwork thumbnails when available;
  missing artwork uses a neutral fallback.
- Folder navigation is hidden for managed libraries as described above and
  remains available for legacy source-path libraries.

## Lyrics

- The player now provides a contextual lyrics panel.
- The editor keeps per-line text/timestamp fields, playback-time capture,
  ±100 ms/±1 s nudges, clear-time, reorder and delete. Each line is now a
  two-row card with a readable text field and a separate timing row instead of
  one dense toolbar.
- Timestamp preservation, plain/synchronized conversion warnings, Unicode,
  blank lines and stale-edit rejection are unchanged and covered by existing
  tests.

## Limitations

- No screen-reader/assistive-technology audit.
- Iced buttons are not the target of the app-level focus operations; navigation
  and Track actions use app shortcuts rather than a complete button focus ring.
- No reduced-motion preference; M25 added no non-essential animation.
- TUI import review still cannot add Album participants during review.
- Native Windows/macOS runtime interaction was not manually validated.
- The GUI import review was not driven end-to-end manually in this milestone.
