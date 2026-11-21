# Library architecture decision: Artists, Albums, Tracks and collaborations

Status: **implemented in M22 as canonical schema version 2.**

This document records the canonical model, the alternatives considered for
collaborative releases, the selected model, and the migration and rollback
behavior now implemented.

## Current model (schema version 2)

The canonical store already uses stable entities and explicit relationships;
Track rows do not own copies of Artist or Album strings.

```text
Artist (stable ArtistId, name, aliases, description, image)
  ↕ ordered participation (many-to-many)
Album (stable AlbumId, physical owner ArtistId, ordered participant ArtistIds,
       title, release type, cover, ordered TrackIds)
  └── Track (stable TrackId, ordered credited ArtistIds,
             physical owner ArtistId, optional AlbumId,
             track/disc numbers, lyrics, optional artwork override)
```

- `CanonicalArtist.id` is the identity. Display names are not unique keys.
  Import resolution only reuses an Artist by an exact stable reference or an
  unambiguous exact normalized name; otherwise it proposes a new Artist.
- `CanonicalAlbum.owner_artist_id` is a physical/organizing role: it selects
  the Album directory and the physical owner required for every member Track.
  It is **not** the same as Album participation.
- `CanonicalAlbum.participant_artist_ids` is the explicit ordered many-to-many
  association. It may contain several Artists, may include or omit the physical
  owner, and may be empty. Track credits never populate it implicitly.
- `CanonicalTrack.credited_artist_ids` is an ordered list. The first credit is
  the default owner at import time; the physical owner may explicitly be a
  later credit. This expresses collaborations and features without duplicating
  audio.
- `Controller::albums_participating_for(artist)` returns explicit Album
  associations; `albums_owned_by` reports physical ownership; and
  `albums_credited_for` reports Track-credit-only reachability. Artist surfaces
  union these views while labelling the relationship, so a participant can
  navigate to a shared release without owning files or appearing on a Track.

This is already an entity model. The M21 investigation did not find a need to
replace it; it found gaps in selection, duplicate presentation and artwork
repair, which are addressed in `flux-app` and the GUI rather than by a
destructive rewrite.

## Alternatives considered for collaborative Albums

### 1. One canonical Album associated with multiple Artists

`Album.participant_artist_ids: Vec<ArtistId>` (or a join table) plus one
physical owner for resources. The owner may be a participant or only the
organizing/physical role.

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

## Decision (implemented)

M22 adopts alternative 1: **one canonical Album entity with an explicit ordered
participant set**, while retaining `owner_artist_id` strictly as the physical
and organizing role.

Semantics:

- Album participation is independent of Track credits in both directions.
- The physical owner must own every member Track and therefore appears in each
  Track's ordered credits, but need not participate in the Album.
- Removing a participant changes only the association. It never deletes the
  Album, Tracks, audio, artwork or Track credits.
- Removing the last participant leaves a valid participant-less Album.
- Album identity, cover, title, release type and track order remain singular.
- Editing an Album's participants is one atomic catalogue transaction.
- No second Album entity or audio copy is created for collaboration.

Artist pages union physical ownership, explicit participation and Track-credit
reachability, and label which relationship applies. Album pages expose every
participant as a navigable Artist entity.

## Schema 2 and migration

`catalog.flux` stores `participant_artist_ids` for every Album as an optional
ordered list. Schema 2 requires the field; an explicitly empty list is valid.
Schema 1 had no such field. On open:

1. Recover any outstanding journal exactly as before.
2. If manifest/catalogue are schema 1, add each Album's physical owner as its
   deterministic initial participant.
3. Validate artists, duplicates, Album/Track relationships and the schema-2
   catalogue before writing.
4. Publish one transaction that upgrades the manifest, catalogue, and every
   version-1 entity file. Raw JSON is edited as `serde_json::Value`, preserving
   unknown fields.
5. Reopen and validate the committed schema-2 library.

The existing catalogue backup and C1 journal provide rollback before the
catalogue commit and roll-forward after it. A failure before the commit leaves
schema 1 intact and the next open retries; a failure after the catalogue commit
is recovered from the journal. Unsupported or malformed versions fail closed;
a schema-2 catalogue with a missing participant list is rejected.

## Lifecycle

- **Deleting a participant Artist** removes only participation associations and
  the existing Track-credit decisions. Shared Albums and audio remain.
- **Deleting a physical owner** requires every owned Album to be either
  participant-free with explicit owned-Album deletion confirmation, or
  ownership-transferred first when other participants exist. Shared Albums are
  never deleted as a side effect.
- **Deleting an Album** still requires it to be empty and explicit; its
  participants are removed with the entity.
- **Track move/reassignment** preserves the Album-owner physical rule and leaves
  Album participation unchanged.
- **Resource safety** is unchanged: audio cleanup only occurs through reviewed
  Track deletions or Album transfer cleanup, after the catalogue commit.

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

Schema 2 remains forward-migratable from schema 1. Older binaries cannot read
schema 2 and therefore must not be pointed at an upgraded library; this is the
explicit downgrade boundary. Unknown fields are preserved during migration, and
entity files are upgraded to schema 2 in the same transaction. Settings, queue
and history schemas are unchanged.

## Ordered roadmap

1. **M21 (implemented):** artwork repair, entity pickers, duplicate navigation,
   hover isolation, GUI/TUI composition.
2. **M22 (implemented):** explicit participants, schema-2 migration, participant
   editing in GUI/TUI, collaboration-aware Artist/Album pages and safe deletion.
3. **Next:** human validation of collaborative workflows and any additional
   release metadata needed by real users.
