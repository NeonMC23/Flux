# Library architecture decision: Artists, Albums, Tracks and collaborations

Status: **accepted for M21; no schema migration applied.**

This document records the current canonical model, the alternatives considered
for collaborative releases, the decision taken for this milestone, and the
migration work that would be required before any explicit multi-artist Album
schema could be introduced.

## Current model (schema version 1)

The canonical store already uses stable entities and explicit relationships;
Track rows do not own copies of Artist or Album strings.

```text
Artist (stable ArtistId, name, aliases, description, image)
  └── Album (stable AlbumId, owner ArtistId, title, release type, cover,
             ordered TrackIds)
        └── Track (stable TrackId, ordered credited ArtistIds,
                   physical owner ArtistId, optional AlbumId,
                   track/disc numbers, lyrics, optional artwork override)
```

- `CanonicalArtist.id` is the identity. Display names are not unique keys.
  Import resolution only reuses an Artist by an exact stable reference or an
  unambiguous exact normalized name; otherwise it proposes a new Artist.
- `CanonicalAlbum` is owned by one Artist. The owner supplies the physical
  directory, the Album cover and the consistency boundary for membership.
- `CanonicalTrack.credited_artist_ids` is an ordered list. The first credit is
  the default owner at import time; the physical owner may explicitly be a
  later credit. This expresses collaborations and features without duplicating
  audio.
- `Controller::albums_credited_for(artist)` returns Albums that the Artist
  reaches only through Track credits. Artist detail pages combine owned and
  credited Albums, so a guest artist can navigate to a collaborative release
  without owning its files.

This is already an entity model. The M21 investigation did not find a need to
replace it; it found gaps in selection, duplicate presentation and artwork
repair, which are addressed in `flux-app` and the GUI rather than by a
destructive rewrite.

## Alternatives considered for collaborative Albums

### 1. One canonical Album associated with multiple Artists

`Album.artist_ids: Vec<ArtistId>` (or a join table) plus one owner for physical
resources.

- Duplicate identity: strongest — one title, one cover, one order.
- Track membership/order: unchanged.
- Credits: Album-level participants become first-class and are independent of
  which Tracks happen to credit an artist.
- Deletion: deleting one associated Artist must not delete the Album or shared
  audio. Ownership would need an explicit transfer step before Artist deletion.
- Artwork: single Album cover, no conflicting copies.
- Editing: all artists edit the same entity; conflicts are ordinary concurrent
  library edits.
- Resource references: unchanged audio reference counting; only the Album's
  artist set changes.
- Import matching: must choose the participant set, not just one owner.
- Migration: **breaking**. `owner_artist_id` becomes a subset of a new required
  field, or a new join entity is added. Every reader/writer, album path
  ownership rule, transfer flow, deletion planner and TUI/GUI album picker must
  change. A rollback would need to choose one owner and drop the extra links,
  losing information.
- Backward compatibility: old binaries would not read the new catalogue.

### 2. Artist-specific relationships or views over a shared release identity

One hidden/shared release identity plus per-Artist presentation records.

- Duplicate identity: one release, many views; identity remains clear.
- Track membership/order: belongs to the shared release.
- Credits: presentation records can carry artist-specific titles/covers.
- Deletion: removing a view is safe; removing the shared release is a separate
  operation.
- Artwork: risks divergent covers unless views are explicitly overrides.
- Editing: needs a distinction between shared facts and per-Artist overrides.
- Import matching: must resolve both the release and the desired view.
- Migration: large. A new release entity and a new relationship layer are
  required; every existing Album must be mapped, and GUI/TUI navigation must
  learn a second identity level.
- Backward compatibility: requires a schema version bump and an explicit
  downgrade story.

This is the most flexible model, but it adds a second identity level before the
product has validated that users need artist-specific Album presentation. It
also increases the risk of conflicting metadata and artwork, which the milestone
explicitly forbids.

### 3. Separate artist-owned Albums referencing shared Tracks and managed audio

Each participating Artist owns an Album entity; both Albums reference the same
TrackIds and audio resources.

- Duplicate identity: weak. The same release appears twice and can drift in
  title, order and cover.
- Track membership/order: two orders must be kept consistent.
- Credits: available on Tracks, but Album-level participation is duplicated.
- Deletion: must reference-count Tracks and audio across Album copies; deleting
  one Album must not remove a shared Track file.
- Artwork: two covers for one release unless synchronized.
- Editing: conflicting copies of metadata and artwork.
- Import matching: ambiguous — which Album copy receives a new Track?
- Migration: moderate but semantics are unsafe; every Album needs a shared
  resource identity and cross-Album synchronization.
- Backward compatibility: readable in principle, but the product would need new
  invariants to prevent divergence.

This model was rejected. It duplicates identity and creates exactly the
conflicting metadata/artwork and unsafe deletion behavior the milestone
prohibits.

## Decision

Keep the existing **Album physical owner + ordered Track credits** model for
M21.

Reasons:

1. It already preserves stable identity, one Album cover, one track order and
   one managed audio resource per Track.
2. Collaborative releases remain reachable from every credited Artist through
   `albums_credited_for`, without duplicating the Album or its files.
