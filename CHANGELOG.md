# Changelog

All notable changes to Flux are documented here. Flux uses semantic versioning
for release artifacts while the workspace remains at `0.1.0` until the first
authorized release.

## [0.1.0] — 2026-11-21

### M27 interaction quality, accessibility and GUI/TUI parity

- Reworked GUI Tab behavior: Tab/Shift+Tab traverse focusable form controls;
  Ctrl+Tab/Shift+Ctrl+Tab cycle primary destinations. Visible focus styling and
  view-switch suppression inside editors are preserved.
- Added keyboard navigation to the queue overlay (select, play, remove, close)
  and to the Track action menu (up/down, Enter, Escape, visible selection).
- Added `M` to open Track actions for the focused/current Track.
- Added a persisted Reduce motion preference in Settings → Appearance; it
  freezes the continuously moving waveform while leaving playback unchanged.
- Implemented TUI import-review Album participant selection with `p`, `a`, `r`,
  existing-Artist disambiguation, owner separation, cancellation and import
  publication tests at 120x36, 80x24 and 40x12.
- Strengthened native identity tests for all canonical PNG sizes, ICO/ICNS
  headers, desktop identity fields and AppStream metadata.
- Documented keyboard behavior, accessibility limits, reduced motion, TUI
  participant workflow and remaining native validation in
  `docs/m27-interaction-accessibility.md`.

### M26 visual polish, playback stability and identity

- Raised the GUI reading scale across titles, rows, metadata, dialogs and
  controls. Added a persisted 100/110/120% text-scale setting in Appearance.
- Added five curated accent presets (Flux Pink default) persisted in shared
  settings and applied to selection, controls, sliders, scrollbars, queue and the
  visualizer.
- Added the canonical Flux mark to the sidebar and a local desktop-identity
  installer for development builds. Existing Linux/Windows/macOS packaging
  resources remain intact.
- Tracks now show inherited Album artwork thumbnails and durations. Missing
  artwork uses a neutral icon placeholder; missing audio remains a separate red
  state.
- Canonical imports persist technical duration. Older records lazily probe and
  cache duration without catalogue writes.
- Fixed player height jumps by reserving fixed progress-row geometry and
  truncating Track metadata to one line.
- Made TITLE, ALBUM, RATE and TIME column headers sortable with direction
  indicators; the advanced sort picker remains at the far right.
- Redesigned the Track context menu with artwork, icons, grouping, readable
  rating labels and explicit disabled states.
- Styled scrollbars as quiet neutral scrollers and added content insets so they
  cannot cover row actions. Settings is now scrollable.
- Preserved the waveform modes and curves; trace colours now follow the accent.
- Documented typography, scale, theme, identity, sorting, scrollbar and remaining
  motion limits in `docs/m26-visual-polish.md`.

### M25 player experience, icons and entity workflows

- Moved Import music from a primary sidebar button to a subdued action above
  Settings. Queue and Lyrics now open from compact player controls; Queue keeps
  stable entry identity, inline/overlay behavior and isolation.
- Added a contextual Lyrics player panel with current-line highlighting, full
  lyrics and editing entry points, without duplicating the playback timeline.
- Replaced GUI Unicode glyphs with a vendored Lucide SVG icon set (ISC license),
  compiled into the binary and used for navigation, transport, rows, actions and
  all Smart Collections. No runtime network access.
- Reworked Track editing: stable-ID primary-Artist selector with inline Artist
  creation; Artist-filtered Album selector including participating Albums and
  inline Album creation; reviewed Album move with save protection.
- Made Artist and Album pages detail-first. Metadata and relationship editors
  sit behind Edit details. Added a compact responsive Artist discography with
  artwork cards, separate owned/participating/credited Album groups, and Album
  play controls.
- Added ID3 APIC and MP4/M4A `covr` extraction tests alongside FLAC/Vorbis.
  Same-Album embedded artwork suggestions now resolve to one deterministic
  cover candidate. Added Album-artwork inheritance and independent Track
  override/replacement/removal coverage.
