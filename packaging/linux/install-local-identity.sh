#!/usr/bin/env bash
# Installs the Flux desktop identity for the current user from a source
# checkout. This is the missing link when running a development binary:
# Wayland compositors resolve application icons through the desktop file and
# the application id, not through the process image alone.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${1:-}"
if [[ -z "$BIN" && -n "${CARGO_TARGET_DIR:-}" ]]; then
  BIN="$CARGO_TARGET_DIR/debug/flux"
fi
if [[ -z "$BIN" || ! -x "$BIN" ]]; then
  BIN="$(command -v flux || true)"
fi
if [[ -z "$BIN" || ! -x "$BIN" ]]; then
  echo "usage: $0 [/absolute/path/to/flux]" >&2
  echo "The GUI binary was not found. Build it first, then pass its path." >&2
  exit 1
fi

DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
APP_ID="io.github.neonmc23.flux"
ICON_ROOT="$DATA_HOME/icons/hicolor"
APPS="$DATA_HOME/applications"

install -d "$APPS"
for size in 16 24 32 48 64 128 256 512 1024; do
  install -Dm644 "$ROOT/assets/icons/flux-$size.png" \
    "$ICON_ROOT/${size}x${size}/apps/$APP_ID.png"
done

sed "s|^Exec=.*|Exec=$BIN|" \
  "$ROOT/packaging/linux/$APP_ID.desktop" > "$APPS/$APP_ID.desktop"
chmod 0644 "$APPS/$APP_ID.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
  update-desktop-database "$APPS" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  gtk-update-icon-cache -f -t "$ICON_ROOT" >/dev/null 2>&1 || true
fi

echo "Installed Flux desktop identity:"
echo "  desktop: $APPS/$APP_ID.desktop"
echo "  icons:   $ICON_ROOT/*/apps/$APP_ID.png"
echo "  binary:  $BIN"
echo "Restart the application after installing the identity."
