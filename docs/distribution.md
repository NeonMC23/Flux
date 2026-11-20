# Flux distribution guide

This document describes the M11 release-engineering outputs. Flux is prepared
for distribution but **no release, tag, or public upload is created by this
repository**.

## Identity

See [`assets/README.md`](../assets/README.md) for the complete identity table.
The stable values are:

- product/executables: `Flux`, `flux`, `flux-tui`;
- application/bundle id: `io.github.neonmc23.flux`;
- version source: workspace `Cargo.toml` (`0.1.0`);
- licence: MIT; copyright `Copyright (c) 2026 NeonMC23`.

`flux --version` and `flux-tui --version` print the same release identity, so
packaged artifacts can be checked without starting a GUI or terminal session.

## Supported build targets

| Platform | Architecture | Build status | Validation status |
|---|---|---|---|
| Linux | x86_64 | Built locally | Packaged, extracted, runtime-tested |
| Linux | ARM64 | CI job defined | Not built locally (x86_64 host) |
| Windows | x86_64 | CI job defined | Not built or run locally |
| Windows | ARM64 | CI job defined | Not built or run locally |
| macOS | ARM64 | CI job defined | Not built or run locally |
| macOS | x86_64 | Script supported | Not built or run locally |
| macOS | universal | Script option | Not built or run locally |

A target is only called validated when the artifact was executed on that native
platform. A green cross-compile or YAML entry is not runtime validation.

## Linux formats

Implemented and locally validated:

- **Portable `.tar.gz`** — `bin/flux`, `bin/flux-tui`, desktop entry,
  AppStream metadata, hicolor icons, README, MIT licence, third-party notices
  and license texts. No user data is embedded.
- **`.deb`** — installs to `/usr/bin` and `/usr/share`, with package metadata,
  desktop entry, icons, AppStream metadata and documentation. Uninstall removes
  only package-owned files.
- **`.rpm`** — same payload and ownership rules, with `alsa-lib`, fontconfig,
  X11, xkbcommon and Wayland client dependencies declared.

AppImage and Flatpak were evaluated and deliberately not implemented in M11:
AppImage tooling is not present in this environment and would add a separate
FUSE/runtime maintenance path; Flatpak requires a portal/sandbox permission
design that has not been validated for Flux's local-library and audio workflow.
The portable archive, `.deb` and `.rpm` cover the supported Linux paths without
claiming an unvalidated sandbox integration.

## Windows formats

Prepared for native Windows packaging:

- portable ZIP containing `flux.exe`, `flux-tui.exe`, docs and license texts;
- an Inno Setup 6 per-user installer with Start Menu entry, optional desktop
  shortcut and stable AppId `{96707750-32C7-4982-8664-11AE5B7B5C1B}`.

`crates/flux-gui/build.rs` embeds the `.ico`, version resource and an as-invoker
DPI-aware/long-path-aware manifest into the MSVC executable when `rc.exe` is
available. The installer does not add user-data deletion entries.

No Windows SDK/runner is available locally. The scripts are source-audited and
the CI packaging jobs are defined but were not executed in this environment.
Artifacts are unsigned unless an authorized signing identity is supplied; expect
SmartScreen warnings and verify SHA-256 checksums.

## macOS formats

Prepared for native macOS packaging:

- `Flux.app` with `Contents/MacOS/flux`, `Contents/MacOS/flux-tui`,
  `Info.plist`, `flux.icns`, docs, notices and license texts;
- ZIP created with `ditto`;
- DMG created with `hdiutil` when available;
- optional universal binary via `FLUX_UNIVERSAL=1` and `lipo`;
- ad-hoc signing by default, or Developer ID signing through
  `CODESIGN_IDENTITY`; notarization only through an explicitly supplied
  `NOTARY_PROFILE`.

No macOS runner or SDK is available locally. The bundle script, plist and icon
are source-audited; the CI job is defined but not executed here. Ad-hoc builds
are not notarized and Gatekeeper will warn. No certificate, credential or
publisher identity is fabricated.

## Installation and user data

Packaged applications never store user data in the installation directory.
Windows and macOS verification scripts validate bundle/installer structure,
`--version`, icons and, where available, install/uninstall lifecycle sentinels.

