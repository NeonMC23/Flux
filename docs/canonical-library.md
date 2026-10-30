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
  belong to that album. Track owner must equal album owner and must be one of the
  ordered credited artists. The first credit is the default owner at import time;
  an explicit review decision may select a later credit as physical owner.
  Featuring/collaboration is represented by additional ordered credits, never by
  duplicate audio.
- Audio tags are never read or written. Stored duration comes from Flux
  metadata (or is absent); content fingerprints are streamed bytes.
- Cover and lyrics are optional managed files with relative, validated names.
  A missing cover does not invalidate an album.

## Credits and physical ownership

A track stores an ordered `credited_artist_ids` list plus exactly one
`owner_artist_id`. The owner must appear in the credits and must match the album
owner because the album directory is the physical container. The owner need not
be first after an explicit ownership choice. Artist discographies resolve both
owned and credited tracks from the same TrackId without copying audio.

Canonical import inference creates one provisional credit from the folder/file
name. The GUI review can add existing artists, add named credits, remove/reorder
credits, and choose the physical owner. Names are matched to existing identities
only when exactly one exact-name identity exists; ambiguous names require an
explicit selection. Tag data is never consulted.

## Ownership transfer

`plan_ownership_transfer` is read-only. It requires the destination artist to be
credited, requires an existing destination album owned by that artist or an
explicit new album title, and rejects collisions before any write. Execution
copies the audio and managed lyrics to new canonical paths, updates both album
files and the track record, and publishes the catalogue last. Old audio/track
records are listed as journal-owned cleanup and are removed only after the
catalogue commits. If cleanup is interrupted, the committed journal removes the
old files on the next open. TrackId, ordered credits, album order, lyrics,
ratings, playlists and queue references are preserved. A retry against the
committed state is an explicit no-op.

## Artist deletion

Artist deletion is a complete explicit workflow, not a directory removal.
`plan_artist_deletion` builds a generation-tagged preview with:

- every owned track, album, audio and lyrics resource;
- every credited-only track owned by another artist;
- ordered credits, physical owners, ratings and playlist references;
- owned albums, including empty albums and managed covers;
- eligible surviving owners and all blocking/resource issues.

Each owned track must receive an explicit decision:

- **Transfer** to an eligible surviving credited artist, into an existing album
  owned by that artist or an explicitly named new release;
- **Delete** after an explicit irreversible confirmation.

Each credited-only track must receive **RemoveCredit**. Audio, TrackId, owner,
album and ratings remain unchanged; the deleted artist is removed from the
ordered credit list. The physical owner must remain credited.

Unresolved tracks, no surviving owner, empty resulting credits, missing
resources, invalid destinations, collisions, malformed playlists/ratings or a
changed catalogue block execution. The plan records the catalogue generation
and per-track album/owner/credit/audio/lyrics/rating snapshots; any relevant
change rejects the plan as stale.

Execution is **one** canonical transaction, not a sequence of transfers followed
by an artist delete:

1. revalidate the complete snapshot and every decision;
2. stage all transferred audio/lyrics and all updated track/album metadata;
3. stage playlist and ratings updates;
4. publish staged resources and entity metadata;
5. publish `catalog.flux` with the complete final state: transfers, credit
   removals, deleted tracks, removed artist/albums and new albums;
6. remove obsolete audio, lyrics, album files/covers and `artist.flux` only as
   journal-owned cleanup;
7. finalize the journal.

Transfers preserve TrackId, ordered surviving credits, playlist/rating
references, lyrics and queue identity. Explicit track deletion removes audio,
track `.flux`, lyrics, rating and playlist entries in the same transaction.
Queue entries are pruned by the application Controller after commit, and
playback stops if the current track was deleted. The artist is never removed
while an owned track or credit reference remains.

The GUI opens **Delete artist** from the Artists setup card, shows the complete
preview, edits per-track decisions and destinations, shows playlist/rating/lyrics
consequences, blocks unresolved plans, and commits with an explicit destructive
button. Cancellation before commit clears the preview and writes nothing. The TUI
intentionally exposes no destructive deletion shortcut; it directs users to the
GUI review workflow. No independent TUI transaction logic exists.

