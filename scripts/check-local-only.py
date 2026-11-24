#!/usr/bin/env python3
"""Fail when a dev script can reach a server that isn't on this machine.

Soak runs, sweeps, shotdiff and verification drivers make farms, move fences
and flood reports. Every one of them that opens a connection must put the
address through the local-only check first: `local_only()`/`local_hostport()`
from scripts/local_only.py, or `localOnly()` from scripts/local-only.mjs (a
skill's driver may carry its own `local_only.py`). No script may name a
staging or production host at all.

    scripts/check-local-only.py [--root <checkout>]

Scans scripts/ and .claude/skills/*/bin/.
"""

import argparse
import pathlib
import re
import sys

CONNECTS = {
    ".py": re.compile(r"urllib\.request|http\.client|\brequests\.|socket\.create_connection|asyncio\.open_connection|websocket"),
    ".ts": re.compile(r"\bfetch\(|new WebSocket\(|\.goto\("),
    ".mjs": re.compile(r"\bfetch\(|new WebSocket\(|\.goto\("),
    ".js": re.compile(r"\bfetch\(|new WebSocket\(|\.goto\("),
    ".sh": re.compile(r"\bcurl\b|\bwget\b|/dev/tcp/"),
}
NO_EXT_SHELL = re.compile(r"\A#!.*\b(ba|z)?sh\b")
GUARDED = re.compile(r"\blocal_only\(|\blocal_hostport\(|\blocalOnly\(|local_only\.py")
HOSTS = re.compile(r"railway\.app|openpasture\.dev|op-server-staging", re.I)
GUARDS = {"scripts/local_only.py", "scripts/local-only.mjs", "scripts/check-local-only.py", "scripts/test_local_only.py", "scripts/local-only.test.mjs"}


def files(root):
    dirs = [root / "scripts", *sorted((root / ".claude" / "skills").glob("*/bin"))]
    for d in dirs:
        if not d.is_dir():
            continue
        for p in sorted(d.rglob("*")):
            if p.is_file() and "node_modules" not in p.parts:
                yield p


def kind(p, text):
    if p.suffix in CONNECTS:
        return p.suffix
    if not p.suffix and NO_EXT_SHELL.match(text):
        return ".sh"
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=pathlib.Path(__file__).resolve().parent.parent)
    root = pathlib.Path(ap.parse_args().root).resolve()
    bad = 0
    for p in files(root):
        rel = p.relative_to(root).as_posix()
        if rel in GUARDS:
            continue
        try:
            text = p.read_text()
        except (UnicodeDecodeError, OSError):
            continue
        k = kind(p, text)
        if k is None:
            continue
        for n, line in enumerate(text.splitlines(), 1):
            if HOSTS.search(line):
                bad += 1
                print(f"::error file={rel},line={n}::{rel}:{n} names a staging or production host. Dev scripts drive this machine only; take the address as an argument and pass it through local_only() (scripts/local_only.py) or localOnly() (scripts/local-only.mjs).")
        m = CONNECTS[k].search(text)
        if m and not GUARDED.search(text):
            n = text[: m.start()].count("\n") + 1
            bad += 1
            fix = "localOnly() from scripts/local-only.mjs" if k in (".ts", ".mjs", ".js") else "local_only() or local_hostport() from scripts/local_only.py"
            print(f"::error file={rel},line={n}::{rel}:{n} connects ({m[0]}) without the local-only check. Put the server address through {fix} before the first request, so staging and production are refused.")
    if bad:
        print(f"{bad} script problem(s)", file=sys.stderr)
        return 1
    print(f"every script that connects checks its address is local ({root})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