| Platform | Data | Config |
|---|---|---|
| Linux | `$XDG_DATA_HOME/flux` (default `~/.local/share/flux`) | `$XDG_CONFIG_HOME/flux` |
| macOS | `~/Library/Application Support/flux` | same |
| Windows | `%LOCALAPPDATA%\flux` | `%APPDATA%\flux` |

The default canonical library is `<data>/FluxLibrary`. On a fresh launch Flux
creates that empty library, announces the path, and opens Library setup so the
user can import music or choose another library. Import itself remains an
explicit action.

Uninstalling the application must not remove `FluxLibrary`, the legacy index,
queue, ratings, history, lyrics, artwork or settings. The Linux package scripts
contain no user-data deletion entries; the Windows installer has no
`[UninstallDelete]` entries; the macOS application bundle has no user-data
payload.

## Build commands

```sh
# Linux x86_64 (or the native architecture)
packaging/linux/build-packages.sh
packaging/linux/validate-packages.sh

# Reproducible-timestamp attempt for the portable archive
SOURCE_DATE_EPOCH=0 packaging/linux/build-packages.sh

# Windows (PowerShell, native or appropriate runner)
packaging/windows/build-windows.ps1 -Architecture x86_64
packaging/windows/verify-windows.ps1 -Architecture x86_64 -TestInstaller
packaging/windows/build-windows.ps1 -Architecture arm64 -SkipInstaller
packaging/windows/verify-windows.ps1 -Architecture arm64

# macOS (native)
packaging/macos/build-macos.sh
packaging/macos/verify-macos.sh
FLUX_UNIVERSAL=1 packaging/macos/build-macos.sh
```

Artifacts are written to `dist/` and are intentionally ignored by Git. Each
platform writes a `SHA256SUMS-*.txt` file. Release artifacts must be reviewed and
checksum-verified before any authorized publication.

### Local reproducibility check

Two consecutive Linux builds with `SOURCE_DATE_EPOCH=0` produced byte-identical
`tar.gz`, `.deb` and `.rpm` artifacts. Normal builds use the current timestamp,
so their checksums differ. RPM build time/host are normalized to the supplied
epoch and `reproducible`; no universal-reproducibility claim is made for
platforms that were not built twice here.

## Linux local validation evidence

The M11 Linux x86_64 run produced and inspected all three formats. Validation
covered package metadata, payload listing, archive traversal rejection, desktop
and AppStream validation, `--version` from each extracted payload, packaged GUI
startup with isolated XDG directories, first-run canonical-library creation, and
preservation of the generated user data after deleting the extracted application
tree. The GUI started without stderr output; a native `.deb`/`.rpm` installation
into a privileged host was not performed.

## Linux compatibility boundary

The locally built Linux binaries were produced on Fedora 44 with glibc 2.43, so
those specific artifacts require a glibc 2.43-or-newer distribution. They are
release-engineering validation artifacts, not the recommended compatibility
baseline. The manual CI packaging job runs on Ubuntu 24.04 (glibc 2.39); those
artifacts must be the ones reviewed for broader Linux distribution. No portable
glibc baseline is claimed until that CI artifact is executed on target systems.
The TUI and GUI both depend on the platform ALSA runtime and the usual X11 or
Wayland desktop libraries. The `.deb` metadata derives its `libc6` lower bound
from the linked GUI binary instead of claiming a false broad compatibility floor.

## Budgets, signing and publication

- No release, tag, GitHub release, installer upload or binary publication is
  performed by scripts or CI.
- CI packaging runs only on `workflow_dispatch` and uploads private workflow
  artifacts with a 14-day retention period.
- Signing credentials are never stored in the repository. Unsigned Windows and
  ad-hoc macOS artifacts are labelled as such.
- The six standard quality gates remain mandatory; packaging is additional
  validation, not a replacement.

## Known distribution limits

- Native Windows and macOS runtime validation is unavailable in this
  environment.
- Linux ARM64, Windows ARM64 and macOS x86_64/universal are prepared but not
  locally built.
- `.deb`/`.rpm` installation was validated by extraction and metadata inspection,
  not by a privileged host installation. Uninstall behavior is enforced by
  package ownership rules and absence of user-data entries.
- Bit-for-bit reproducibility is claimed only where two local builds were
  compared. RPM embeds host/build metadata by default; do not claim universal
  reproducibility from a single build.
- No automatic update mechanism, telemetry or network service exists.
