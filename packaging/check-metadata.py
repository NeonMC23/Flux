#!/usr/bin/env python3
"""Checks release identity and version consistency across packaging metadata.

Run from anywhere after editing Cargo.toml, desktop/AppStream metadata, icons or
platform templates. CI runs this before packaging.
"""

from __future__ import annotations

import plistlib
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
APP_ID = "io.github.neonmc23.flux"


def workspace_version() -> str:
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    section = re.search(
        r"^\[workspace\.package\]\s*$(.*?)(?=^\[)",
        text,
        flags=re.MULTILINE | re.DOTALL,
    )
    if not section:
        raise SystemExit("missing [workspace.package]")
    match = re.search(r'^version\s*=\s*"([^"]+)"', section.group(1), re.MULTILINE)
    if not match:
        raise SystemExit("missing workspace version")
    return match.group(1)


def require(condition: bool, message: str) -> None:
    if not condition:
        raise SystemExit(f"identity check failed: {message}")


def main() -> None:
    version = workspace_version()
    desktop = (ROOT / "packaging/linux/io.github.neonmc23.flux.desktop").read_text(
        encoding="utf-8"
    )
    require("Name=Flux" in desktop, "desktop Name")
    require("Exec=flux" in desktop, "desktop Exec")
    require(f"Icon={APP_ID}" in desktop, "desktop Icon")
    require(f"StartupWMClass={APP_ID}" in desktop, "desktop StartupWMClass")

    metainfo = (
        ROOT / "packaging/linux/io.github.neonmc23.flux.metainfo.xml"
    ).read_text(encoding="utf-8")
    require(f"<id>{APP_ID}</id>" in metainfo, "AppStream id")
    require(f"{APP_ID}.desktop" in metainfo, "AppStream launchable")
    require(f'<release version="{version}"' in metainfo, "AppStream release version")
    require("<metadata_license>MIT</metadata_license>" in metainfo, "metadata license")
    require("<project_license>MIT</project_license>" in metainfo, "project license")

    template = (ROOT / "packaging/macos/Info.plist.in").read_text(encoding="utf-8")
    plist = plistlib.loads(
        template.replace("__APP_ID__", APP_ID)
        .replace("__VERSION__", version)
        .replace("__COPYRIGHT__", "Copyright (c) 2026 NeonMC23")
        .encode()
    )
    require(plist["CFBundleIdentifier"] == APP_ID, "macOS bundle id")
    require(plist["CFBundleExecutable"] == "flux", "macOS executable")
    require(plist["CFBundleIconFile"] == "flux.icns", "macOS icon")
    require(plist["CFBundleShortVersionString"] == version, "macOS version")

    manifest = (ROOT / "packaging/windows/flux.manifest").read_text(encoding="utf-8")
    require("<assembly" in manifest and "asInvoker" in manifest, "Windows manifest")
    installer = (ROOT / "packaging/windows/flux.iss").read_text(encoding="utf-8")
    require(
        "AppId={{96707750-32C7-4982-8664-11AE5B7B5C1B}" in installer,
        "stable installer AppId",
    )
    require("AppName=Flux" in installer, "installer name")

    for size in (16, 24, 32, 48, 64, 128, 256, 512, 1024):
        require(
            (ROOT / "assets/icons" / f"flux-{size}.png").is_file(),
            f"icon {size}",
        )
    require((ROOT / "assets/icons/flux.ico").is_file(), "ICO")
    require((ROOT / "assets/icons/flux.icns").is_file(), "ICNS")

    print(f"metadata identity/version OK for Flux {version} ({APP_ID})")


if __name__ == "__main__":
    try:
        main()
    except SystemExit:
        raise
    except Exception as error:  # pragma: no cover - diagnostic path
        print(f"identity check failed: {error}", file=sys.stderr)
        raise SystemExit(1)