3. No irreversible migration is needed, and existing libraries remain
   bit-compatible.
4. The product problems reported by the user are selection, duplicate
   presentation and artwork repair. Those are solved without changing storage
   semantics.

The limitation is explicit: Album-level participants are derived from Track
credits and the owner. A release with an artist who appears on no Track, or an
Album-level credit display independent of Track credits, is not representable.
That limitation is acceptable until user validation shows it matters.

### Deferred proposal: explicit multi-artist Album

If validation requires Album-level participation, use alternative 1, not 3:
one Album entity plus an ordered `artist_ids` set (owner included) and a
schema version bump with forward migration. Required work before applying it:

- new catalogue/entity fields with serde defaults for old libraries;
- validation that the owner is a member and remains the physical path owner;
- Album transfer that can change owner without dropping other participants;
- Artist deletion planner that refuses to delete an Album still owned by the
  Artist until ownership is transferred;
- relationship queries that merge owned and participating Albums;
- GUI/TUI participant editors and pickers;
- downgrade behavior: refuse to write a newer schema, or export one owner and a
  lossy participant list;
- migration fixtures for every prior schema version.

No part of this proposal has been implemented.

## Duplicate Artist names

Two Artists may share a display name. They are never merged by name.

- Identity is `ArtistId`; renaming does not change it.
- Import resolution reuses an Artist only by stable ID or by a unique exact
  normalized name in the relevant scope. Multiple matches stay unresolved and
  require explicit selection.
- GUI pickers disambiguate same-named Artists with their stable ID; Album
  pickers disambiguate identical titles within the same owner. Unique names stay
  clean.
- Duplicate resolution is an explicit future merge operation, not an implicit
  side effect. No merge operation exists in M21.

## Singles and release type

Release type remains optional in practice:

- Import review does not require classifying a single. A new Album defaults to
  `album`; `single`, `ep`, `compilation` and `other` are editable metadata, not
  a gate.
- A Track may be standalone (`album_id = None`); the importer never invents an
  "Unknown Album".
- No schema change was made to add an "unknown" release type. Defaulting to
  `album` preserves existing wire compatibility. A future optional release type
  would be a separate, documented schema change.

## Artwork ownership and precedence

- Album cover is Album-owned. Artist image is Artist-owned. Track artwork is an
  optional Track-owned override.
- Display precedence is Track override → Album cover → generated fallback.
- Import review may attach Album artwork, Track artwork and lyrics. These are
  suggestions and remain visible before commit.
- Embedded pictures are now extracted read-only as an additional suggestion:
  PNG/JPEG/WebP only, at most 16 MiB, at most 4096 px per side and 16 MP,
  signature-checked, dimension-checked, staged in a bounded temporary
  directory owned by the import plan, and skipped entirely for audio
  containers above 512 MiB so parsing stays bounded.
- Embedded pictures never supply canonical title, artist, album or lyrics
  metadata. They are never written back to audio files. A sidecar or manual
  artwork choice always wins over an embedded picture.
- Existing duplicates retain artwork repair suggestions. They are applied only
  when the user explicitly chooses **Update details**; **Skip** leaves existing
  artwork untouched.

## Deletion and resource safety

- Audio files are managed resources referenced by Track entities. Deleting an
  Artist or Album must not remove audio still referenced by a live Track.
- Album deletion is refused while populated; Tracks must be moved or removed
  first. Artist deletion uses an explicit per-Track plan with transfer or
  delete decisions, and shared/credited Tracks are never deleted merely because
  one credit is removed.
- Album transfer stages the album file, cover, every Track record and its
  managed assets in one transaction.
- Cleanup refuses to remove any path still referenced by the active catalogue.
  Uncommitted files are restored/removed by the journal rules.

## Import matching and duplicate behavior

- Exact existing path, identical content, similar metadata, same title with
  different audio, probable audio and ambiguous audio are distinct states.
- An existing Track is never overwritten silently. **Skip** preserves the
  existing entity, metadata and artwork. **Update details** is an explicit
  metadata/artwork repair path and keeps the stable TrackId. **Import anyway**
  is an explicit separate-Track choice.
- The review surface shows the existing Track, its Album/owner, artwork
  readiness and **Open album** / **Open artist** actions.
- Album selection is based on the chosen Artist and existing Album entities; the
  user can pick an existing Album without retyping its title.

## Compatibility

No catalogue, entity or settings schema version changed. Existing libraries
continue to load and publish unchanged. The only new persisted artifacts are
temporary extracted images, which live outside the library and are removed when
the import plan drops. Embedded artwork that the user accepts is copied through
the existing managed artwork publication path.

## Ordered roadmap

1. **M21 (implemented):** bounded embedded-artwork suggestion, duplicate
   artwork repair, existing-Album picker, duplicate open actions, same-name
   disambiguation, hover isolation, GUI/TUI composition work.
2. **Next:** explicit hardening tests for shared-resource deletion and
   collaboration navigation across both front ends.
3. **Only if validated:** explicit multi-artist Album schema with the migration
   and rollback plan above.