The multi-item deletion path runs the same 18-point injected interruption matrix
plus real child-process aborts at after-journal, metadata publication,
post-catalogue commit and cleanup. Recovery is verified as complete-old or
complete-new, never a partially deleted artist; a second recovery is idempotent.

## Transaction state machine and recovery

Ownership transfer uses the canonical batch journal. The persisted states are:

| State | Meaning | Recovery decision |
|---|---|---|
| `prepared` | Journal is durable; resources are not yet staged. | Uncommitted: roll back. |
| `copying` | Destination resources are being staged inside the transaction directory. | Uncommitted: roll back; staging is discarded. |
| `verified` | All staged copies passed size/content verification. | Uncommitted: roll back. |
| `publishing` | Staged resources may be replacing entity files; catalogue is not yet authoritative. | Uncommitted unless the catalogue records this transaction ID: roll back. |
| `committed` | The catalogue was atomically published and records this transaction ID. | Finish cleanup only if the catalogue records the ID; otherwise recovery-required. |
| `cleaning` | Obsolete source resources are being removed. | Same catalogue-ID check, then finish cleanup idempotently. |
| `completed` | Cleanup finished; only journal removal remains. | Same catalogue-ID check, then finish cleanup/remove journal. |
| `rolled_back` | Reserved for an explicitly recorded rollback. | Treated as uncommitted; rollback is idempotent and the journal is only then removed. |
| `applying` | Legacy journals from the earlier implementation. | Treat as uncommitted unless the catalogue records its ID. |

`Planned` is the in-memory ownership plan and creates no persistent mutation.
The **single authoritative commit point is `catalog.flux` containing the
transaction ID**. Destination files existing alone never imply commitment.
A malformed, truncated, unknown-version, missing or ambiguous journal stops open
with a recovery-required error and preserves the transaction directory and
valuable files.

### Persistence sequence

1. Create `transactions/<txn>/`, synchronize the transactions directory, back up
   every replaced target, and synchronize the transaction directory.
2. Write `journal.flux` with `prepared`, using a synced temporary file, rename,
   and parent-directory sync.
3. Stage every byte payload and audio/lyrics copy inside the transaction
   directory, syncing each staged file. Verify expected content fingerprints.
4. Write `verified`, then `publishing`.
5. Publish staged entity files with same-filesystem renames and parent-directory
   syncs.
6. Publish `catalog.flux` last. Its transaction ID is the commit record.
7. Write `committed`, `cleaning`, remove only journal-owned obsolete resources
   (checking the active catalogue never references them), write `completed`, then
   remove the transaction directory.
8. On any error before catalogue publication, rollback restores backups and
   deletes only transaction-created targets. On interruption, no rollback runs;
   reopen follows the journal/catalogue decision above.

Staging inside `transactions/<txn>/` prevents partially copied resources from
appearing as destination files. Cross-filesystem source copies are performed as
verified copy-then-publish; target publication uses same-root renames. Files are
never removed merely because they are unreferenced by a partial catalogue.

### Transaction concurrency and pending work

Normal writes are single-transaction: before creating a journal the engine
checks `transactions/` and refuses to start if any transaction directory is
present. This prevents a retry from writing a second transaction while a failed
rollback still owns earlier backups. Recovery can process multiple pre-existing
journal directories independently (for example after external corruption or
copied crash artifacts); committed cleanup and uncommitted rollback are decided
per transaction and one transaction never uses another's resources. The
catalogue transaction ID remains the only commit authority.

### Partial journal writes

`write_atomic` creates `journal.flux.tmp`, writes and syncs content, renames it
to `journal.flux`, then syncs the parent. Tests cover the observable artifacts:
missing journal after directory creation, incomplete temporary journal, truncated
final journal, complete temporary journal before rename, and a complete journal
after rename before parent sync. Missing/truncated artifacts stop open and
preserve the directory; a complete uncommitted journal is rolled back. Low-level
write failures inside `write_atomic` itself are not injected because the
standard library does not expose a portable hook; the closest deterministic
filesystem states are tested instead.

### Real process termination