- Hid Folder navigation for canonical and portable libraries, where it exposed
  internal per-Album `tracks` folders; legacy source-path libraries keep it.
  Restyled Tracks filters as a compact toolbar rather than a permanent card.
- Improved the lyrics editor into readable per-line cards while preserving
  timestamps, Unicode, blank lines and karaoke conversion warnings.
- Documented the M25 product changes and remaining accessibility/native
  validation limits in `docs/m25-player-experience.md`.

### M24 workflow hardening and accessibility foundations

- Import review now distinguishes same managed file, same managed audio,
  duplicate-in-batch, metadata-only match, title collision, probable duplicate
  and ambiguous match. Exact duplicates default to Skip; explicit Update details
  is required for metadata/artwork repair and no duplicate Track is created by
  re-review.
- Import completion reports complete/partial/no-commit outcomes, identifies
  failed reviewed Tracks and explains that failed files were not partially
  catalogue-registered. Cancellation and atomic publication behavior are
  unchanged.
- GUI Track actions were moved from a clipped narrow pick list to a frozen-base
  modal action sheet with full labels, Track context, Escape dismissal and only
  working actions. Queue rows now play by stable QueueEntryId; drag and remove
  are separate labelled controls.
- The queue overlay now uses frozen covered-layer composition; closing it
  restores Search focus. Empty-queue guidance no longer references removed "+"
  row buttons.
- Collaborative Album participant removal in the GUI is now two-step and
  consequence-aware. Same-name Artists are disambiguated in GUI labels and TUI
  Artist rows/detail/owner/participant pickers.
- Added GUI keyboard navigation: Tab/Shift+Tab destinations, ↑/↓ Track focus,
  Enter play, E edit, Y lyrics, A queue, F favorite, I import. Current playback
  and keyboard focus now use distinct markers. Global shortcuts are blocked
  while editors, setup and modal workflows own input.
- TUI import review uses precise duplicate qualifiers and documents `u` for
  explicit update of a unique exact-audio duplicate; compact help is used below
  80 columns.
- Added regression coverage for duplicate status precision, idempotent
  duplicate review, partial import outcome labels, queue entry activation,
  participant confirmation, same-name Artist disambiguation, keyboard
  navigation/focus and input suppression in editors.
- Documented the audit, fixes and remaining accessibility limits in
  `docs/m24-interaction-hardening.md`.

### M23 product UX/UI redesign

- Added a shared product design language (`docs/design-system.md`) covering
  color roles, typography, spacing, surfaces, navigation, selection/focus, one
  playback timeline, context actions and GUI/TUI responsive rules.
- GUI shell redesigned around a Home-first sidebar with Library and Collections
  groups, adaptive 232/204/184 px navigation widths and a single Import action.
  Home is now the default destination.
- Home rebuilt around a library overview, real continue-listening context,
  recent artwork shelves, Top Artists/Albums and metadata/artwork coverage. No
  listening data is invented.
- Artist and Album index rows now use managed artwork with deterministic
  fallbacks. Artist/Album details gained hero surfaces, clearer relationship
  labels, participant presentation and responsive actions.
- Track lists now hide secondary columns and favorite actions before compressing
  primary text, retain one overflow menu, and preserve selected/current/focused
  states. Fixed-order detail views no longer show irrelevant filter controls.
- Inline queue now appears only at genuinely wide window sizes; narrower windows
  use the queue overlay so content does not become cramped. Player and page
  compact thresholds are independent of queue placement.
- TUI gained a Home destination with the shared Recently Played, Recently
  Added, Most Played, Favorites and Highly Rated collections (`F1`–`F8`), plus
  explicit-size rendering tests and evidence at 120×36, 92×24, 80×24 and 40×12.
  Duplicated now-playing metadata was removed: the sidebar rail defers to the
  content side panel when that panel is present, playback time appears only on
  the primary timeline, and redundant sidebar playback details were removed.
- Added responsive GUI/TUI regression tests for shell widths, Home rendering and
  required terminal dimensions.

