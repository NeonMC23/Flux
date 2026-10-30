# Canonical Flux library foundation

This document records the canonical storage design and the application
integration state. The canonical engine is now the normal GUI/TUI runtime
backend: both front-ends open, create and switch canonical libraries through the
shared Controller. Legacy and portable schema-v3 formats are explicit migration
sources; normal front-end workflows no longer select or write them. The public
Controller still retains compatibility constructors for migration/tests, so
legacy retirement at the API level is not complete.

## Audit findings

- The original runtime had two persistent backends: the legacy app-data
  `library-index.json` (`store.rs`) and the portable schema-v3 segmented
  catalogue (`portable.rs`, `.flux/`). The integrated Controller now defaults
  to canonical (`open_default`, `new_canonical`, `switch_to_canonical`); the old
  constructors remain compatibility/migration APIs.
- Schema v3 already has stable `TrackId`/`ArtistId`, sparse overrides,
  immutable track segments, catalogue backups, generation checks, path
  validation and an explicit legacy migration (`migrate.rs`).
- The legacy scanner still uses `lofty` (`scan.rs`), but canonical startup,
  import inference, editing and migration never call it. The review importer
  now derives metadata only from filenames/folders.
- Tracks are physically addressed by absolute paths in the runtime library and
  queue data. Portable records are relative, but the legacy index and runtime
  queue are not guaranteed to be library-owned.
- AlbumId, playlist, rating/favorite and managed-lyrics runtime types now exist
  in the canonical engine. GUI/TUI screens for playlists/ratings/lyrics are not
  implemented.
- The GUI/TUI Import Manager is review-first and copy-only. Reference imports
  are rejected in canonical mode; the old portable import tests still cover
  their compatibility behavior.
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

## Known gaps

1. The public Controller still exposes the old legacy/portable constructors and
   switches for migration compatibility and existing tests. No GUI/TUI normal
   workflow uses them, but the API-level retirement is incomplete.
2. Import review supports one credited owner artist; ordered featuring credits,
   release-type review and physical-owner selection are not exposed yet.
3. Import copy/publication runs synchronously at the canonical commit boundary;
   cancellation is safe before commit, but there is no background byte-copy
   progress for very large files.
4. Artist ownership transfer/deletion with collaborative tracks and album moves
   is not implemented. Changing physical ownership is refused; destructive
   deletion is not exposed.
5. Artwork is copied/validated by extension and bounded size only; there is no
   image decode/dimension validation or display and no native image picker.
6. Playlists, ratings/favorites and lyrics have canonical Controller APIs and
   persistence tests, but no GUI/TUI screens.
7. Automatic catalogue-backup rollback is not generation-consistent and is
   disabled; transaction-journal recovery is implemented and tested.
8. Migration is application-level and bidirectional in the normal UI, but the
   execute phase is synchronous; very large migrations need background progress
   and cooperative cancellation.
9. Cross-platform native behavior and real GUI migration were not runtime
   verified in this stage.
