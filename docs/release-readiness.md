# Flux release-readiness audit

This audit is evidence-backed and does not authorize publication. No tag,
release, store listing, signing credential or public binary was created. The
authorized signing/notarization procedure is in
[`signing-and-notarization.md`](signing-and-notarization.md).

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


## M20.1 actual CI artifact verification

Source revision: `2cb94ab75352ad6a32ae8f439a269e2501c911f6`.

The original push run `44901144010` had the packaging jobs intentionally skipped
because packaging requires `workflow_dispatch`. Private packaging runs were
dispatched on the exact candidate SHAs; the final artifact set was produced from
the docs-complete HEAD:

- Run: `44904831642`
- Event: `workflow_dispatch`
- Result: success
- Packaging jobs: Linux x86_64/ARM64, Windows x86_64/ARM64, macOS ARM64

Artifacts were downloaded outside the repository and their published checksums
verified:

| Artifact | SHA-256 verified |
|---|---|
| `Flux-0.1.0-linux-x86_64.tar.gz` | `cd9e3f6069cdc3e57368256b8526e84487902e9a660a12ad23b5243509e43727` |
| `Flux-0.1.0-linux-x86_64.deb` | `28d0c309bf8a4dd8925a9b582feaf2462bda5932e5e3cacefcb0be7a8ca2255a` |
| `Flux-0.1.0-linux-x86_64.rpm` | `b57f9afe5025847b316decf07a84781578b7a086496bb0668152b8e32d7a4623` |
| `Flux-0.1.0-linux-aarch64.tar.gz` | `4c8b48843c2bc0a2f6ed6f9e3274bf7d6703b114664b359074e202a37e923aa0` |
| `Flux-0.1.0-linux-aarch64.deb` | `5739888d3c84332a958bb8f256f9f0cb6175de7d9d0ed6eea76ceb16ccaf31a9` |
| `Flux-0.1.0-linux-aarch64.rpm` | `cb9725cb8894a3961467e668f7d89da67c0f1492cb37e058882ef7b53e2b557d` |
| `Flux-0.1.0-windows-x86_64.zip` | `c0be0371e1424ab2c23c95aae2009c0ab219aed5e86ba82c0bf5f13f9f5bf9cf` |
| `Flux-0.1.0-windows-x86_64-setup.exe` | `63735c7b84aa00b6882a2f70f47e3bfdddf3bd83ea14e518fc9991fd77afe001` |
| `Flux-0.1.0-windows-arm64.zip` | `8eca67ada8bc3a168c615402f31040b971e91997a2731e25fec38199223c320c` |
| `Flux-0.1.0-macos-arm64.app.zip` | `56c57cd002ae3808bc7c068f4237dd13a406f86c7399eca99edeede4b7e4a191` |
| `Flux-0.1.0-macos-arm64.dmg` | `69f78b76632b1f1c8359483dd6f62436b23fc850bc6f266accefaf484965efab` |

### Linux actual artifacts

- x86_64 and AArch64 `.deb` metadata report version `0.1.0`, correct
  architecture and `libc6 (>= 2.39)`.
- AArch64 and x86_64 binaries report `Flux 0.1.0 (io.github.neonmc23.flux)`.
- The actual x86_64 CI `.deb`/`.rpm` passed the disposable package-manager
  lifecycle in Ubuntu 24.04 and Fedora 44 containers: install, synthetic
  upgrade/reinstall, sentinel preservation, removal and package-database state.
- The portable archive extracted with expected permissions and version identity.
- CI itself ran packaged GUI first-run validation under Xvfb.
- The AArch64 artifacts were inspected for metadata/architecture but not
  executed on this x86_64 host.

### Windows actual artifacts

- x86_64 ZIP/installer and ARM64 ZIP downloaded and checksum-verified.
- PE inspection: x86_64 GUI is PE32+ x86-64 GUI subsystem; TUI is console
  subsystem; ARM64 ZIP contains ARM64 PE executables.
- Embedded version resources report `Flux`, `0.1.0` and `NeonMC23`.
- CI native x86_64 verifier passed portable GUI/TUI identity, Unicode path,
  icon, GUI first-run/reopen, installer install/reinstall/uninstall and
  user-data sentinels.
- Windows artifacts are unsigned; Authenticode status is `NotSigned`.
- ARM64 native CI verifier passed the portable GUI launch path.

### macOS actual artifacts

- ARM64 app ZIP and DMG downloaded; checksums verified.
- Bundle identifier `io.github.neonmc23.flux`, version `0.1.0`, executable
  `flux`, icon `flux.icns`, minimum macOS 11.0.
- Both Mach-O executables are arm64 with PIE.
- CI native verifier passed `--version`, ZIP expansion, DMG mount, and packaged
  GUI first-run/reopen with a non-ASCII home.
- Signature state is ad-hoc only. Gatekeeper assessment does not pass and
  notarization was not performed.

### CI publication safety

- The packaging workflow is manual-dispatch only.
- Repository permission is `contents: read`.
- Only private workflow artifacts are uploaded (14-day retention).
- No workflow creates a tag, GitHub release or public binary.
- Signing secrets are not present or printed.

## Verification status matrix

| Status | Evidence |
|---|---|
| Source validated | All six gates on `1aacbbf` |
| CI build validated | Run `44902493899` success |
| Actual CI artifacts inspected | All five platform artifact sets downloaded; checksums verified |
| Linux package lifecycle validated | Actual x86_64 `.deb`/`.rpm` installed/upgraded/removed in Ubuntu 24.04/Fedora 44 containers |
| Windows native lifecycle validated | x86_64 CI verifier install/reinstall/uninstall and GUI first-run/reopen |
| macOS native startup validated | ARM64 CI verifier first-run/reopen and ZIP/DMG mount |
| Signing verified | No: Windows unsigned, macOS ad-hoc only |
| Notarization verified | No |
| Human usability/acoustic validation | Deferred |

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

The actual private CI artifacts for source `1aacbbf` have been downloaded,
checksum-verified and inspected. Linux x86_64 package-manager lifecycle passed
on disposable Ubuntu 24.04 and Fedora 44 containers. Windows x86_64 native
installer lifecycle and GUI first-run/reopen passed in CI; macOS ARM64 native
first-run/reopen and ZIP/DMG inspection passed in CI.

Verdict: **CI artifacts verified and ready for native validation.** A public
release still requires authorized Windows signing and macOS Developer ID signing
plus notarization, plus the deferred human/acoustic validation. No artifact is
claimed signed or notarized when it is not.
