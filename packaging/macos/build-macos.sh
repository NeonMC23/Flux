#!/bin/bash
# Builds Flux.app and archives on macOS. Never publishes anything.
#
# Usage:
#   packaging/macos/build-macos.sh                 # native architecture
#   FLUX_ARCH=arm64 packaging/macos/build-macos.sh
#   FLUX_ARCH=x86_64 packaging/macos/build-macos.sh
#   FLUX_UNIVERSAL=1 packaging/macos/build-macos.sh
#
# Signing is explicit:
#   CODESIGN_IDENTITY="Developer ID Application: ..." -> Developer ID signing
#   otherwise an ad-hoc signature is applied so the bundle is structurally valid
#   CODESIGN_IDENTITY with NOTARY_PROFILE set also submits for notarization.
# Unsigned/ad-hoc builds are not notarized and Gatekeeper will warn.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "error: macOS packaging must run on macOS (hdiutil/codesign/ditto/lipo)." >&2
  exit 2
fi

VERSION="$(awk '
  /^\[workspace.package\]/ { in_section=1; next }
  /^\[/ { in_section=0 }
  in_section && /^version *=/ { gsub(/[",]/, "", $3); print $3; exit }
' Cargo.toml)"
[ -n "$VERSION" ] || { echo "error: could not read workspace version" >&2; exit 1; }

TARGET_ROOT="${CARGO_TARGET_DIR:-$ROOT/target}"
DIST="${FLUX_DIST_DIR:-$ROOT/dist}"
APP_ID="io.github.neonmc23.flux"
COPYRIGHT="Copyright (c) 2026 NeonMC23"
JOBS="${FLUX_JOBS:-}"

target_for_arch() {
  case "$1" in
    arm64|aarch64) echo "aarch64-apple-darwin" ;;
    x86_64|amd64) echo "x86_64-apple-darwin" ;;
    *) echo "error: unsupported architecture $1" >&2; exit 2 ;;
  esac
}

sign_bundle() {
  local app="$1"
  if [ -n "${CODESIGN_IDENTITY:-}" ]; then
    echo "Signing with Developer ID identity from CODESIGN_IDENTITY"
    codesign --force --deep --options runtime --timestamp \
      --sign "$CODESIGN_IDENTITY" "$app"
    codesign --verify --deep --strict --verbose=2 "$app"
    if [ -n "${NOTARY_PROFILE:-}" ]; then
      local zip
      zip="$(mktemp -d)/Flux-notarize.zip"
      ditto -c -k --sequesterRsrc --keepParent "$app" "$zip"
      xcrun notarytool submit "$zip" --keychain-profile "$NOTARY_PROFILE" --wait
      xcrun stapler staple "$app"
      rm -rf "$(dirname "$zip")"
    fi
  else
    echo "No CODESIGN_IDENTITY supplied; applying an ad-hoc signature (not notarized)."
    codesign --force --deep --sign - "$app"
    codesign --verify --deep --strict --verbose=2 "$app"
  fi
}

make_app() {
  local bin="$1" tui="$2" stage="$3"
  local app="$stage/Flux.app"
  rm -rf "$app"
  mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
  install -m755 "$bin" "$app/Contents/MacOS/flux"
  install -m755 "$tui" "$app/Contents/MacOS/flux-tui"
  install -m644 "$ROOT/assets/icons/flux.icns" "$app/Contents/Resources/flux.icns"
  sed \
    -e "s|__APP_ID__|$APP_ID|g" \
    -e "s|__VERSION__|$VERSION|g" \
    -e "s|__COPYRIGHT__|$COPYRIGHT|g" \
    "$ROOT/packaging/macos/Info.plist.in" > "$app/Contents/Info.plist"
  printf 'APPL????' > "$app/Contents/PkgInfo"
  [ -f "$ROOT/README.md" ] && install -m644 "$ROOT/README.md" "$app/Contents/Resources/README.md"
  [ -f "$ROOT/LICENSE" ] && install -m644 "$ROOT/LICENSE" "$app/Contents/Resources/LICENSE"
  [ -f "$ROOT/THIRD_PARTY_NOTICES.md" ] && install -m644 "$ROOT/THIRD_PARTY_NOTICES.md" "$app/Contents/Resources/THIRD_PARTY_NOTICES.md"
  mkdir -p "$app/Contents/Resources/licenses"
  cp "$ROOT"/packaging/licenses/*.txt "$app/Contents/Resources/licenses/"
  plutil -lint "$app/Contents/Info.plist" >/dev/null
  sign_bundle "$app"
  echo "$app"
}

mkdir -p "$DIST"
if [ "${FLUX_UNIVERSAL:-0}" = "1" ]; then
  ARCH_LABEL="universal"
  BUILD_ARGS=()
  [ -n "$JOBS" ] && BUILD_ARGS+=(--jobs "$JOBS")
  cargo build --release --locked -p flux-gui -p flux-tui --features cpal \
    --target aarch64-apple-darwin "${BUILD_ARGS[@]}"
  cargo build --release --locked -p flux-gui -p flux-tui --features cpal \
    --target x86_64-apple-darwin "${BUILD_ARGS[@]}"
  ARM_BIN="$TARGET_ROOT/aarch64-apple-darwin/release/flux"
  X86_BIN="$TARGET_ROOT/x86_64-apple-darwin/release/flux"
  ARM_TUI="$TARGET_ROOT/aarch64-apple-darwin/release/flux-tui"
  X86_TUI="$TARGET_ROOT/x86_64-apple-darwin/release/flux-tui"
  STEP="$DIST/.Flux-$VERSION-macos-universal"
  rm -rf "$STEP"
  mkdir -p "$STEP"
  lipo -create "$ARM_BIN" "$X86_BIN" -output "$STEP/flux-universal"
  lipo -create "$ARM_TUI" "$X86_TUI" -output "$STEP/flux-tui-universal"
  lipo -info "$STEP/flux-universal"
  APP="$(make_app "$STEP/flux-universal" "$STEP/flux-tui-universal" "$STEP")"
else
  ARCH_LABEL="${FLUX_ARCH:-$(uname -m)}"
  TARGET="$(target_for_arch "$ARCH_LABEL")"
  BUILD_ARGS=()
  [ -n "$JOBS" ] && BUILD_ARGS+=(--jobs "$JOBS")
  cargo build --release --locked -p flux-gui -p flux-tui --features cpal \
    --target "$TARGET" "${BUILD_ARGS[@]}"
  STEP="$DIST/.Flux-$VERSION-macos-$ARCH_LABEL"
  rm -rf "$STEP"
  mkdir -p "$STEP"
  APP="$(make_app "$TARGET_ROOT/$TARGET/release/flux" \
    "$TARGET_ROOT/$TARGET/release/flux-tui" "$STEP")"
fi

DEST="$DIST/Flux-$VERSION-macos-$ARCH_LABEL"
rm -rf "$DEST"
mkdir -p "$DEST"
ditto "$APP" "$DEST/Flux.app"
ditto -c -k --sequesterRsrc --keepParent "$DEST/Flux.app" \
  "$DEST/Flux-$VERSION-macos-$ARCH_LABEL.app.zip"

if command -v hdiutil >/dev/null 2>&1; then
  hdiutil create -volname "Flux" -srcfolder "$DEST/Flux.app" -ov -format UDZO \
    "$DEST/Flux-$VERSION-macos-$ARCH_LABEL.dmg" >/dev/null
fi

(
  cd "$DIST"
  : > "SHA256SUMS-macos-$ARCH_LABEL.txt"
  for artifact in "Flux-$VERSION-macos-$ARCH_LABEL.app.zip" \
    "Flux-$VERSION-macos-$ARCH_LABEL.dmg"; do
    if [ -f "$artifact" ]; then
      shasum -a 256 "$artifact" >> "SHA256SUMS-macos-$ARCH_LABEL.txt"
    fi
  done
)
rm -rf "$STEP"
echo "macOS artifacts in $DEST"
ls -lh "$DEST"
if [ -z "${CODESIGN_IDENTITY:-}" ]; then
  echo "NOTE: ad-hoc signed only; Gatekeeper will warn and notarization was not performed."
fi
