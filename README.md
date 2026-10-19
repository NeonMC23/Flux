# Flux

Flux is a planned native, cross-platform desktop music player written in
Rust. It is the successor to the Flow web prototype, keeping Flow's product
ideas and dark visual identity while replacing the implementation with a
native Rust architecture: an Iced graphical application and a dedicated
terminal application sharing the same core logic.

**Status: Milestone M0 — project foundations only.** This repository
currently contains the Cargo workspace skeleton, crate boundaries,
licensing policy, and continuous-integration configuration. It does **not**
play music yet.

## What exists at M0

| Crate | Kind | Responsibility |
|---|---|---|
| `flux-core` | library | Shared domain vocabulary and application-level abstractions. No UI, filesystem, database, audio-device, or network implementation. |
| `flux-library` | library | Future home of local library scanning, metadata, and indexing. Documentation-only placeholder. |
| `flux-audio` | library | Future home of the playback engine boundary. Documentation-only placeholder. |
| `flux-providers` | library | Future home of integrations with legally supported streaming providers. Documentation-only placeholder. |
| `flux-platform` | library | Future home of OS integration (paths, credentials, media controls). Documentation-only placeholder. |
| `flux-gui` | binary `flux` | Native [Iced](https://iced.rs) application. M0 shows a static startup view only. |
| `flux-tui` | binary `flux-tui` | Terminal application. M0 prints a startup message and exits. |

Dependency direction at M0: `flux-gui → flux-core`, `flux-tui → flux-core`,
and `flux-gui → iced`. The future boundary crates have no dependencies and
no public API yet; they will be wired in when their milestones implement
them.

## What does not exist yet

Music-library scanning, metadata reading, playback, playlists, streaming
providers, accounts, lyrics, installers, and updaters are all future
milestones. Nothing in this repository makes network requests, collects
telemetry, or requires an account.

## Building

The Rust toolchain is pinned in `rust-toolchain.toml` (currently Rust
1.99.1, with `rustfmt` and `clippy`). With [rustup](https://rustup.rs)
installed, the correct toolchain is installed automatically on first use.
The workspace declares `rust-version = 1.88`, the minimum required by Iced
0.14; the pinned toolchain is what CI and contributors are expected to use.

On Linux, the GUI also needs system development libraries for fontconfig,
libxkbcommon, and Wayland, plus `pkg-config`. On Debian/Ubuntu:

```sh
sudo apt-get install pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev
```

Common commands:

```sh
cargo build --workspace          # build everything
cargo test --workspace           # run tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo deny check licenses        # dependency license policy (see deny.toml)
cargo run -p flux-gui            # GUI startup view (binary name: flux)
cargo run -p flux-tui            # TUI startup message (binary name: flux-tui)
```

## Supported targets and CI

Flux targets Linux, Windows 10/11, and macOS on x86_64 and ARM64. The CI
workflow in `.github/workflows/ci.yml` runs formatting, Clippy, dependency
license checks, tests, and builds. Tests and builds run natively on Linux
x86_64 and ARM64, Windows x86_64 and ARM64, and macOS ARM64. macOS x86_64
has no standard GitHub-hosted runner, so CI performs a compile-only
cross-check (`cargo check --target x86_64-apple-darwin`) from an ARM64
macOS runner; it is not executed or tested there.

## License

Flux's own code is licensed under the MIT License (see `LICENSE`).

Third-party dependency licenses are governed by `deny.toml`, which allows
only permissive licenses (MIT, Apache-2.0, Zlib, BSD, ISC, BSL-1.0,
CC0-1.0, Unlicense, Unicode-3.0) and denies copyleft obligations. Iced
itself is MIT-licensed. Run `cargo deny check licenses` to reproduce the
check against `Cargo.lock`.

No assets from the Flow prototype (recordings, artwork, photographs, or
fonts) are included in this repository, and the Flow repository itself is
unchanged.
