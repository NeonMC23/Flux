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
`f672237` (GitHub Actions run `44770380901`). All jobs passed:

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

Earlier M12/M13/M14 dispatches exposed and fixed native defects before this
green run. The final artifacts below are from the M14 green run.

## Artifact evidence

All sizes are bytes. All SHA-256 values were independently verified locally
after downloading the private CI artifacts.

### Linux x86_64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-linux-x86_64.tar.gz` | 13,584,370 | `cb0879f1ac880c87b378522cd8b3d1538410aec6a46075635025c6205246a007` |
| `Flux-0.1.0-linux-x86_64.deb` | 10,324,776 | `0e78f97c1b55637d6a8e840e58650b10da2750d8b63e379b5529855cf25dc81b` |
| `Flux-0.1.0-linux-x86_64.rpm` | 13,445,395 | `0f97041ad60584cab596e001e67643c9398fd96247f377a69dc7476ac7e6ee91` |

### Linux ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-linux-aarch64.tar.gz` | 13,646,886 | `d596d056fc355f5e0e40cfa951e47293a2e030b5441e98c7bb7f5a56df8bfa52` |
| `Flux-0.1.0-linux-aarch64.deb` | 10,152,142 | `dc9e6d77fb45b074a4422a9484bf3574b4afd408a482567492906090a099217a` |
| `Flux-0.1.0-linux-aarch64.rpm` | 13,629,427 | `eb8cf6079d6cbd8a208b3631cc126d8297b2ae636bea57a18508d283ff516afc` |

### Windows x86_64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-windows-x86_64.zip` | 10,303,805 | `743ecbe38fd05ccc310de74d56c1a5f56cb5eae147570ddb472a1a4c50c96a2f` |
| `Flux-0.1.0-windows-x86_64-setup.exe` | 8,658,750 | `ecdb48c3699894428d9d448ecc0b503f1ef1fc567d86f38b2e932b85e9664abc` |

### Windows ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-windows-arm64.zip` | 9,753,233 | `17d140d16318eb20e80e64b3c3d0ce5a41994eb4a7172894639723a188de4a19` |

### macOS ARM64

| Artifact | Size | SHA-256 |
|---|---:|---|
| `Flux-0.1.0-macos-arm64.app.zip` | 9,777,444 | `de0407893b75de0cf3cfcf6aec598111363505da9974c843f199511cf942d8f9` |
| `Flux-0.1.0-macos-arm64.dmg` | 10,819,223 | `51a0bcfc71ab3d47cf2d69f4c9147bba689ce4eb58e0658dd2e7c79003ee38ac` |

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
artifacts require glibc 2.43 and are not the broad-compatibility baseline. The
packaging script now refuses a publishable artifact when the binary floor exceeds
the configured baseline; `FLUX_ALLOW_NEWER_GLIBC=1` is diagnostic-only.

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
documented rather than hidden; no partial catalogue is ever published. M14 adds
a concrete worker/job design and acceptance-test contract in
[`import-publication-design.md`](import-publication-design.md), and fixes the
race where a cancel request arriving after preparation but before publication
was ignored.

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

See [`signing-and-notarization.md`](signing-and-notarization.md) for the exact
operator procedure. No credential is present in this environment; the prepared
paths remain optional and fail closed when signing is explicitly required.

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

## M20.1 actual CI artifact verification

Private packaging run `44904831642` was dispatched on source
`2cb94ab75352ad6a32ae8f439a269e2501c911f6` and completed successfully. All
platform artifacts were downloaded and their published checksums verified.

The actual x86_64 CI `.deb` and `.rpm` were installed, upgraded/reinstalled and
removed in disposable Ubuntu 24.04 and Fedora 44 containers. Both declare the
expected `0.1.0` identity, glibc 2.39 baseline and runtime dependencies; the
library/configuration sentinels survived the lifecycle. The portable archive
extracted with expected permissions and version output. AArch64 packages were
metadata/ELF-inspected but not executed on this x86_64 host.

The actual Windows x86_64 installer passed the native CI install/reinstall/
uninstall and GUI first-run/reopen verifier. Windows ARM64 portable verification
also passed. Windows artifacts remain unsigned.

The actual macOS ARM64 app and DMG passed the native CI first-run/reopen, ZIP
expansion and DMG-mount checks. The bundle is ad-hoc signed only; Gatekeeper does
not accept it and no notarization occurred.

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
