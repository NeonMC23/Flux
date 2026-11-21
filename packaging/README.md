# Packaging quick start

Release-engineering inputs and scripts live here. They never publish artifacts.

- `linux/` — portable tar.gz, `.deb`, `.rpm`, validation script.
- `windows/` — portable ZIP and Inno Setup installer scripts/resources.
- `macos/` — `Flux.app`, ZIP/DMG, signing hooks, `Info.plist`.
- `licenses/` — standard third-party license texts.
- `generate-notices.py` — regenerates `THIRD_PARTY_NOTICES.md` from `Cargo.lock`.

Run the platform script on the target platform. Builds use the workspace
`Cargo.toml` version and write to `dist/` (ignored by Git). Linux release
packages must be built on the oldest supported distribution baseline
(Ubuntu 24.04, glibc 2.39 in CI). The script refuses a publishable package when
the binary needs a newer glibc; `FLUX_ALLOW_NEWER_GLIBC=1` is for host-local
diagnostics only and must not be used for published artifacts. See
[`docs/distribution.md`](../docs/distribution.md) for the supported matrix,
validation evidence, signing policy and known limitations.
