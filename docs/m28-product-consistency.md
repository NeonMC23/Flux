# M28 product consistency and playback reliability

Status: implemented and automated-tested; runtime-inspected on Linux. Physical
audio hardware, native assistive technology and native Windows/macOS interaction
validation remain pending.

## Playback lifecycle audit

The audit covered Controller/engine ownership, explicit and default device
selection, recovery, playback intent, queue identity, transitions and shutdown.
The existing design already had strong safeguards and deterministic tests:

- Explicit device selection is persisted and is never silently replaced with the
  system default when unavailable.
- Startup with an unavailable saved device remains unavailable and exposes
  explicit retry/switch guidance.
- Device loss enters bounded recovery, releases the failed stream immediately,
  and preserves the queue entry and playback intent.
- Recovery can be paused, stopped, retried or superseded without stale callbacks
  restarting old playback.
- After the bounded attempts fail, playback state is stopped, the queue entry is
  retained and the error is reported.
- Device switching installs the new engine before the previous one is stopped
  and restores playing or paused intent at the current position.
- Queue replacement, current-entry removal, repeat/shuffle, seeking and shutdown
  preserve `QueueEntryId` identity.
- Missing, unsupported and corrupted audio paths are distinguished from
  recoverable output failures.

No concrete engine/decoder/output defect was found in this audit, so the real
audio path was not changed merely to claim activity.

## Playback state communication in the TUI

A concrete GUI/TUI inconsistency was found: the TUI summary reported `Playing`,
`Paused` or `Stopped` as if output existed even when the build had no audio
backend or the output was unavailable/recovering. The GUI already reported this.

The TUI now appends the real output state to the now-playing summary:

- `no audio backend`
- `audio unavailable`
- `audio recovering`

Playback intent remains visible, but the interface no longer implies audible
output when there is none.

## Reduced motion parity

The M27 reduced-motion preference was GUI-only in practice; the TUI visualizer
still animated. M28:

- honors the shared reduced-motion preference in TUI visualizer drawing;
- adds a **Reduce motion** setting row to TUI Settings;
- persists the setting through the shared Controller/settings store;
- keeps the static/dynamic visualizer behavior consistent with the GUI.

## Playback regression coverage

Existing lifecycle tests remain authoritative and include:

- explicit saved-device unavailability at startup;
- no device/backend availability;
- fatal output loss during playback;
- pause, stop and queue replacement during recovery;
- bounded recovery failure and explicit retry;
- output switching while playing, paused or stopped;
- stale engine errors and nonfatal route notices;
- queue removal/current-entry replacement;
- long gapless/crossfade resource drift;
- shutdown during pending or blocked recovery.

M28 adds coverage that the TUI summary reports missing output and that the TUI
settings toggle controls and persists the shared reduced-motion preference.

## Consistency and UX

- The TUI now distinguishes playback intent from actual output availability.
- Reduced motion is no longer a hidden GUI-only preference.
- Existing fixed player geometry, artwork precedence, duration hints, sortable
  headers, queue keyboard behavior and canonical entity semantics are preserved.
- No schema, dependency, packaging or migration change was introduced.

## Remaining limits

- Physical device hotplug, Bluetooth loss/recovery, device-clock changes,
  acoustic gaplessness and long-session hardware stability require real audio
  hardware.
- Native screen-reader, compositor/taskbar/Dock icon and Windows/macOS keyboard
  validation remain outstanding.
- Full-layout scale and optional animated transitions remain deferred.
- TUI and GUI visual layouts remain intentionally different.
