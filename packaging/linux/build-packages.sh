#!/bin/bash
# Builds Flux release executables and Linux artifacts: portable tar.gz, .deb
# and .rpm. It never installs into the host and never publishes anything.
#
# Usage:
#   packaging/linux/build-packages.sh
#   FLUX_ARCH=aarch64 packaging/linux/build-packages.sh
#   FLUX_SKIP_BUILD=1 packaging/linux/build-packages.sh
#   SOURCE_DATE_EPOCH=0 packaging/linux/build-packages.sh   # reproducible timestamps
#
# Requirements: bash, cargo, dpkg-deb, rpmbuild, tar, gzip, sha256sum.
# The build must run on the target architecture or with a configured target.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

VERSION="$(awk '
  /^\[workspace.package\]/ { in_section=1; next }
  /^\[/ { in_section=0 }
  in_section && /^version *=/ { gsub(/[",]/, "", $3); print $3; exit }
' Cargo.toml)"
[ -n "$VERSION" ] || { echo "error: could not read workspace version" >&2; exit 1; }

HOST_ARCH="$(uname -m)"
ARCH="${FLUX_ARCH:-$HOST_ARCH}"
case "$ARCH" in
  x86_64|aarch64) ;;
  *) echo "error: unsupported architecture $ARCH (expected x86_64 or aarch64)" >&2; exit 2 ;;
esac
case "$ARCH" in
  x86_64) DEB_ARCH="amd64" ;;
  aarch64) DEB_ARCH="arm64" ;;
esac
if [ "$ARCH" != "$HOST_ARCH" ]; then
  echo "error: $ARCH packaging must run on a native $ARCH host (current: $HOST_ARCH)." >&2
  echo "Cross-compiling packaging is intentionally not claimed until a native runner validates it." >&2
  exit 2
fi

TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}"
DIST="${FLUX_DIST_DIR:-$ROOT/dist}"
EPOCH="${SOURCE_DATE_EPOCH:-$(date +%s)}"
NAME="Flux-$VERSION-linux-$ARCH"
STAGE="$DIST/$NAME"
APPDIR="$DIST/.Flux-$VERSION-linux-$ARCH"
DEBROOT="$DIST/.Flux-$VERSION-linux-$ARCH-deb"
RPMTOP="$(mktemp -d "${TMPDIR:-/tmp}/flux-rpm.XXXXXX")"
trap 'rm -rf "$RPMTOP"' EXIT

if [ "${FLUX_SKIP_BUILD:-0}" != "1" ]; then
  cargo build --release --locked -p flux-gui -p flux-tui --features cpal
fi
GUI_BIN="$TARGET_ROOT/release/flux"
TUI_BIN="$TARGET_ROOT/release/flux-tui"
for bin in "$GUI_BIN" "$TUI_BIN"; do
  [ -x "$bin" ] || { echo "error: missing release binary $bin" >&2; exit 1; }
done

mkdir -p "$DIST"
rm -rf "$STAGE" "$APPDIR" "$DEBROOT" "$RPMTOP"
mkdir -p "$STAGE/bin" "$STAGE/share/applications" "$STAGE/share/metainfo" \
  "$STAGE/share/doc/flux"
install -m755 "$GUI_BIN" "$STAGE/bin/flux"
install -m755 "$TUI_BIN" "$STAGE/bin/flux-tui"
install -m644 packaging/linux/io.github.neonmc23.flux.desktop \
  "$STAGE/share/applications/"
install -m644 packaging/linux/io.github.neonmc23.flux.metainfo.xml \
  "$STAGE/share/metainfo/"
mkdir -p "$STAGE/share/doc/flux"
for doc in README.md LICENSE THIRD_PARTY_NOTICES.md; do
  [ -f "$doc" ] && install -m644 "$doc" "$STAGE/share/doc/flux/"
