# Flux distribution guide

This document records the M12 release-candidate evidence. Flux is prepared for
distribution, but **no release, tag, or public upload is created by this
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

## Supported targets

M12 executed the native CI matrix and the manual packaging jobs. A green
cross-compile is not runtime validation; the columns below distinguish what was
actually built, packaged, inspected and executed.

| Platform | Built natively | Packaged | Artifact verified | CI tests/build | Installed | GUI runtime | Audio tested |
|---|---|---|---|---|---|---|---|
| Linux x86_64 | Yes (Ubuntu 24.04) | `.tar.gz`, `.deb`, `.rpm` | Yes | Yes | Extracted/upgrade simulated, not privileged install | Yes, under Xvfb | No device on runner |
| Linux ARM64 | Yes (native ARM64 runner) | `.tar.gz`, `.deb`, `.rpm` | Yes | Yes | Extracted/upgrade simulated, not privileged install | Yes, under Xvfb | No device on runner |
| Windows x86_64 | Yes (windows-2025) | Portable ZIP, Inno Setup | Yes | Yes | Yes, temp-dir install/reinstall/uninstall | Yes, first-run and reopen | No device on runner |
| Windows ARM64 | Yes (windows-11-arm) | Portable ZIP | Yes | Yes | Portable extraction only (no installer) | Yes, first-run and reopen | No device on runner |
| macOS ARM64 | Yes (macos-15) | `.app`, ZIP, DMG | Yes | Yes | Bundle extracted from ZIP and DMG | Yes, first-run and reopen | No device on runner |
| macOS x86_64 | Compile-only cross-check | Not built | Not applicable | Compile only | No | No | No |

macOS universal binaries remain a supported script option but were not built or
validated in M12.

## CI execution

The complete workflow was executed by `workflow_dispatch` at commit
`48419dc` (GitHub Actions run `44706634558`). All jobs passed:

- format/Clippy, dependency licences, Linux x86_64 and ARM64 tests/builds,
  Windows x86_64 and ARM64 tests/builds, macOS ARM64 tests/build and macOS
  x86_64 compile-only cross-check;
- Linux x86_64/ARM64 packaging and package validation;
- Windows x86_64/ARM64 packaging and artifact verification;
- macOS ARM64 packaging and artifact verification.

The workflow has `contents: read` permission, does not create tags or releases,
does not sign with an unavailable identity, and uploads only private workflow
artifacts with 14-day retention. Packaging jobs run only on
`workflow_dispatch`; ordinary pushes run tests and build checks.

Earlier M12 dispatches exposed and fixed several native defects before this
green run. The final artifacts below are from the green run.

## Artifact evidence

All sizes are bytes. All SHA-256 values were independently verified locally
after downloading the private CI artifacts.

### Linux x86_64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-linux-x86_64.tar.gz` | 13,538,559 | `856c6c76088f3c48e3dc2be56b2b6beba557c8a39cc295863f22356019c672a0` |
| `Flux-0.1.0-linux-x86_64.deb` | 10,294,888 | `6a1ca68d29ceec447bfbda5b1ed349736b5805862e6261bb4c55d6454df406d5` |
| `Flux-0.1.0-linux-x86_64.rpm` | 13,401,465 | `9f7a9f4bec6e49584e8f1586a77563c187972ada0f54c24820d8c5c2be86d7e4` |

### Linux ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-linux-aarch64.tar.gz` | 13,614,084 | `860fda90fe2d7123d85a7240d140cb3b9e877149803f344ce4cb3b10272368f6` |
| `Flux-0.1.0-linux-aarch64.deb` | 10,098,860 | `ec1aa2efec3cd36eec89699e170642ec4cfff0952c10f2fec5b56f50d84b9521` |
| `Flux-0.1.0-linux-aarch64.rpm` | 13,591,211 | `f6e2ce644d0fdaef84cea0227b6c1eb18a942a5bb8b81aff6c523a15e36b72a5` |

