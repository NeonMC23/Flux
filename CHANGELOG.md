# Changelog

All notable changes to Flux are documented here. Flux uses semantic versioning
for release artifacts while the workspace remains at `0.1.0` until the first
authorized release.

## [0.1.0] — 2026-11-21

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