### M22 canonical multi-artist Albums and schema migration

- Canonical schema 2 adds an explicit, ordered, many-to-many Album participant
  set while retaining `owner_artist_id` strictly as the physical directory and
  Track-owner role. Participation is independent of Track credits in both
  directions, and participant-less Albums remain valid.
- Schema 1 libraries upgrade atomically on open: every physical owner becomes
  the initial participant, all version-1 entity files are rewritten as schema 2
  with unknown JSON fields preserved, and the normal catalogue backup and C1
  journal provide rollback/roll-forward. Malformed schema-2 participant lists
  and unsupported versions fail closed.
- Artist pages now distinguish physical ownership, explicit Album participation
  and Track-credit-only reachability. Album pages list and navigate every
  participant and support searchable add/remove without touching tracks, audio,
  artwork or credits.
- Import review supports collaborative Albums: existing participants are
  preserved, additional participants can be added, and selecting a participant-
  owned Album changes only the physical Track owner while keeping the selected
  Artist credited. Album selection is now filterable.
- Artist deletion no longer cascades shared Albums: participant-only Artists are
  removed from associations without deleting releases, owned shared Albums
  require ownership transfer first, and owned Albums require explicit deletion
  confirmation. Resource cleanup remains post-commit and reference-safe.
- TUI Album detail exposes `Alt+p` participant navigation/editing with explicit
  removal confirmation, and Artist views include participating Albums.
- Added migration fixtures/tests for empty libraries, duplicate Artist names,
  collaborative credits, empty participant sets, interrupted/committed
  migration recovery, unknown-field preservation, participant editing, import
  into collaborative Albums, shared-Album deletion safety and GUI/TUI workflows.

### M21 product redesign and library trust

- Embedded artwork is now extracted read-only as a bounded import suggestion
  (PNG/JPEG/WebP, 16 MiB, 4096 px / 16 MP), never as canonical metadata and
  never written back to audio. Explicit sidecar/manual artwork always wins.
- Existing duplicate reviews retain artwork repair suggestions; they apply only
  through an explicit **Update details** decision, and **Skip** preserves the
  existing Track, metadata and artwork.
- Import review offers a primary existing-Album picker scoped to the chosen
  Artist, shows provenance, artwork readiness and duplicate **Open album/artist**
  actions, and disambiguates same-named Artists by stable ID.
- GUI redesign foundation: icon-led navigation, a restrained accent hierarchy,
  now-playing sidebar block, contextual Lyrics access moved out of the top bar,
  compact collapsible filters with active-state summary, Track overflow menus,
  and a frozen covered layer so hover cannot bleed through modals.
- Dashboard now derives Top Artists, Top Albums, metadata/artwork coverage and
  recent activity from real local plays and library state; no history is
  fabricated.
- TUI composition redesign: persistent destination sidebar on wide terminals,
  adaptive fallback to tabs, one primary playback timeline, a larger real
  visualizer, and balanced now-playing/queue context.
- Documented the Artist/Album/collaboration decision, including the rejected
  duplication model and the migration plan required before any explicit
  multi-artist Album schema.
- Added regressions for duplicate Artist identities, collaborative Album
  reachability, bounded embedded artwork, duplicate artwork repair, entity
  picker disambiguation, Track menus, collapsible filters, hover isolation,
  responsive sidebar thresholds and the single TUI timeline.

### M20.1 CI artifact verification

- Dispatched private packaging runs on the M20 source and, after documentation
  updates, on final candidate `2cb94ab`; downloaded the actual Linux, Windows
  and macOS workflow artifacts from run `44904831642`.
- Verified every published SHA-256 checksum and inspected payload identity,
  version, architecture, resources and signing state.
- Installed, upgraded/reinstalled and removed the actual x86_64 CI `.deb` and
  `.rpm` in disposable Ubuntu 24.04 and Fedora 44 containers, confirming
  glibc 2.39 compatibility and user-data sentinel preservation.
- Confirmed CI-native Windows installer/portable lifecycle and macOS first-run,
  ZIP/DMG mount and ad-hoc signature state.
