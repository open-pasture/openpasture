#!/usr/bin/env python3
"""Set the app version everywhere it is written down.

    scripts/set-version.py 0.2.0     # or v0.2.0

Updates the workspace version in Cargo.toml, the workspace crates in
Cargo.lock, and apps/desktop/src-tauri/tauri.conf.json. The release workflow
runs it with the pushed tag, so the tag is the version.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit("usage: set-version.py <version>")
    version = sys.argv[1].removeprefix("v")
    if not re.fullmatch(r"\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?", version):
        sys.exit(f"not a semver version: {version}")

    cargo = ROOT / "Cargo.toml"
    text = cargo.read_text()
    section = re.search(r"^\[workspace\.package\]\n(.*?)(?=^\[)", text, re.S | re.M)
    if not section:
        sys.exit("Cargo.toml has no [workspace.package] section")
    body = re.sub(r'^version = ".*"$', f'version = "{version}"', section.group(1), count=1, flags=re.M)
    text = text[: section.start(1)] + body + text[section.end(1) :]
    cargo.write_text(text)

    # Workspace crates are the lock entries without a `source`.
    lock = ROOT / "Cargo.lock"
    lock.write_text(
        re.sub(
            r'(\[\[package\]\]\nname = "[^"]+"\nversion = )"[^"]+"(\n(?!source))',
            lambda m: f'{m.group(1)}"{version}"{m.group(2)}',
            lock.read_text(),
        )
    )

    conf_path = ROOT / "apps/desktop/src-tauri/tauri.conf.json"
    conf = json.loads(conf_path.read_text())
    conf["version"] = version
    conf_path.write_text(json.dumps(conf, indent=2) + "\n")

    print(f"version {version}")


if __name__ == "__main__":
    main()
