#!/bin/bash
# Validates Linux artifacts without installing into the host or touching user
# data. Run after packaging/linux/build-packages.sh.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIST="${FLUX_DIST_DIR:-$ROOT/dist}"
VERSION="$(awk '
  /^\[workspace.package\]/ { in_section=1; next }
  /^\[/ { in_section=0 }
  in_section && /^version *=/ { gsub(/[",]/, "", $3); print $3; exit }
' "$ROOT/Cargo.toml")"
ARCH="${FLUX_ARCH:-$(uname -m)}"
NAME="Flux-$VERSION-linux-$ARCH"
TARBALL="$DIST/$NAME.tar.gz"
DEB="$DIST/$NAME.deb"
RPM="$DIST/$NAME.rpm"
for artifact in "$TARBALL" "$DEB" "$RPM"; do
  [ -f "$artifact" ] || { echo "error: missing $artifact" >&2; exit 1; }
done

TMP="$(mktemp -d "${TMPDIR:-/tmp}/flux-package-validate.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
cat > "$TMP/asound.conf" <<'ALSA'
pcm.!default { type null }
ctl.!default { type null }
ALSA

assert_version() {
  local bin="$1"
  local output
  output="$("$bin" --version)"
  [ "$output" = "Flux $VERSION (io.github.neonmc23.flux)" ] || {
    echo "error: unexpected version output from $bin: $output" >&2
    exit 1
  }
}

# Portable archive.
mkdir -p "$TMP/portable"
tar -xzf "$TARBALL" -C "$TMP/portable"
PORTABLE="$TMP/portable/$NAME"
if tar -tzf "$TARBALL" | grep -Eq '(^/)|(\.\./)'; then
  echo "error: portable archive contains an absolute or traversal path" >&2
  exit 1
fi
if tar -tvzf "$TARBALL" | awk '{print substr($1,1,1)}' | grep -qvE '^[-d]'; then
  echo "error: portable archive contains a symlink or special file" >&2
  exit 1
fi
if tar -tzf "$TARBALL" | grep -Eq 'FluxLibrary|\.flux/|queue\.json|ratings\.flux|history\.flux'; then
  echo "error: portable archive contains user-library data" >&2
  exit 1
fi
assert_version "$PORTABLE/bin/flux"
assert_version "$PORTABLE/bin/flux-tui"
desktop-file-validate "$PORTABLE/share/applications/io.github.neonmc23.flux.desktop"
appstreamcli validate --no-net "$PORTABLE/share/metainfo/io.github.neonmc23.flux.metainfo.xml" >/dev/null

# Debian package metadata and extraction.
dpkg-deb -I "$DEB" >/dev/null
dpkg-deb -x "$DEB" "$TMP/deb"
assert_version "$TMP/deb/usr/bin/flux"
assert_version "$TMP/deb/usr/bin/flux-tui"
[ -f "$TMP/deb/usr/share/applications/io.github.neonmc23.flux.desktop" ]

# RPM metadata, payload, and extraction.
rpm -qip "$RPM" >/dev/null
rpm -qlp "$RPM" | grep -q '/usr/bin/flux$'
mkdir -p "$TMP/rpm"
( cd "$TMP/rpm" && rpm2cpio "$RPM" | cpio -idm --quiet )
assert_version "$TMP/rpm/usr/bin/flux"
assert_version "$TMP/rpm/usr/bin/flux-tui"

# First-run GUI startup from the extracted portable tree, with isolated user
# data. The timeout exit is expected; any stderr output or other exit is not.
XDG="$TMP/xdg"
mkdir -p "$XDG/config" "$XDG/data"
set +e
timeout 8 env ALSA_CONFIG_PATH="$TMP/asound.conf" HOME="$XDG" \
  XDG_CONFIG_HOME="$XDG/config" XDG_DATA_HOME="$XDG/data" \
  "$PORTABLE/bin/flux" >"$TMP/gui.out" 2>"$TMP/gui.err"
status=$?
set -e
if [ "$status" -ne 124 ]; then
  echo "error: packaged GUI exited $status" >&2
  sed -n '1,80p' "$TMP/gui.err" >&2
  exit 1
fi
# ALSA's C library writes environment diagnostics when the CI host has no
# sound card. Those are not Flux errors; any other stderr remains fatal.
if grep -vE '^(ALSA lib |Cannot connect to server|jack server is not running)' \
  "$TMP/gui.err" | grep -q .; then
  echo "error: packaged GUI wrote stderr:" >&2
  cat "$TMP/gui.err" >&2
  exit 1
fi
[ -d "$XDG/data/flux/FluxLibrary" ] || { echo "error: first-run canonical library was not created" >&2; exit 1; }

# Simulate a reinstall/upgrade from a fresh extraction over the same user data,
# then confirm an application-owned sentinel and the library survive.
mkdir -p "$XDG/config/flux"
printf 'keep-config\n' > "$XDG/config/flux/upgrade-sentinel"
printf 'keep-library\n' > "$XDG/data/flux/FluxLibrary/upgrade-sentinel"
mkdir -p "$TMP/upgrade"
tar -xzf "$TARBALL" -C "$TMP/upgrade"
UPGRADE="$TMP/upgrade/$NAME"
set +e
timeout 6 env ALSA_CONFIG_PATH="$TMP/asound.conf" HOME="$XDG" \
  XDG_CONFIG_HOME="$XDG/config" XDG_DATA_HOME="$XDG/data" \
  "$UPGRADE/bin/flux" >"$TMP/upgrade.out" 2>"$TMP/upgrade.err"
status=$?
set -e
if [ "$status" -ne 124 ]; then
  echo "error: upgraded GUI exited $status" >&2
  sed -n '1,80p' "$TMP/upgrade.err" >&2
  exit 1
fi
if grep -vE '^(ALSA lib |Cannot connect to server|jack server is not running)' \
  "$TMP/upgrade.err" | grep -q .; then
  echo "error: upgraded GUI wrote stderr:" >&2
  cat "$TMP/upgrade.err" >&2
  exit 1
fi
[ "$(cat "$XDG/config/flux/upgrade-sentinel")" = "keep-config" ]
[ "$(cat "$XDG/data/flux/FluxLibrary/upgrade-sentinel")" = "keep-library" ]

# Simulate uninstallation by deleting the application tree; user data must stay.
rm -rf "$PORTABLE" "$UPGRADE" "$TMP/deb" "$TMP/rpm"
[ -d "$XDG/data/flux/FluxLibrary" ] || { echo "error: application removal deleted user data" >&2; exit 1; }
[ -d "$XDG/config" ] || { echo "error: application removal deleted config" >&2; exit 1; }

# Checksums.
( cd "$DIST" && sha256sum --check "SHA256SUMS-linux-$ARCH.txt" >/dev/null )

echo "Linux package validation passed for $NAME"