done
mkdir -p "$STAGE/share/doc/flux/licenses"
install -m644 packaging/licenses/*.txt "$STAGE/share/doc/flux/licenses/"
for size in 16 24 32 48 64 128 256 512 1024; do
  icon="assets/icons/flux-$size.png"
  [ -f "$icon" ] || { echo "error: missing icon $icon" >&2; exit 1; }
  dir="$STAGE/share/icons/hicolor/${size}x${size}/apps"
  mkdir -p "$dir"
  install -m644 "$icon" "$dir/io.github.neonmc23.flux.png"
done
cat > "$STAGE/share/doc/flux/README-linux.txt" <<'EOF'
Flux for Linux
==============

Flux is a native, local-first music player. This archive contains the GUI
(`bin/flux`) and terminal (`bin/flux-tui`) applications and desktop integration
files. It does not contain user data.

Portable use
------------
Run from this directory with the bundled binaries on PATH:

    PATH="$PWD/bin:$PATH" flux

To install the desktop entry and icons for the current user without root:

    install -Dm755 bin/flux      "$HOME/.local/bin/flux"
    install -Dm755 bin/flux-tui  "$HOME/.local/bin/flux-tui"
    install -Dm644 share/applications/io.github.neonmc23.flux.desktop \
      "$HOME/.local/share/applications/io.github.neonmc23.flux.desktop"
    for size in 16 24 32 48 64 128 256 512 1024; do
      install -Dm644 "share/icons/hicolor/${size}x${size}/apps/io.github.neonmc23.flux.png" \
        "$HOME/.local/share/icons/hicolor/${size}x${size}/apps/io.github.neonmc23.flux.png"
    done
    install -Dm644 share/metainfo/io.github.neonmc23.flux.metainfo.xml \
      "$HOME/.local/share/metainfo/io.github.neonmc23.flux.metainfo.xml"

The package manager packages (`*.deb`, `*.rpm`) already place these files in the
system prefix. Uninstalling them removes application files only; the library and
settings remain under `~/.local/share/flux` and `~/.config/flux`.

Runtime requirements
--------------------
A Linux desktop with X11 or Wayland, ALSA/PipeWire/PulseAudio for audio output,
and the common winit/fontconfig runtime libraries. Native file dialogs also use
`zenity` or `kdialog` when available; Flux Browser and typed paths remain
available without them.

Offline
-------
No account, network service, analytics or telemetry is used.
EOF

# Normalize timestamps in the portable tree.
find "$STAGE" -exec touch -h -d "@$EPOCH" {} +

TARBALL="$DIST/$NAME.tar.gz"
(
  cd "$DIST"
  tar --sort=name --mtime="@$EPOCH" --owner=0 --group=0 --numeric-owner \
    -cf - "$NAME" | gzip -9n > "$TARBALL"
)

# Debian package.
mkdir -p "$DEBROOT/DEBIAN" "$DEBROOT/usr/bin" "$DEBROOT/usr/share"
install -m755 "$GUI_BIN" "$DEBROOT/usr/bin/flux"
install -m755 "$TUI_BIN" "$DEBROOT/usr/bin/flux-tui"
cp -a "$STAGE/share/." "$DEBROOT/usr/share/"
INSTALLED_SIZE="$(du -sk "$DEBROOT" | awk '{print $1}')"
cat > "$DEBROOT/DEBIAN/control" <<EOF
Package: flux
Version: $VERSION
Section: sound
Priority: optional
Architecture: $DEB_ARCH
Installed-Size: $INSTALLED_SIZE
Maintainer: NeonMC23 <piotr.souchon23@gmail.com>
Homepage: https://github.com/NeonMC23/Flux
Depends: libasound2t64 | libasound2, libc6, libgcc-s1, libfontconfig1, libx11-6, libxkbcommon0, libwayland-client0
Description: Local-first native music player
 Flux is a native desktop music player for local audio files. It manages a
 self-contained canonical library with stable artist, album, track and queue
 identities, managed artwork and lyrics, playlists, ratings and playback
 history. Import review is offline and never reads or rewrites audio tags.
EOF
chmod 755 "$DEBROOT/DEBIAN"
find "$DEBROOT" -exec touch -h -d "@$EPOCH" {} +
DEB="$DIST/${NAME}.deb"
dpkg-deb --root-owner-group --build "$DEBROOT" "$DEB" >/dev/null

# RPM package through a staged-tree spec; no source rebuild is performed.
mkdir -p "$RPMTOP/BUILD" "$RPMTOP/RPMS" "$RPMTOP/SOURCES" "$RPMTOP/SPECS" "$RPMTOP/SRPMS"
SOURCE_DATE_EPOCH="$EPOCH" rpmbuild -bb packaging/linux/flux.spec \
  --define "_topdir $RPMTOP" \
  --define "_buildhost reproducible" \
  --define "use_source_date_epoch_as_buildtime 1" \
  --define "flux_version $VERSION" \
  --define "flux_stage $STAGE" \
  --define "debug_package %{nil}" >/dev/null
RPM="$(find "$RPMTOP/RPMS" -name '*.rpm' -type f | head -1)"
[ -n "$RPM" ] || { echo "error: rpmbuild produced no artifact" >&2; exit 1; }
cp "$RPM" "$DIST/${NAME}.rpm"

(
  cd "$DIST"
  sha256sum "$(basename "$TARBALL")" "$(basename "$DEB")" "$(basename "$DIST/${NAME}.rpm")" \
    > "SHA256SUMS-linux-$ARCH.txt"
)

rm -rf "$DEBROOT" "$RPMTOP"
echo "Linux artifacts in $DIST"
ls -lh "$TARBALL" "$DEB" "$DIST/${NAME}.rpm"
