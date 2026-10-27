#!/usr/bin/env python3
"""Write THIRD-PARTY-NOTICES: every crate and web package a release ships, with its licence.

    scripts/third-party-notices.py [OUT]     # default: THIRD-PARTY-NOTICES

Needs `cargo about` (cargo install --locked cargo-about) and ui/node_modules
(cd ui && bun install --frozen-lockfile). The Rust part comes from about.toml
and about.hbs; the web part follows ui/package.json's dependencies (not the
dev ones, which never reach the bundle) through node_modules. Fonts and map
services are listed from what the UI actually uses.

Stops with an error when a crate or package has no licence it can name, so
nothing ships unlisted.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
UI = ROOT / "ui"

HEADER = """\
openpasture third-party notices

openpasture is Copyright 2026 Cody Menefee (openpasture), licensed under the
GNU Affero General Public License v3.0 only (LICENSE); op-protocol and op-geo
under the Apache License 2.0. It is built with the open source software
below, each under its own licence, reproduced here as those licences require.

Map services, used over the network, not shipped. Satellite imagery: Esri
World Imagery, (c) Esri, Maxar, Earthstar Geographics, under Esri's terms
and credited on the map. Place search on first run: OpenStreetMap Nominatim,
data (c) OpenStreetMap contributors, ODbL 1.0.
"""


def rust_notices() -> str:
    try:
        out = subprocess.run(
            ["cargo", "about", "generate", "--locked", "--fail", "--workspace", "about.hbs"],
            cwd=ROOT,
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError:
        sys.exit("cargo not found")
    except subprocess.CalledProcessError as e:
        sys.exit(f"cargo about failed:\n{e.stderr}")
    return out.stdout


def find_package(name: str, start: Path) -> Path | None:
    """Node's lookup: <dir>/node_modules/<name>, walking up to ui/."""
    d = start
    while True:
        candidate = d / "node_modules" / name / "package.json"
        if candidate.is_file():
            return candidate.parent
        if d == UI or UI not in d.parents:
            return None
        d = d.parent


def licence_text(pkg: Path) -> str | None:
    for f in sorted(pkg.iterdir()):
        if f.is_file() and f.name.upper().startswith(("LICENSE", "LICENCE", "COPYING")):
            return f.read_text(errors="replace").strip()
    return None


def web_packages() -> list[tuple[str, str, str, str | None]]:
    """(name, version, licence, text) for each runtime package reachable from ui/package.json."""
    if not (UI / "node_modules").is_dir():
        sys.exit("ui/node_modules is missing: cd ui && bun install --frozen-lockfile")
    root = json.loads((UI / "package.json").read_text())
    queue = [(name, UI) for name in root.get("dependencies", {})]
    seen: dict[Path, tuple[str, str, str, str | None]] = {}
    missing = []
    while queue:
        name, start = queue.pop()
        pkg = find_package(name, start)
        if pkg is None:
            missing.append(name)
            continue
        if pkg in seen:
            continue
        meta = json.loads((pkg / "package.json").read_text())
        licence = meta.get("license")
        if isinstance(licence, dict):
            licence = licence.get("type")
        if not licence and isinstance(meta.get("licenses"), list):
            licence = " OR ".join(l.get("type", "") for l in meta["licenses"])
        seen[pkg] = (meta.get("name", name), meta.get("version", "?"), licence or "", licence_text(pkg))
        deps = dict(meta.get("dependencies", {}))
        # Optional dependencies count only when installed for this platform.
        for opt in meta.get("optionalDependencies", {}):
            if find_package(opt, pkg):
                deps[opt] = ""
        queue.extend((dep, pkg) for dep in deps)
    if missing:
        sys.exit(f"not installed in ui/node_modules: {', '.join(sorted(set(missing)))}")
    unknown = [n for n, _, l, _ in seen.values() if not l]
    if unknown:
        sys.exit(f"web packages with no licence field: {', '.join(sorted(unknown))}")
    return sorted(seen.values(), key=lambda p: (p[0], p[1]))


def web_notices() -> str:
    parts = ["Web app packages (ui/, bundled into the app and the server)", "=" * 60, ""]
    for name, version, licence, text in web_packages():
        parts.append(f"{name} {version}  ({licence})")
        parts.append("-" * 60)
        parts.append(text or f"Licensed under {licence}. The package ships no licence file.")
        parts.append("")
    fonts = sorted((UI / "public/fonts").glob("OFL-*.txt"))
    if fonts:
        parts += ["Fonts", "=" * 60, ""]
        for f in fonts:
            parts.append(f"{f.stem.removeprefix('OFL-')}  (OFL-1.1)")
            parts.append("-" * 60)
            parts.append(f.read_text(errors="replace").strip())
            parts.append("")
    return "\n".join(parts)


def main() -> None:
    out = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "THIRD-PARTY-NOTICES"
    text = "\n".join([HEADER, rust_notices().strip(), "", web_notices()])
    out.write_text(text.rstrip() + "\n")
    print(f"{out} ({len(text) // 1024} KB)")


if __name__ == "__main__":
    main()
