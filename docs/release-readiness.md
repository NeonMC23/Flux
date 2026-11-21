# Flux release-readiness audit

This audit is evidence-backed and does not authorize publication. No tag,
release, store listing, signing credential or public binary was created.

## Release blockers

No source, metadata, license, packaging-script or CI blocker was found in this
development environment.

The following external states remain unresolved and must be treated as blockers
for a **public signed release**, not for development validation:

- Windows binaries are unsigned; SmartScreen warnings are expected.
- macOS artifacts are ad-hoc signed only and not notarized; Gatekeeper
  assessment does not pass.
- No authorized signing identity, Developer ID or notary credentials are present.
- Windows and macOS runtime behavior has CI coverage but no interactive native
  audit in this cycle.
- Acoustic/device quality and human usability are not validated.

## Important pre-release improvements

- Single-track ownership transfer remains GUI-only. Album and Artist transfer
  decisions are available in both frontends.
- TUI import uses typed paths instead of native file pickers and has no
  multi-selection or drag-and-drop.
- The TUI is keyboard-only; no mouse support is claimed.
- `flux-tui/src/main.rs` remains large even after module extraction.
- No screen-reader or full focus-order audit has been performed.
- Windows installer and macOS bundle scripts are prepared but signing and
  notarization require external credentials.

## Deferred manual validation

- First launch, library creation and switching on each native OS.
- Installation, upgrade, rollback and uninstall on real package managers.
- Audible playback, device hotplug, recovery and acoustic quality.
- Every GUI/TUI manual workflow in the deferred checklist.
- Terminal emulator/font coverage beyond the captured Linux PTY sessions.
- Accessibility, screen-reader and high-contrast validation.

## Optional future enhancements

- Configurable TUI keybindings.
- TUI native-independent file browser/multi-selection.
- Further splitting entity actions out of `main.rs`.
- Word-level synchronized lyrics and karaoke parity.

## M20 packaging audit finding

The local development host uses glibc 2.43. A host-built `.deb` therefore
declared `libc6 (>= 2.43)` and could not install on Ubuntu 24.04, the supported
Linux package baseline. This was a genuine host-artifact defect, not a source
defect.

`packaging/linux/build-packages.sh` now refuses to produce publishable packages
when the binary's glibc floor exceeds the configured baseline (default `2.39`).
Host-local diagnostic packages require the explicit
`FLUX_ALLOW_NEWER_GLIBC=1` override and must not be published. Release artifacts
must be built by the Ubuntu 24.04 CI packaging job, whose baseline matches the
supported Debian/Ubuntu target.

M20 runtime evidence on the Linux audit host:

- release TUI started through a PTY at 80×24 and exited cleanly;
- release GUI started under Xephyr with a fresh XDG root, created the first-run
  canonical library, remained responsive until the expected timeout, and wrote
  no stderr;
- no physical audio device or acoustic validation is claimed.

Local host-specific packaging validation passed:

- portable `.tar.gz` extraction and version identity,
- `.deb` and `.rpm` metadata/payload extraction,
- first-run GUI startup under a disposable X session,
- reinstall/upgrade sentinel preservation,
- application removal without deleting user library/config sentinels,
- SHA-256 checksum verification.

The podman package-manager lifecycle was attempted and correctly failed because
the host-built diagnostic `.deb` requires glibc 2.43. It cannot validate the CI
artifact from this host. The native CI packaging job and the deferred Ubuntu
package-manager validation remain the authoritative lifecycle evidence.

## Verified metadata and packaging inputs

- Workspace/app identity `io.github.neonmc23.flux`.
- Workspace version `0.1.0` consumed by Linux, Windows and macOS packaging
  scripts.
- MIT project license and generated third-party notices.
- Desktop file, AppStream metadata, macOS Info.plist template, Windows resource
  identity and canonical icon set.
- Linux `.tar.gz`, `.deb` and `.rpm` workflows; Windows portable/installer
  workflows; macOS app/ZIP/DMG workflows.
- Checksums and signing-state checks are present in the packaging scripts.
- CI covers Linux, Windows and macOS builds/tests, compile-only macOS x86_64,
  Clippy, CPAL compilation, dependency licenses and packaging metadata.

## Current release classification

Flux is ready for final human/platform validation and CI-produced release
candidates. A public signed release still requires external signing/notarization
credentials, native-platform/manual lifecycle validation and acoustic/usability
validation. The host glibc mismatch is guarded and must not be bypassed for a
published package.
