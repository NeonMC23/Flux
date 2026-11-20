#!/usr/bin/env python3
"""Regenerates THIRD_PARTY_NOTICES.md from the locked dependency tree.

The release scripts include this file and packaging/licenses/ in artifacts.
Run from the repository root; no network access is required:

    python3 packaging/generate-notices.py
"""

from __future__ import annotations

import datetime
import json
import os
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TEXTS = {
    "0BSD",
    "Apache-2.0",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "BSL-1.0",
    "CC0-1.0",
    "ISC",
    "MIT",
    "Unicode-3.0",
    "Unlicense",
    "Zlib",
}


def main() -> None:
    raw = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--locked"],
        cwd=ROOT,
    )
    metadata = json.loads(raw)
    packages = sorted(
        (
            package
            for package in metadata["packages"]
            if not package["name"].startswith("flux-")
        ),
        key=lambda package: (package["name"].lower(), package["version"]),
    )

    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    generated = (
        datetime.datetime.fromtimestamp(int(epoch), datetime.UTC).date()
        if epoch
        else datetime.date.today()
    )
    missing = [
        name
        for name in sorted(TEXTS)
        if not (ROOT / "packaging" / "licenses" / f"{name}.txt").is_file()
    ]
    if missing:
        raise SystemExit(f"missing license text(s): {', '.join(missing)}")

    lines = [
        "# Third-party notices",
        "",
        "Flux is MIT-licensed. Release artifacts include the following third-party Rust crates,",
        "each under one of the license expressions below. Full standard license texts are in",
        "`packaging/licenses/`; the project copyright is in `LICENSE`.",
        "",
        f"Generated from `Cargo.lock` on {generated.isoformat()}.",
        "",
        "| Package | Version | License | Repository |",
        "|---|---|---|---|",
    ]
    for package in packages:
        license_name = package.get("license") or (
            f"SEE LICENSE FILE: {package['license_file']}"
            if package.get("license_file")
            else "UNKNOWN"
        )
        repository = package.get("repository") or ""
        lines.append(
            f"| `{package['name']}` | {package['version']} | "
            f"{license_name.replace('|', '/')} | {repository.replace('|', '/')} |"
        )
    lines.extend(
        [
            "",
            "License texts included: MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, 0BSD, ISC, Zlib,",
            "BSL-1.0, CC0-1.0, Unlicense, and Unicode-3.0. Where a crate offers an OR expression,",
            "the permissive option listed by the crate is used.",
            "",
        ]
    )
    (ROOT / "THIRD_PARTY_NOTICES.md").write_text("\n".join(lines), encoding="utf-8")
    print(f"wrote THIRD_PARTY_NOTICES.md for {len(packages)} packages")


if __name__ == "__main__":
    main()