- Added an exact Windows Authenticode and macOS Developer ID/notarization
  procedure in `docs/signing-and-notarization.md`.

### M20 release-candidate consolidation

- Added a glibc compatibility guard to Linux packaging. Host-local packages that
  exceed the supported Ubuntu 24.04 baseline are refused unless the explicit
  diagnostic override is set, preventing accidentally published incompatible
  `.deb`/`.rpm` artifacts.
- Rebuilt and validated portable, Debian and RPM payloads, first-run GUI startup,
  upgrade/reinstall sentinel preservation, removal behavior and checksums on the
  Linux audit host.
- Regenerated third-party notices and aligned AppStream/RPM release metadata and
  README status with the release-candidate state.
- Consolidated release-readiness evidence, host-package limitations and deferred
  native/manual validation in `docs/release-readiness.md`. Release TUI/GUI
  startup, first-run library creation, package extraction, upgrade sentinels,
  removal behavior and checksums were revalidated locally.

### M19 completion

- TUI Artist deletion transfer now uses explicit destination selection: choose a
  surviving owner, then an eligible existing album or a uniquely titled new
  album and release type. Auto-resolution remains a separate convenience action.
- TUI Album ownership transfer is available from Album detail with `Alt+t`,
  including per-track destination-credit decisions and readiness validation.
- Added regression coverage for interactive artist transfer and album transfer
  with credit resolution.
- Added `docs/release-readiness.md` with release blockers, signing state,
  deferred manual validation and verified packaging inputs.

### M18 completion

- TUI populated-artist deletion now reviews owned and credited tracks through the
  shared `ArtistDeletionPlan`: transfer to a surviving owner, remove a credit,
  explicitly delete twice, reset, or auto-resolve safe decisions. Publication is
  refused until the plan is ready.
- TUI import review supports fixed-target existing albums (`F`) and bulk release
  types (`1`–`5`) while preserving the review-first canonical transaction.
- The TUI was split into `render`, `import`, `forms`, `help`, `text` and
  `visualizer` modules, reducing `main.rs` by roughly a quarter without moving
  domain rules out of `flux-app`.
- Regression tests cover populated-artist transfer deletion, fixed-target import
  and bulk release type application.

### Feature completion

- TUI album order and membership now use shared reviewed plans: `Ctrl+↑/↓`
  reorders canonical album tracks, `Alt+m` moves a track to an owner album or
  standalone state, and stale plans remain rejected.
- TUI safe deletion covers tracks, empty albums and track-free artists with
  explicit confirmation and shared generation/session checks. Placement,
  ratings, lyrics, artwork, queue, history and managed-resource cleanup follow
  the existing canonical deletion semantics.
- TUI managed-audio replacement/repair (`Alt+r`), lyrics export, and import
  review attachment of album artwork, track artwork and lyrics close the main
  terminal feature gaps. Typed paths replace the old `track-id|path` forms for
  artwork, lyrics and replacement.
- GUI queue clearing now exposes a confirmed "Clear queue" action backed by the
  shared Controller operation.
- The TUI renderer's pure text/width helpers and waveform visualizer were split
  into `text` and `visualizer` modules; canonical business logic remains in
  `flux-app`.
- Documentation and the parity matrix were reconciled with the implemented
  workflows and remaining GUI-only review operations.

### Terminal player

- The TUI is now a complete keyboard-first player: Tracks, Artists, Albums,
  Playlists, Collections, Lyrics and Settings destinations; artist and album
  detail navigation; structured prefilled metadata forms; playlist create,
  rename, delete, entry add/remove/reorder and playback; ratings, favorites,
  history collections and audio-device selection.
- Five persisted layouts (Standard, Library, Player, Visualizer, Compact), a
  command palette, contextual help and an explicit queue with follow, jump and
  confirmed clear.
- A real audio visualizer with Waveform, Bars and Pulse modes, shared
  `WaveformScope` processing and the engine's post-gain waveform tap. Hidden
  visualizers stop sampling.
