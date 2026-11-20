#!/bin/bash
# Verifies a macOS Flux.app bundle and archives on a disposable macOS runner.
# Does not publish or notarize anything.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
VERSION="$(awk '
  /^\[workspace.package\]/ { in_section=1; next }
  /^\[/ { in_section=0 }
  in_section && /^version *=/ { gsub(/[",]/, "", $3); print $3; exit }
' Cargo.toml)"
ARCH="${FLUX_ARCH:-$(uname -m)}"
DIST="${FLUX_DIST_DIR:-$ROOT/dist}"
DEST="$DIST/Flux-$VERSION-macos-$ARCH"
APP="$DEST/Flux.app"
[ -d "$APP" ] || { echo "error: missing $APP" >&2; exit 1; }

plutil -lint "$APP/Contents/Info.plist" >/dev/null
[ "$(plutil -extract CFBundleIdentifier raw "$APP/Contents/Info.plist")" = "io.github.neonmc23.flux" ]
[ "$(plutil -extract CFBundleShortVersionString raw "$APP/Contents/Info.plist")" = "$VERSION" ]
[ "$(plutil -extract CFBundleIconFile raw "$APP/Contents/Info.plist")" = "flux.icns" ]
[ -f "$APP/Contents/Resources/flux.icns" ]

EXPECTED="Flux $VERSION (io.github.neonmc23.flux)"
[ "$("$APP/Contents/MacOS/flux" --version)" = "$EXPECTED" ]
[ "$("$APP/Contents/MacOS/flux-tui" --version)" = "$EXPECTED" ]
lipo -info "$APP/Contents/MacOS/flux"
codesign --verify --deep --strict --verbose=2 "$APP"

# Verify the ZIP expands to the same bundle and version.
TMP="$(mktemp -d "${TMPDIR:-/tmp}/flux-macos-verify.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
ZIP="$DEST/Flux-$VERSION-macos-$ARCH.app.zip"
[ -f "$ZIP" ] || { echo "error: missing $ZIP" >&2; exit 1; }
ditto -x -k "$ZIP" "$TMP"
[ "$("$TMP/Flux.app/Contents/MacOS/flux" --version)" = "$EXPECTED" ]
codesign --verify --deep --strict "$TMP/Flux.app"

# Mount and inspect the DMG if present.
DMG="$DEST/Flux-$VERSION-macos-$ARCH.dmg"
if [ -f "$DMG" ]; then
  MOUNT="$TMP/mount"
  mkdir -p "$MOUNT"
  hdiutil attach "$DMG" -nobrowse -readonly -mountpoint "$MOUNT" >/dev/null
  [ -d "$MOUNT/Flux.app" ]
  [ "$("$MOUNT/Flux.app/Contents/MacOS/flux" --version)" = "$EXPECTED" ]
  hdiutil detach "$MOUNT" >/dev/null
fi

(
  cd "$DIST"
  shasum -a 256 -c "SHA256SUMS-macos-$ARCH.txt"
)

echo "macOS artifact verification passed for Flux-$VERSION-macos-$ARCH"