### Windows x86_64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-windows-x86_64.zip` | 10,258,111 | `f9334045f52a9d0295ad579ee5c576a7bcae25f46407667ab16e4c62cdde6ff0` |
| `Flux-0.1.0-windows-x86_64-setup.exe` | 8,629,402 | `ba0cc6a546bfbdb7d258df0ec17d488f9b9548e050417792391ac0aace05237b` |

### Windows ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-windows-arm64.zip` | 9,712,733 | `29b011130380dd731bd62455135d44fdcc5880fdf036c290fbec5b482455d70c` |

### macOS ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-macos-arm64.app.zip` | 9,739,185 | `a1ab410672cf4a41f1e50f74079318f1a31f8b095bc564746bc1b9b58aa00d48` |
| `Flux-0.1.0-macos-arm64.dmg` | 10,781,313 | `99bb71ff609445020e2bf870c707abc355f4bb444595c0bf9f73ee9ef041e1f2` |

Each platform's `SHA256SUMS-*.txt` file was verified with `sha256sum -c` against
the downloaded bytes. No artifact contains a user library, queue, ratings,
history, settings or other user data.

## Linux formats and compatibility

Implemented and validated:

- **Portable `.tar.gz`** — `bin/flux`, `bin/flux-tui`, desktop entry,
  AppStream metadata, hicolor icons, README, MIT licence, third-party notices
  and license texts.
- **`.deb`** — installs to `/usr/bin` and `/usr/share`, with package metadata,
  desktop entry, icons, AppStream metadata and documentation.
- **`.rpm`** — same payload and ownership rules.

The Ubuntu-built binary's highest referenced glibc symbol is `GLIBC_2.39`;
the `.deb` declares `libc6 (>= 2.39)` plus ALSA, fontconfig, X11, xkbcommon and
Wayland client libraries. The ARM64 artifact is a native AArch64 ELF with the
same floor and declares `Architecture: arm64` / RPM `aarch64`. Local Fedora
artifacts require glibc 2.43 and are not the broad-compatibility baseline.

The CI package verifier ran the packaged GUI under Xvfb, checked `--version`,
first-run canonical-library creation, desktop/AppStream metadata, package
payloads and upgrade extraction. A privileged host installation with system
package managers was **not** performed.

## Windows formats and evidence

- portable ZIP containing `flux.exe`, `flux-tui.exe`, docs and license texts;
- an Inno Setup 6 per-user installer with Start Menu entry, optional desktop
  shortcut and stable AppId `{96707750-32C7-4982-8664-11AE5B7B5C1B}`.

Native verification confirmed:

- `flux.exe` is an `IMAGE_SUBSYSTEM_WINDOWS_GUI` executable and
  `flux-tui.exe` is `IMAGE_SUBSYSTEM_WINDOWS_CUI`;
- the GUI carries embedded icon, version resource and manifest; release
  identity is `Flux 0.1.0 (io.github.neonmc23.flux)`;
- `--version` and `--help` work for both executables, including from a
  Unicode extraction path;
- the release GUI creates a first-run canonical library under a redirected
  `%LOCALAPPDATA%`, stays running, and reopens the same library;
- the installer installs, reinstalls over the same location, preserves
  sentinel data/config files, and uninstall leaves user data/config intact
  while removing the application executable.

The Windows artifacts are unsigned. SmartScreen warnings are expected until an
authorized signing identity is supplied. Native file dialogs, Explorer
integration, audio output and case-sensitive volumes were not exercised.

## macOS formats and evidence

- `Flux.app` with `Contents/MacOS/flux`, `Contents/MacOS/flux-tui`,
  `Info.plist`, `flux.icns`, docs, notices and license texts;
- ZIP created with `ditto`;
- DMG created with `hdiutil`;
- optional universal binary via `FLUX_UNIVERSAL=1` and `lipo`.

Native verification confirmed the bundle identifier, version, icon, ARM64
Mach-O slices, code-signature integrity, ZIP expansion and DMG mount. The
packaged GUI was launched against a disposable Unicode home directory; it
created the first-run canonical library and reopened it successfully.

