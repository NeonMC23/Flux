# Packaging quick start

Release-engineering inputs and scripts live here. They never publish artifacts.

- `linux/` — portable tar.gz, `.deb`, `.rpm`, validation script.
- `windows/` — portable ZIP and Inno Setup installer scripts/resources.
- `macos/` — `Flux.app`, ZIP/DMG, signing hooks, `Info.plist`.
- `licenses/` — standard third-party license texts.
- `generate-notices.py` — regenerates `THIRD_PARTY_NOTICES.md` from `Cargo.lock`.

Run the platform script on the target platform. Builds use the workspace
`Cargo.toml` version and write to `dist/` (ignored by Git). See
[`docs/distribution.md`](../docs/distribution.md) for the supported matrix,
validation evidence, signing policy and known limitations.
