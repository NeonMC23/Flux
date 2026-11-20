# Flux release checklist

This checklist prepares artifacts. It does **not** authorize publication,
tagging or uploading to a public release; those require separate explicit
authorization.

## 1. Repository and toolchain

- [ ] `git status` is clean and the intended commit range has been reviewed.
- [ ] `rust-toolchain.toml` pin is unchanged unless deliberately updated.
- [ ] No signing credentials, tokens or private keys are present in the tree,
      logs or artifacts.
- [ ] `Cargo.lock` is reviewed and unchanged since the intended dependency set.
- [ ] `THIRD_PARTY_NOTICES.md` matches `Cargo.lock` (`python3 packaging/generate-notices.py`).

## 2. Mandatory gates

```sh
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build -p flux-gui -p flux-tui --locked
cargo build -p flux-gui --features cpal --locked
```

Also run `cargo clippy` and builds with `--features cpal`, and the existing
`cargo deny check licenses` CI job.

## 3. Packaging

Run the packaging script on each intended native platform and then its verifier:

```sh
packaging/linux/build-packages.sh
packaging/linux/validate-packages.sh

packaging/windows/build-windows.ps1 -Architecture x86_64
packaging/windows/verify-windows.ps1 -Architecture x86_64 -TestInstaller

packaging/macos/build-macos.sh
packaging/macos/verify-macos.sh
```

- [ ] Artifact names include product, version, OS and architecture.
- [ ] Every `.deb`, `.rpm`, ZIP, installer, `.app.zip` and DMG has a SHA-256
      checksum and the checksum verifies.
- [ ] `flux --version` and `flux-tui --version` report the same release identity
      from every extracted/installed artifact.
- [ ] Icons, desktop file, AppStream metadata, `Info.plist` and Windows version
      resource match `assets/README.md`.
- [ ] No archive contains an absolute path, `..` traversal, symlink or user data.
- [ ] Unsigned/ad-hoc artifacts are labelled; no certificate or publisher is
      fabricated.

## 4. Lifecycle

Use a disposable VM, container or CI runner, never the user's real library:

- [ ] Fresh install, first launch and default-library creation.
- [ ] Existing library selection and import.
- [ ] Reinstall/upgrade over the same user data (sentinel preserved).
- [ ] Uninstall removes only application files.
- [ ] Reinstall after uninstall opens the previous library/settings.
- [ ] Audio device discovery and playback (where a device exists).
- [ ] Shutdown, no orphaned processes, no writes into the install directory.

## 5. Reproducibility

For Linux, compare two builds with a fixed epoch:

```sh
SOURCE_DATE_EPOCH=0 packaging/linux/build-packages.sh
sha256sum dist/Flux-*.tar.gz dist/Flux-*.deb dist/Flux-*.rpm > /tmp/first
SOURCE_DATE_EPOCH=0 packaging/linux/build-packages.sh
sha256sum dist/Flux-*.tar.gz dist/Flux-*.deb dist/Flux-*.rpm > /tmp/second
diff -u /tmp/first /tmp/second
```

Only claim bit-for-bit reproducibility for the formats actually compared. RPM
build host/time must be normalized as in the provided script.

## 6. Publication boundary

- [ ] Do not create a release or tag.
- [ ] Do not upload artifacts publicly.
- [ ] Attach only the reviewed, checksum-verified artifacts after explicit user
      authorization.
- [ ] Preserve existing tags, branches and user changes.
