# M24 interaction hardening and accessibility notes

Status: implemented and automated-tested. Human usability and accessibility
audits remain pending.

## Scope

M24 clarifies the high-risk workflows around import, duplicate resolution,
collaborative Album participation, the queue, Track actions, and keyboard
operation. It does not change the canonical schema, audio engine, persistence
format, or ownership model.

## Import audit and behavior

The import pipeline remains split into discovery, review, copy preparation, and
atomic catalogue publication. The review now uses precise duplicate labels
instead of a generic "duplicate" state:

| State | Review label | Safe resolution |
|---|---|---|
| No conflict | Ready | Import |
| Exact managed source path | Same managed file | Skip by default; Update details for an explicit metadata/artwork repair |
| Different path, same audio, unique match | Same managed audio | Skip by default; Update details explicitly |
| Same content elsewhere in this batch | Duplicate in batch | Skip or make a deliberate separate import |
| Metadata matches an existing Track | Metadata-only match | Review/import separately |
| Same title, different audio | Title/audio conflict | Review/import separately |
| Size/duration/title agree but fingerprint differs | Probable duplicate | Review/import separately |
| Several possible existing matches | Ambiguous match | Review/import separately |

An exact managed-file duplicate defaults to **Skip**. Re-reviewing it and
confirming the plan does not create another Track. Metadata or artwork repair
requires the explicit **Update details** choice. No discovered artwork replaces
an existing managed cover without that choice.

Import completion now distinguishes:

- complete success;
- partial success with failed candidates;
- no committed Tracks and failures requiring attention.

Failed items show the reviewed Track title and the per-file reason. The result
states that successfully published Tracks are already visible and failed files
were not partially catalogue-registered. **Choose again** restarts source
selection; it does not silently retry with stale preparation output.

Unreadable discovery paths remain separate from candidate-level publication
failures. Cancellation still removes prepared copies and never publishes a
partial catalogue. The existing atomic Controller tests remain authoritative.

## Duplicate identity

Identity remains evidence-based:

- stable TrackId and exact managed path first;
- content fingerprint/size second;
- title/artist/album similarity is advisory only;
- no title/name-only automatic merge;
- no invented fuzzy identity.

The GUI and TUI now describe the same distinctions in their review surfaces.

## Collaborative Album workflow

- Album participation is edited separately from Track credits.
- GUI participant removal is a two-step action: **Remove** changes to
  **Confirm remove** and explains that only the association changes.
- TUI participant removal retains its existing explicit confirmation.
- Removing the last participant remains valid; the Album is not deleted.
- Participant-only Artist deletion and physical-owner deletion protections are
  unchanged from M22.
- Same-name Artists are disambiguated by stable ID in GUI pickers/rows and in
  TUI Artist rows, detail titles, owner labels and participant pickers.

## Queue and overlays

- The queue overlay now uses the same frozen-base modal composition as other
  overlays, preventing pointer/hover leakage into covered rows.
- Queue rows use stable QueueEntryIds: clicking the row plays, the labelled drag
  handle reorders, and the labelled remove button removes.
- The queue row exposes tooltips for drag and remove actions.
- Empty queue copy now points to the Track action menu rather than a stale "+"
  instruction.
- Queue close/Escape restores keyboard focus to Search where available.
- Track actions are a modal action sheet rather than a narrow pick list. This
  fixes clipped labels and menus extending past the right edge, gives the menu a
  title/Track context, and makes Escape/action closure predictable.
- The action sheet freezes the covered view and exposes only working actions.

## Keyboard behavior

Added GUI keyboard operation in normal library contexts:

- `Tab` / `Shift+Tab`: next/previous primary destination.
- `↑` / `↓`: move keyboard focus through the visible Track rows.
- `Enter`: play the focused Track.
- `E`: edit the focused/current Track.
- `Y`: open the focused/current Track's lyrics.
- `A`: add the focused/current Track to the queue.
- `F`: toggle favorite for the focused/current Track.
- `I`: open Import.
- `/`: focus Search.
- `Q`: toggle the queue.
- `Esc`: dismiss overlays, editors and transient detail state.

Keyboard focus is visually distinct: the playing Track uses `▶` and accent,
while keyboard focus uses `›` until played.

Global shortcuts are disabled while Setup, Track editor, Lyrics editor, playlist
creation, lyrics import, import manager, Track actions, or other modal state owns
input. Text-entry focus prevents character shortcuts such as typographic "n" or
"e" from triggering playback/edit actions; this was runtime-checked by typing
"next" into Search without opening the editor or changing playback.

## TUI consistency

- Import review now uses precise duplicate qualifiers and documents `u` for
  explicit update of a unique exact-audio duplicate.
- Duplicate rows continue to default to Skip.
- Artist labels use stable IDs when names collide.
- Queue view exposes play (`Enter`), remove (`d`/Delete), reorder
  (`Ctrl+↑/↓`) and cancellation semantics unchanged.
- Compact import help is used below 80 columns.
- Required 120×36, 92×24, 80×24 and 40×12 rendering tests remain in place.

## Accessibility limits

Implemented:

- visible focused/current Track distinction;
- keyboard navigation and shortcuts for primary browsing/playback actions;
- labels/tooltips for icon-only Track menu, selection, favorite, queue drag and
  queue remove controls;
- no color-only state distinction;
- modal pointer isolation and focus restoration to Search on queue/menu close;
- dark-theme contrast consistent with `docs/design-system.md`.

Not implemented or not validated:

- No screen-reader / assistive-technology audit.
- Iced button widgets are not the focusable target used by the app's focus
  operations; keyboard operation is provided by app-level shortcuts rather than
  a complete focus ring across every button.
- No full tab-order audit of every form field.
- No reduced-motion OS integration audit.
- Tooltips are not a substitute for screen-reader names.
- Windows/macOS keyboard behavior was compiled and CI-tested but not manually
  audited.

## Import capabilities deliberately not added

- No online metadata or artwork lookup.
- No audio tag writing.
- No automatic Artist/Album merging.
- TUI review still does not expose Album participant selection; participants
  are managed from Album detail after import. The GUI remains the full
  participant-review surface.

## Validation status

Automated tests cover the state transitions described above. GUI runtime was
inspected for the queue overlay, Track action sheet, participant confirmation,
import chooser/browser and responsive shell. The GUI import review was not
driven end-to-end in this milestone; its behavior is covered by deterministic
Controller/GUI tests. TUI import duplicate review and queue view were rendered
and inspected. None of this is a claim of human usability validation.
