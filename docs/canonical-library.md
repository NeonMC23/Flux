# Canonical Flux library foundation

This document records the audited legacy/portable architecture and the canonical
storage foundation implemented in `flux-library`. **The canonical engine is not
yet the GUI/TUI runtime backend.** The existing Controller still opens the
legacy index or schema-v3 portable store. Do not describe the migration as
complete until the integration stages below are implemented and tested.

## Audit findings

- The runtime has two persistent backends: the legacy app-data
  `library-index.json` (`store.rs`) and the portable schema-v3 segmented
  catalogue (`portable.rs`, `.flux/`). `flux-app::Controller` selects between
  them (`new_portable`, `open_default`, `switch_to_*`).
- Schema v3 already has stable `TrackId`/`ArtistId`, sparse overrides,
  immutable track segments, catalogue backups, generation checks, path
  validation and an explicit legacy migration (`migrate.rs`).
- Track metadata scanning still uses `lofty` (`scan.rs`) to read embedded tags
  for the existing import flow. The new canonical paths never call it.
- Tracks are physically addressed by absolute paths in the runtime library and
  queue data. Portable records are relative, but the legacy index and runtime
  queue are not guaranteed to be library-owned.
- There is no AlbumId, playlist persistence, managed artwork or managed lyrics
  in the current runtime model. Ratings/favorites are not persisted today.
- The GUI Import Manager is review-first and supports copy/reference; reference
  mode is incompatible with the canonical ownership rule.
- No external image-decoding dependency is available without approval.

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
  transaction ID. It is published last and is the commit record.
- `artist.flux`, `album.flux`, `track.flux`, playlist files and `ratings.flux`
  are authoritative for their metadata. Redundant owner/album/credit values are
  validated against the catalogue; the catalogue wins for relationships and
  paths. A mismatch is rejected rather than silently repaired.
- Album track order lives in `album.flux`; every referenced track must exist and
  belong to that album. Track owner must equal album owner; the first credited
  artist must be the owner, with featuring artists as additional references.
- Audio tags are never read or written. Stored duration comes from Flux
  metadata (or is absent); content fingerprints are streamed bytes.
- Cover and lyrics are optional managed files with relative, validated names.
  A missing cover does not invalidate an album.

## Transactions and recovery

- A batch writes `transactions/<txn>/journal.flux` before changing files,
  backs up replaced files inside the transaction directory, installs data with
  same-directory temporary files plus renames, publishes `catalog.flux` last,
  marks the journal committed, then removes backups.
- Open scans journals before normal use. A journal whose transaction ID matches
  the committed catalogue is cleaned up; an uncommitted journal restores
  backups and removes only journaled created files. Malformed journals stop
  open rather than deleting data.
- Paths with absolute components, traversal, non-UTF-8 canonical text or any
  symlink component are rejected. Audio may be missing at runtime, but migration
  blocks on missing source files.
- Catalog backups are retained as recovery artifacts. Automatic rollback from a
  catalog-only backup is deliberately **not** implemented: metadata files can
  advance independently, so a stale catalogue would be inconsistent. This is a
  known limitation requiring a future generation-consistent snapshot.

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

## Known gaps before declaring the redesign complete

1. `flux-app::Controller`, GUI and TUI still use the legacy/portable runtime;
   the canonical engine is not wired to playback, queue, import UI or startup.
2. Existing import metadata still reads audio tags. Floor inference from
   filenames/directories, review UI and canonical copy-only publication are not
   integrated.
3. Artist ownership transfer/deletion with collaborative tracks, album moves
   and collision planning is not implemented.
4. Artwork is copied/validated by extension and bounded size only; there is no
   image decode/dimension validation and no native image picker.
5. Lyrics are storage-only; playlists/ratings APIs exist but are not exposed in
   the GUI/TUI.
6. Automatic catalogue-backup rollback is not generation-consistent and is
   therefore disabled; transaction-journal recovery is implemented and tested.
7. No legacy/portable runtime write path has been removed. Migration
   compatibility remains in the old modules until integration is complete.
8. Cross-platform native behavior and real GUI migration were not runtime
   verified in this stage.
