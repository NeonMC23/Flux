# Changelog

All notable changes to Flux are documented here. Flux uses semantic versioning
for release artifacts while the workspace remains at `0.1.0` until the first
authorized release.

## [0.1.0] — 2026-11-21

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