The bundle is **ad-hoc signed only** and not notarized; Gatekeeper assessment
does not pass. No Developer ID or notary credential was available, and none was
fabricated. CoreAudio output was not available on the runner.

## Installation and user data

Packaged applications never store user data in the installation directory.

| Platform | Data | Config |
|---|---|---|
| Linux | `$XDG_DATA_HOME/flux` (default `~/.local/share/flux`) | `$XDG_CONFIG_HOME/flux` |
| macOS | `~/Library/Application Support/flux` | same |
| Windows | `%LOCALAPPDATA%\flux` | `%APPDATA%\flux` |

The default canonical library is `<data>/FluxLibrary`. On a fresh launch Flux
creates that empty library and opens Library setup so the user can import music
or choose another library.

Uninstalling the application must not remove `FluxLibrary`, the legacy index,
queue, ratings, history, lyrics, artwork or settings. The Linux package scripts
contain no user-data deletion entries; the Windows installer has no
`[UninstallDelete]` entries; the macOS application bundle has no user-data
payload.

## Import publication responsiveness

Import planning, fingerprinting and portable copy preparation are cancellable
and run off the UI thread in the GUI. The final canonical publication is still a
synchronous, journal-backed transaction on the applying thread: it copies and
verifies staged resources, publishes them and commits the catalogue atomically.
A large import can therefore pause the GUI during that final commit. M12 kept
this behavior deliberately: moving publication to a detached worker would have
to preserve controller/library-session rebinding, journal ownership, shutdown
ordering and cancellation truthfulness, which is a broader transaction redesign
than this milestone's platform-validation scope. The limitation is therefore
documented rather than hidden; no partial catalogue is ever published.

## Build commands

```sh
# Linux x86_64 (or the native architecture)
packaging/linux/build-packages.sh
packaging/linux/validate-packages.sh

# Windows (PowerShell, native or appropriate runner)
packaging/windows/build-windows.ps1 -Architecture x86_64
packaging/windows/verify-windows.ps1 -Architecture x86_64 -TestInstaller -TestGuiLaunch
packaging/windows/build-windows.ps1 -Architecture arm64 -SkipInstaller
packaging/windows/verify-windows.ps1 -Architecture arm64 -TestGuiLaunch

# macOS (native)
packaging/macos/build-macos.sh
packaging/macos/verify-macos.sh
FLUX_UNIVERSAL=1 packaging/macos/build-macos.sh
```

Artifacts are written to `dist/` and are intentionally ignored by Git. Each
platform writes a `SHA256SUMS-*.txt` file. Release artifacts must be reviewed and
checksum-verified before any authorized publication.

Two consecutive Linux builds with `SOURCE_DATE_EPOCH=0` previously produced
byte-identical `tar.gz`, `.deb` and `.rpm` artifacts. Normal builds use the
current timestamp, so their checksums differ. No universal-reproducibility claim
is made for platforms that were not built twice.

## Budgets, signing and publication

- No release, tag, GitHub release, installer upload or binary publication is
  performed by scripts or CI.
- CI packaging runs only on `workflow_dispatch` and uploads private workflow
  artifacts with a 14-day retention period.
- Signing credentials are never stored in the repository. Windows artifacts are
  unsigned; macOS artifacts are ad-hoc signed and not notarized.
- The six standard quality gates remain mandatory; packaging is additional
  validation, not a replacement.

## Known distribution limits

- Native Windows and macOS audio output was not available on the CI runners.
- Windows artifacts are unsigned; macOS is ad-hoc signed and not notarized.
- Native Windows file dialogs, Explorer integration, terminal behavior and
  case-sensitive volumes remain unverified.
- macOS x86_64 and universal builds are compile/script options, not M12 runtime
  evidence.
- `.deb`/`.rpm` installation was validated by extraction, metadata and packaged
  GUI startup, not by a privileged host installation.
- Bit-for-bit reproducibility is claimed only for the previously compared Linux
  builds with a fixed epoch.
- The final canonical import commit remains synchronous as documented above.
- No automatic update mechanism, telemetry or network service exists.
