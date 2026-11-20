#!/bin/bash
# Exercises the real Debian and RPM package-manager lifecycle in disposable
# containers. The host's package database and user data are never touched.
#
# Requirements: podman. The .deb/.rpm artifacts must already exist in DIST.
# If podman is unavailable the script reports SKIPPED unless
# FLUX_REQUIRE_PODMAN=1, in which case it fails.
#
# Usage:
#   packaging/linux/validate-package-managers.sh
#   FLUX_DIST_DIR=/path/to/artifacts packaging/linux/validate-package-managers.sh

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
DEB="$DIST/$NAME.deb"
RPM="$DIST/$NAME.rpm"
for artifact in "$DEB" "$RPM"; do
  [ -f "$artifact" ] || { echo "error: missing $artifact" >&2; exit 1; }
done

if ! command -v podman >/dev/null 2>&1; then
  if [ "${FLUX_REQUIRE_PODMAN:-0}" = "1" ]; then
    echo "error: podman is required for package-manager lifecycle validation" >&2
    exit 1
  fi
  echo "SKIPPED: podman is not installed; package-manager lifecycle was not tested."
  exit 0
fi

DEB_NAME="$(basename "$DEB")"
RPM_NAME="$(basename "$RPM")"
run_common_checks() {
  cat <<'COMMON'
set -eu
export HOME=/root
export DEBIAN_FRONTEND=noninteractive
mkdir -p /root/.local/share/flux/FluxLibrary /root/.config/flux
printf 'keep-library\n' > /root/.local/share/flux/FluxLibrary/lifecycle-sentinel
printf 'keep-config\n' > /root/.config/flux/lifecycle-sentinel
flux --version
flux-tui --version
[ -f /usr/share/applications/io.github.neonmc23.flux.desktop ]
[ -f /usr/share/metainfo/io.github.neonmc23.flux.metainfo.xml ]
[ -f /usr/share/icons/hicolor/256x256/apps/io.github.neonmc23.flux.png ]
# Container images often exclude /usr/share/doc via dpkg path-exclude; the
# package file list and the host artifact inspection cover documentation.
! find /usr/share -path '*FluxLibrary*' -print -quit | grep -q .
COMMON
}

echo "=== Debian lifecycle (Ubuntu 24.04) ==="
podman run --rm -i --security-opt label=disable \
  -v "$DIST:/artifacts:ro" \
  -e DEB_NAME="$DEB_NAME" -e VERSION="$VERSION" \
  docker.io/library/ubuntu:24.04 bash -s <<DEB_SCRIPT
set -eu
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends "/artifacts/\$DEB_NAME"

# Start with a synthetic older package so the real upgrade path is exercised.
mkdir -p /tmp/old
dpkg-deb -R "/artifacts/\$DEB_NAME" /tmp/old
sed -i 's/^Version: .*/Version: 0.0.9/' /tmp/old/DEBIAN/control
dpkg-deb --root-owner-group --build /tmp/old /tmp/flux-old.deb >/dev/null
apt-get remove -y -qq flux
dpkg -i /tmp/flux-old.deb >/dev/null
[ "\$(dpkg-query -W -f='\${Version}' flux)" = "0.0.9" ]
$(run_common_checks)
apt-get install -y -qq "/artifacts/\$DEB_NAME"
[ "\$(dpkg-query -W -f='\${Version}' flux)" = "\$VERSION" ]
[ "\$(cat /root/.local/share/flux/FluxLibrary/lifecycle-sentinel)" = "keep-library" ]
[ "\$(cat /root/.config/flux/lifecycle-sentinel)" = "keep-config" ]

apt-get install -y -qq --reinstall "/artifacts/\$DEB_NAME"
[ "\$(dpkg-query -W -f='\${Version}' flux)" = "\$VERSION" ]
[ "\$(cat /root/.local/share/flux/FluxLibrary/lifecycle-sentinel)" = "keep-library" ]

apt-get remove -y -qq flux
if dpkg-query -W -f='\${Status}' flux 2>/dev/null | grep -q 'install ok installed'; then
  echo 'error: .deb package remains installed' >&2; exit 1
fi
[ ! -e /usr/bin/flux ]
[ ! -e /usr/bin/flux-tui ]
[ ! -e /usr/share/applications/io.github.neonmc23.flux.desktop ]
[ ! -e /usr/share/metainfo/io.github.neonmc23.flux.metainfo.xml ]
[ "\$(cat /root/.local/share/flux/FluxLibrary/lifecycle-sentinel)" = "keep-library" ]
[ "\$(cat /root/.config/flux/lifecycle-sentinel)" = "keep-config" ]
echo "Debian package lifecycle passed"
DEB_SCRIPT

echo "=== RPM lifecycle (Fedora 44) ==="
podman run --rm -i --security-opt label=disable \
  -v "$DIST:/artifacts:ro" \
  -e RPM_NAME="$RPM_NAME" -e VERSION="$VERSION" \
  registry.fedoraproject.org/fedora:44 bash -s <<RPM_SCRIPT
set -eu
export HOME=/root
dnf install -y -q "/artifacts/\$RPM_NAME"
[ "\$(rpm -q --qf '%{VERSION}' flux)" = "\$VERSION" ]
$(run_common_checks)
rpm -Uvh --replacepkgs "/artifacts/\$RPM_NAME" >/dev/null
[ "\$(rpm -q --qf '%{VERSION}' flux)" = "\$VERSION" ]
[ "\$(cat /root/.local/share/flux/FluxLibrary/lifecycle-sentinel)" = "keep-library" ]
dnf remove -y -q flux
if rpm -q flux >/dev/null 2>&1; then
  echo 'error: RPM package remains installed' >&2; exit 1
fi
[ ! -e /usr/bin/flux ]
[ ! -e /usr/bin/flux-tui ]
[ ! -e /usr/share/applications/io.github.neonmc23.flux.desktop ]
[ ! -e /usr/share/metainfo/io.github.neonmc23.flux.metainfo.xml ]
[ "\$(cat /root/.local/share/flux/FluxLibrary/lifecycle-sentinel)" = "keep-library" ]
[ "\$(cat /root/.config/flux/lifecycle-sentinel)" = "keep-config" ]
echo "RPM package lifecycle passed"
RPM_SCRIPT

echo "Package-manager lifecycle validation passed for $NAME"
