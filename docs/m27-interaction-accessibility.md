# M27 interaction quality, accessibility and parity

Status: implemented and automated-tested; runtime-inspected on Linux. Native
assistive-technology, compositor-icon and Windows/macOS interaction validation
remain pending.

## Keyboard and focus behavior

- Plain **Tab / Shift+Tab** now performs focus traversal through focusable
  controls, including forms, editors and Setup. It no longer changes views.
- **Ctrl+Tab / Shift+Ctrl+Tab** cycles the visible primary destinations.
- Track-focused shortcuts remain: `↑/↓` focus, `Enter` play, `E` edit,
  `M` Track action menu, `Y` lyrics, `A` queue, `F` favorite, `I` import.
- The Track action menu supports `↑/↓` selection, `Enter` activation and
  `Escape` dismissal. The highlighted row is visible.
- The queue overlay supports keyboard operation when open:
  `↑/↓` selects, `Enter` plays, `Delete/Backspace/d` removes, `Escape/Q` closes.
- Form focus is visible through the existing accent-bordered text fields.
- Global view shortcuts are suppressed while editors and modal workflows own
  input.

## Accessibility limits

- Iced 0.14 does not expose an accessibility tree or screen-reader role/name API
  that Flux can drive from the application layer. Tooltips and adjacent labels
  remain the available naming mechanism for icon-only controls.
- Iced buttons are not focusable targets in the widgets used by Flux; complete
  Tab traversal of every button still requires framework support. The focus
  traversal implemented here covers focusable text inputs and supported controls.
- No screen-reader smoke test was performed. Issue #1 remains open for a native
  accessibility audit and any future framework support.

## Forms and destructive actions

- Tab traversal now reaches text fields without changing views.
- Dirty editors still block navigation and require explicit discard/escape
  confirmation.
- Existing two-step destructive confirmations (participants, deletion, clear
  queue, playlist changes) are unchanged.
- Validation errors remain inline and recoverable.

## TUI collaborative Album participants during import

- Press `p` in TUI import review to open the participant list for the selected
  candidate.
- `a` opens the existing-Artist picker; `Enter` adds, `r` removes, `Esc` returns.
- The physical owner is displayed separately and can never be added/removed as a
  participant through this workflow.
- Display-name identity is not used for merging. The owner hint is used only when
  the candidate has a stable ID or exactly one existing Artist has the proposed
  name; ambiguous names remain unresolved for canonical import validation.
- Additional participants are stored only in the reviewed candidate. Cancelling
  or closing the review leaves the catalogue unchanged.
- Regression coverage verifies add, remove, cancellation, import publication,
  owner preservation and Track-credit independence.
- The participant views render at 120×36, 80×24 and 40×12.
- Creating a brand-new Artist from the participant picker is not implemented;
  existing Artists only. This remains a deliberate scope limit, not simulated.

## Reduced motion

- Added a persisted **Reduce motion** preference in Settings → Appearance.
- When enabled, the continuous waveform visualization subscription is stopped
  and the visualizer renders a static frame. Playback, timing, seeking and
  controls are unchanged.
- No decorative transitions exist in M27, so there is no separate transition
  switch. OS-level preference detection is not available through the current
  reliable Iced/platform integration and was not claimed.
- Issue #2 remains open for optional restrained transitions; reduced-motion
  support itself is now implemented at the application level.

## Interaction consistency and layout resilience

- The M26 fixed player geometry, Track artwork/duration presentation and sortable
  headers are preserved.
- Settings scrolls; the appearance controls remain reachable at 900×650 and at
  120% text scale.
- The sidebar gains width at larger text scales to avoid clipped labels.
- Tab focus and queue/Track keyboard operation work alongside existing pointer
  interactions without changing queue or Track identity semantics.

## Desktop identity

- Canonical PNG sizes, ICO/ICNS headers, desktop-entry icon/StartupWMClass fields
  and AppStream identity are verified by automated tests.
- The development installer for `XDG_DATA_HOME` remains the required step for
  source-checkout runs on Wayland.
- Actual compositor/taskbar/Dock rendering was not manually tested; issue #4
  remains open.

## Validation limits

- Linux GUI runtime only.
- TUI startup and responsive rendering only; no native terminal usability audit.
- No human screen-reader, high-contrast, reduced-motion preference or native
  Windows/macOS interaction validation.
