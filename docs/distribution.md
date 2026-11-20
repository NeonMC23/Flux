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
validated in M12/M13.

## CI execution

The complete workflow was executed by `workflow_dispatch` at commit
`8a1d525` (GitHub Actions run `44749832007`). All jobs passed:

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

Earlier M12/M13 dispatches exposed and fixed several native defects before this
green run. The final artifacts below are from the green run.

## Artifact evidence

All sizes are bytes. All SHA-256 values were independently verified locally
after downloading the private CI artifacts.

### Linux x86_64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-linux-x86_64.tar.gz` | 13,553,515 | `b8658af1c1c099c1752f63a3f6256f8057090c6065339076a0925b6f599b8549` |
| `Flux-0.1.0-linux-x86_64.deb` | 10,310,662 | `d159670f7a5ecc77a79719d0e41706bfaf739e34c8ffdfac7213a879dcaff17e` |
| `Flux-0.1.0-linux-x86_64.rpm` | 13,417,897 | `87673255554c5f5da3287c397d21b32771ef3142cf9a36e5ed0f8d583edd478d` |

### Linux ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-linux-aarch64.tar.gz` | 13,625,693 | `cfeba176c2baa3e1d19cd72a63e4c8f261bb642d329fd950f7ff26729fa31a4a` |
| `Flux-0.1.0-linux-aarch64.deb` | 10,072,002 | `b3efd84bb73568713250521f643059201ee98678aa36f7a103889e6bed62637a` |
| `Flux-0.1.0-linux-aarch64.rpm` | 13,606,773 | `7cee1f97e8a3876eaab51968de586722667db6004a2d62002cf676f8d9064ab9` |

### Windows x86_64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-windows-x86_64.zip` | 10,272,051 | `1967859075256811f23f50a1251910827d36d412cb0511cd20d3e53d066f1491` |
| `Flux-0.1.0-windows-x86_64-setup.exe` | 8,639,734 | `2f240aa50f4fad971d239bbdef6b6b25c013586a1c70f13cf8ccc67413596409` |

### Windows ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-windows-arm64.zip` | 9,728,225 | `2e1f2d0c217aaf24a3663054359be773a1f7ef66159adcf86b455eef980c8bdd` |

### macOS ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-macos-arm64.app.zip` | 9,753,180 | `d4cfe6bdf2d780c02237f11e52763945ee13e2abcb6df74caec53f304c3faf0a` |
| `Flux-0.1.0-macos-arm64.dmg` | 10,786,982 | `9e6febd5cbccf6f5d7f1139ddfbb3159a79ca8e082ade6b311bea21c1efed706` |

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
payloads and upgrade extraction. M13 additionally ran real package-manager
lifecycles in disposable containers; see below. The host's own package
database is still not modified by any Flux script.

### Linux package-manager lifecycle (M13)

`packaging/linux/validate-package-managers.sh` uses rootless podman with
Ubuntu 24.04 and Fedora 44 containers. It:

- installs the `.deb` with `apt`/`dpkg` and the `.rpm` with `dnf`;
- checks executables, desktop entry, AppStream metadata and icons;
- creates sentinel library/config files under the container's `$HOME`;
- upgrades a synthetic `0.0.9` `.deb` (same payload, older control version) to
  `0.1.0`, then reinstalls the current package;
- removes the package and verifies package-owned executables and desktop files
  are gone while library/config sentinels remain;
- checks that no user library was placed under `/usr/share`.

The Debian and RPM lifecycle checks both passed locally. The script is opt-in
because it requires `podman`; it fails clearly with `FLUX_REQUIRE_PODMAN=1` if
the prerequisite is missing. ARM64 lifecycle installation was not attempted
because an x86_64 host would emulate the package environment; ARM64 remains
artifact-inspected and native-runtime-tested only under the CI Xvfb job.

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

## Audio diagnostics and local hardware validation

Both front-ends expose:

```sh
flux --audio-diagnostics
flux-tui --audio-diagnostics
```

The report enumerates the backend devices with stable IDs, default/openable
flags, then attempts to open and start the system-default output stream and
reports its sample rate and channel count. It does not write settings, scan the
library, or claim audible output. A build without the `cpal` feature reports
that no backend exists.

M13's local Fedora/Nobara validation reached these distinct levels:

| Level | Evidence |
|---|---|
| Enumeration | 37 CPAL/ALSA devices reported, including the PipeWire default |
| Stream initialization | Default sink opened at 48,000 Hz, 2 channels |
| Decoded playback | Opt-in hardware test loaded a WAV, played for ≥1 second, and observed callbacks/delivered/played frames with no decoder/output errors |
| Signal at the system sink | PulseAudio monitor capture measured a ~0.98 s tone at ~439 Hz; max sample amplitude was low because the test used 5% volume |
| Audible confirmation | Not performed; no human listening or physical-route check is claimed |

Windows and macOS CI runners exposed no output device, so their audio output
remains unvalidated. The native tests still exercise decoding, queue/transition,
recovery, and shutdown behavior through the deterministic null backend.

## Native dialog coverage

Linux dialog command boundaries are covered by a process-level integration test
using fake `zenity`/`kdialog` executables: multi-select Unicode paths,
cancellation, process failure, missing zenity fallback to kdialog, missing
backends, and malformed single-selection output. A single-selection dialog that
returns multiple lines is rejected rather than converted into a path containing
a newline.

The Windows PowerShell and macOS AppleScript dialog code is compiled and
reviewed but was not interactively exercised; GUI-subsystem CLI probes,
Unicode paths, bundle identity and application-data resolution were tested
natively. Explorer/taskbar interaction and native dialog usability remain
unverified.

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
FLUX_DIST_DIR=/path/to/artifacts packaging/linux/validate-package-managers.sh

# Local audio diagnostics and opt-in hardware smoke tests
cargo run --release -p flux-gui --features cpal -- --audio-diagnostics
cargo run --release -p flux-tui --features cpal -- --audio-diagnostics
cargo test -p flux-audio --features cpal --test cpal_hardware -- --ignored

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

## Signing readiness

No credential is present in this environment; the prepared paths remain
optional and fail closed when signing is explicitly required.

**Windows Authenticode**

- `FLUX_SIGN_CERT_SHA1` selects a certificate from the Windows certificate
  store (preferred; no password on the command line).
- `FLUX_SIGN_CERT_PATH` + optional `FLUX_SIGN_CERT_PASSWORD` select a PFX file.
- `FLUX_SIGN_TIMESTAMP_URL` overrides the RFC 3161 timestamp service.
- `FLUX_REQUIRE_SIGNING=1` makes missing or invalid signatures fatal.
- Signing runs on the staged executables before ZIP/installer assembly and on
  the installer before checksum generation. `signtool verify` and
  `Get-AuthenticodeSignature` must both report success; the verifier rejects an
  invalid signature.

**macOS Developer ID / notarization**

- `CODESIGN_IDENTITY` selects the signing identity; without it the bundle gets
  an explicit ad-hoc signature.
- `NOTARY_PROFILE` requires `CODESIGN_IDENTITY`; the script submits through
  `notarytool`, staples the ticket, validates the staple and runs Gatekeeper
  assessment. Signing happens before ZIP/DMG and checksum generation.
- If an explicitly signed bundle fails Gatekeeper, the verifier fails rather
  than silently downgrading the result.

No secret is printed by either flow. Unsigned Windows and ad-hoc macOS
artifacts remain clearly labelled; SmartScreen and Gatekeeper warnings are not
suppressed.

## Budgets, signing and publication

- No release, tag, GitHub release, installer upload or binary publication is
  performed by scripts or CI.
- CI packaging runs only on `workflow_dispatch` and uploads private workflow
  artifacts with a 14-day retention period.
- Signing credentials are never stored in the repository. M13 Windows artifacts
  are unsigned; M13 macOS artifacts are ad-hoc signed and not notarized.
- The six standard quality gates remain mandatory; packaging is additional
  validation, not a replacement.

## Known distribution limits

- Native Windows and macOS audio output was not available on the CI runners;
  Linux hardware reached stream initialization and decoded playback, but not
  human listening.
- Windows artifacts are unsigned; macOS is ad-hoc signed and not notarized.
- Native Windows/macOS dialogs, Explorer integration, terminal behavior and
  case-sensitive volumes remain unverified; Linux dialog command boundaries are
  covered by fake-backend integration tests.
- macOS x86_64 and universal builds are compile/script options, not runtime
  evidence.
- `.deb`/`.rpm` lifecycle was validated in disposable containers, including a
  synthetic older `.deb` upgrade; a privileged host install and an older RPM
  upgrade were not performed.
- ARM64 package-manager installation was not attempted under emulation.
- Bit-for-bit reproducibility is claimed only for the previously compared Linux
  builds with a fixed epoch.
- The final canonical import commit remains synchronous as documented above.
- No automatic update mechanism, telemetry or network service exists.