Dedicated child test processes abort without unwinding at:
- after journal preparation,
- during audio staging,
- after metadata/resource publication before catalogue publication,
- immediately after catalogue publication,
- during post-commit cleanup,
- during recovery cleanup.

The parent reopens the same disposable library, verifies old/new authority,
audio and lyrics bytes, TrackId/credits/ratings/playlists, then reopens again to
prove idempotence. The recovery child is aborted inside cleanup and the parent
resumes it; journal and file remain until the successful retry.

### Failure-injection matrix

Test-only, deterministic boundaries are injected through a scoped
`TransactionFailure` value; production has no user-facing control. The transfer
matrix exercises all 18 points below with an abrupt interruption, then drops the
library, reopens/recoveries twice, and checks the old-or-new authority, exact
audio/lyrics bytes, owner, album, credits, ratings, playlists, TrackId and one
physical audio copy:

1. before journal creation;
2. after durable journal preparation;
3. before staging;
4. during generic staging;
5. during audio staging;
6. after audio staging before verification;
7. during lyrics staging;
8. after copy before verification;
9. after verification;
10. before metadata publication;
11. after a metadata/resource publication step;
12. before catalogue publication;
13. immediately after catalogue publication;
14. before cleanup;
15. during source-audio/record cleanup;
16. during source-lyrics cleanup;
17. after cleanup;
18. before journal finalization.

Additional tests cover rollback retry, referenced-path cleanup refusal, and
malformed/truncated/unknown-version/orphan journals. Recovery is idempotent and
preserves ambiguous resources.

### Durability limits

The implementation flushes journals and staged files and synchronizes parent
directories after critical renames on Unix. It cannot promise stronger
power-loss guarantees than the filesystem/device cache honors, nor atomic
multi-file visibility across independent renames. The catalogue is the sole
commit record, so those renames are reconciled by journal rollback/cleanup.
Unsupported or read-only storage fails visibly; no automatic repair is attempted
for a malformed journal. Catalog-only backup restoration remains disabled because
it is not generation-consistent with per-entity files.

Filesystem tests on Unix cover read-only destination directories, pending-write
rejection after a permission-blocked rollback, destination symlink replacement,
and missing source audio. Cross-device library roots and ENOSPC were not
available to test here; staging uses copies inside the transaction directory and
publishes with same-root renames, so a true cross-device source is copy-based,
but no second mounted filesystem test is claimed. Non-Unix directory-sync
semantics are not independently verified.

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
2. Import review supports ordered credits, owner selection, add/remove/reorder,
   and existing-artist selection. A separate featuring boolean and release-type
   picker are not exposed; the canonical model stores order only.
3. Import copy/publication runs synchronously at the canonical commit boundary;
   cancellation is safe before commit, but there is no background byte-copy
   progress for very large files.
4. Single-track ownership transfer and multi-item artist deletion are implemented
   with copy-first publication, journal cleanup and no-op retry. Whole-album
   moves are not implemented. The GUI exposes artist deletion and transfer; the
   TUI refuses transfer/deletion and points to the GUI.
5. Artwork is copied/validated by extension and bounded size only; there is no
   image decode/dimension validation or display and no native image picker.
6. Playlists, ratings/favorites and lyrics have canonical Controller APIs and
   persistence tests, but no GUI/TUI screens.
7. Automatic catalogue-backup rollback is not generation-consistent and is
   disabled; transaction-journal recovery is implemented and test-injected at
   every listed boundary. A recovery-required malformed journal requires manual
   inspection; it is preserved rather than destructively repaired.
8. Migration is application-level and bidirectional in the normal UI, but the
   execute phase is synchronous; very large migrations need background progress
   and cooperative cancellation.
9. Cross-platform native behavior and real GUI migration were not runtime
   verified in this stage.
10. Artist deletion execution is exposed in the GUI only. The TUI deliberately
    refuses deletion/transfer and points to the GUI; this is a product limitation,
    not a separate transaction implementation.
11. ENOSPC and a true second mounted filesystem were not available for testing.
    Low-level `write_atomic` interruption is represented by deterministic journal
    artifacts rather than an in-process portable filesystem hook. A real process
    abort is tested, but not a kernel/power failure.