- The GUI and TUI now share smart-collection queries and queue clearing through
  `flux-app::Controller`; neither frontend duplicates domain rules.
- PTY captures and automated tests cover navigation, forms, queue confirmation,
  playlist state, visualization data mapping, preference persistence and a
  5,000-track performance fixture.

### Usability

- The ordinary import now follows Source → Destination → Verify → Import. The
  review leads with title, artist, album, artwork, lyrics and a concise status;
  track/disc numbers, credits, existing Album selection, release metadata,
  bulk actions, direct paths and detailed duplicate handling live under one
  collapsed **Advanced Options** section.
- The sidebar was reduced to primary library destinations, a small Browse group
  (including Folders, Lyrics, Smart Collections and Dashboard) and Settings. Counters, shortcut reminders, technical library identity and
  duplicate setup actions were removed.
- Setup now centers on the active library and audio output, with first-run text
  and advanced Artist/Health/Migration tools collapsed. Library filters are
  collapsed behind **Filters**, and the Track editor leads with title, artist,
  album, artwork and lyrics while ownership, credits, replacement, membership
  and deletion controls are under **Advanced Options**.
- TUI headers and import wording were simplified without changing shared
  Controller behavior or persistence.

### Local library

- Canonical self-contained libraries with stable artist, album, track, playlist
  and queue identities, transactional publication and startup cleanup recovery.
- Artist, Album and Track editing, aliases, descriptions, ownership/credits,
  optional Album membership, standalone Tracks, empty Albums and playlists.
- Managed Artist images, Album covers and per-Track artwork overrides with
  previews, missing/inaccessible states, repair paths and safe cleanup.
- Managed lyrics import/discovery, LRC/TXT editing, karaoke display, atomic
  export and native save dialogs.

### Import

- Review-first local import: filename/folder inference, local lyrics/artwork
  sidecars, provenance, conflict notes, credits/owner selection, per-candidate
  release type, bulk metadata and lyrics/artwork actions.
- Exact/byte-length-aware duplicate detection, cancellable fingerprinting,
  progress and stale-plan rejection.
- Cancellable review-first TUI importer with explicit decisions, progress and
  honest final-transaction limits.
- A cancel request that arrives between copy preparation and catalogue
  publication now aborts before the irreversible commit and removes unpublished
  copies; the commit phase is explicitly non-cancellable.

### Playback and experience

- WAV, FLAC, MP3, AAC/M4A, Ogg Vorbis and Opus playback, seek, gapless/crossfade,
  queue identity, history, ratings, favorites, playlists and oscilloscope.
- `--audio-diagnostics` in the GUI and TUI reports enumerated devices, the
  selected/default stream, sample rate/channels, and whether stream startup
  actually succeeded.
- Compact dark GUI (`#ff66cc` accent), terminal interface, native pickers,
  keyboard navigation and dirty-edit protection.
- TUI path-based Artist name/aliases/description/image, Album title/release
  type/cover, Track artwork override and managed-lyrics replacement workflows,
  all through the shared Controller.

### Release engineering

- Canonical Flux icon set (SVG, PNG, ICO, ICNS), window/taskbar identity and
  `--version`/`--help` release probes.
- Portable Linux archive, `.deb` and `.rpm` packaging with desktop/AppStream
  integration, documentation, license notices and SHA-256 checksums.
- Windows resource/manifest and portable/Inno Setup packaging scripts.
- macOS `Flux.app`, ZIP/DMG, Info.plist, signing hooks and universal-binary
  support.
- Manual-dispatch CI packaging jobs and platform artifact verifiers.
- Disposable Ubuntu/Fedora package-manager lifecycle verification with real
  `.deb`/`.rpm` install, synthetic older-version upgrade, reinstall and removal.
- Optional Authenticode/Developer ID signing paths that fail closed when signing
  is explicitly required; credentials remain external.
- Reproducible Linux artifacts with `SOURCE_DATE_EPOCH`, including normalized
  RPM build time/host.

No release tag or public binary publication is created by this repository.
