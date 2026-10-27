#!/usr/bin/env python3
"""Set the app version everywhere it is written down, or check that it agrees.

    scripts/set-version.py 0.2.0          # or v0.2.0: write it everywhere
    scripts/set-version.py --check        # every file agrees (CI)
    scripts/set-version.py --check v0.2.0 # ...and equals this tag (release)

The version lives in the workspace Cargo.toml, the workspace crates in
Cargo.lock that inherit it, apps/desktop/src-tauri/tauri.conf.json and
ui/package.json. The release workflow runs --check with the pushed tag, so a
tag that disagrees with the repo stops the release.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO = ROOT / "Cargo.toml"
LOCK = ROOT / "Cargo.lock"
TAURI = ROOT / "apps/desktop/src-tauri/tauri.conf.json"
UI = ROOT / "ui/package.json"

SEMVER = r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?"
# Workspace crates are the lock entries without a `source`.
LOCK_ENTRY = re.compile(r'(\[\[package\]\]\nname = "([^"]+)"\nversion = )"([^"]+)"(\n(?!source))')
# The first top-level "version" key in a package.json, formatting kept.
PACKAGE_VERSION = re.compile(r'^(  "version": )"([^"]*)"', re.M)


def workspace_section(text: str) -> re.Match:
    section = re.search(r"^\[workspace\.package\]\n(.*?)(?=^\[)", text, re.S | re.M)
    if not section:
        sys.exit("Cargo.toml has no [workspace.package] section")
    return section


def inheriting_crates() -> set[str]:
    """Workspace members that take the workspace version (`version.workspace = true`).

    A crate with its own version (say a published protocol crate) is left alone.
    """
    members = re.search(r"^members = \[(.*?)\]", CARGO.read_text(), re.S | re.M)
    names = set()
    for path in re.findall(r'"([^"]+)"', members.group(1) if members else ""):
        manifest = (ROOT / path / "Cargo.toml").read_text()
        package = re.search(r"^\[package\]\n(.*?)(?=^\[|\Z)", manifest, re.S | re.M)
        if package and re.search(r"^version\.workspace = true$", package.group(1), re.M):
            names.add(re.search(r'^name = "([^"]+)"$', package.group(1), re.M).group(1))
    return names


def parse(arg: str) -> str:
    version = arg.removeprefix("v")
    if not re.fullmatch(SEMVER, version):
        sys.exit(f"not a semver version: {version}")
    return version


def current() -> dict[str, str]:
    """Every place the version is written, and what it says there."""
    found = {}
    section = workspace_section(CARGO.read_text())
    m = re.search(r'^version = "(.*)"$', section.group(1), re.M)
    found["Cargo.toml"] = m.group(1) if m else "(missing)"
    crates = inheriting_crates()
    for m in LOCK_ENTRY.finditer(LOCK.read_text()):
        if m.group(2) in crates:
            found[f"Cargo.lock {m.group(2)}"] = m.group(3)
    found["tauri.conf.json"] = json.loads(TAURI.read_text()).get("version", "(missing)")
    m = PACKAGE_VERSION.search(UI.read_text())
    found["ui/package.json"] = m.group(2) if m else "(missing)"
    return found


def check(expected: str | None) -> None:
    found = current()
    want = expected or found["Cargo.toml"]
    wrong = {k: v for k, v in found.items() if v != want}
    if wrong:
        for k, v in wrong.items():
            print(f"{k}: {v} (expected {want})", file=sys.stderr)
        hint = f"scripts/set-version.py {want}"
        sys.exit(f"version mismatch; run `{hint}` and commit" if not expected else
                 f"tag {want} does not match the repo; run `{hint}`, commit, then tag that commit")
    print(f"version {want} in {len(found)} places")


def write(version: str) -> None:
    text = CARGO.read_text()
    section = workspace_section(text)
    body = re.sub(r'^version = ".*"$', f'version = "{version}"', section.group(1), count=1, flags=re.M)
    CARGO.write_text(text[: section.start(1)] + body + text[section.end(1) :])

    crates = inheriting_crates()
    LOCK.write_text(
        LOCK_ENTRY.sub(
            lambda m: f'{m.group(1)}"{version}"{m.group(4)}' if m.group(2) in crates else m.group(0),
            LOCK.read_text(),
        )
    )

    conf = json.loads(TAURI.read_text())
    conf["version"] = version
    TAURI.write_text(json.dumps(conf, indent=2) + "\n")

    ui = UI.read_text()
    if not PACKAGE_VERSION.search(ui):
        sys.exit("ui/package.json has no top-level version")
    UI.write_text(PACKAGE_VERSION.sub(lambda m: f'{m.group(1)}"{version}"', ui, count=1))

    print(f"version {version}")


def main() -> None:
    args = sys.argv[1:]
    if args[:1] == ["--check"] and len(args) <= 2:
        check(parse(args[1]) if len(args) == 2 else None)
    elif len(args) == 1 and not args[0].startswith("-"):
        write(parse(args[0]))
    else:
        sys.exit("usage: set-version.py <version> | --check [<version>]")


if __name__ == "__main__":
    main()
