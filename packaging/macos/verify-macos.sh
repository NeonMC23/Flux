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
"$APP/Contents/MacOS/flux" --help | grep -q 'Usage: flux'
"$APP/Contents/MacOS/flux-tui" --help | grep -q 'Usage: flux-tui'
lipo -info "$APP/Contents/MacOS/flux"
codesign --verify --deep --strict --verbose=2 "$APP"

# Report the actual signing state instead of assuming an identity.
echo "Code signing state:"
codesign -dvv "$APP" 2>&1 | sed -n '1,20p' || true
if codesign -dvv "$APP" 2>&1 | grep -q 'Authority=Developer ID Application'; then
  echo "Developer ID signed."
elif codesign -dvv "$APP" 2>&1 | grep -q 'Signature=adhoc'; then
  echo "Ad-hoc signed only; not notarized."
else
  echo "Signing state could not be classified."
fi
if spctl -a -vv "$APP" >/dev/null 2>&1; then
  echo "Gatekeeper assessment passed."
else
  echo "Gatekeeper assessment does not pass (expected without Developer ID/notarization)."
fi

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

# Launch the packaged GUI against a disposable, non-ASCII home directory and
# verify first-run library creation and reopening. This proves process startup
# and persistence; it is not an audio-output test.
launch_gui() {
  local home="$1"
  local label="$2"
  local library="$home/Library/Application Support/flux/FluxLibrary"
  local out="$TMP/$label.out"
  local err="$TMP/$label.err"
  mkdir -p "$home"
  HOME="$home" "$APP/Contents/MacOS/flux" >"$out" 2>"$err" &
  local pid=$!
  local waited=0
  while [ ! -f "$library/catalog.flux" ] && [ "$waited" -lt 80 ]; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid" || true
      echo "error: GUI $label exited before creating its library" >&2
      sed -n '1,80p' "$err" >&2
      return 1
    fi
    sleep 0.25
    waited=$((waited + 1))
  done
  if [ ! -f "$library/catalog.flux" ]; then
    kill -KILL "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "error: GUI $label did not create a first-run canonical library" >&2
    sed -n '1,80p' "$err" >&2
    return 1
  fi
  sleep 1
  if ! kill -0 "$pid" 2>/dev/null; then
    wait "$pid" || true
    echo "error: GUI $label exited after first-run setup" >&2
    sed -n '1,80p' "$err" >&2
    return 1
  fi
  kill "$pid" 2>/dev/null || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.2
  done
  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  if [ -s "$err" ]; then
    echo "GUI $label stderr:"
    sed -n '1,40p' "$err"
  fi
}

GUI_HOME="$TMP/home-ünicode-音乐"
launch_gui "$GUI_HOME" 'first-run'
launch_gui "$GUI_HOME" 'reopen'

(
  cd "$DEST"
  shasum -a 256 -c "SHA256SUMS-macos-$ARCH.txt"
)

echo "macOS artifact verification passed for Flux-$VERSION-macos-$ARCH"
