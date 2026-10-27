#!/usr/bin/env python3
"""Every advisory ignored in deny.toml says when to look at it again
("review by YYYY-MM-DD" in its reason); fail once a date has passed or an
ignore has none. CI runs this beside `cargo deny check`.

    python3 scripts/deny-ignores.py [deny.toml] [--today YYYY-MM-DD]
"""
import argparse
import datetime
import re
import sys

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    tomllib = None


def ignores(text):
    if tomllib is not None:
        return tomllib.loads(text).get("advisories", {}).get("ignore", [])
    # Python 3.9/3.10: the entries are one inline table per line.
    return [dict(re.findall(r'(\w+)\s*=\s*"([^"]*)"', line)) for line in text.splitlines() if line.strip().startswith('{ id = "')]


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("path", nargs="?", default="deny.toml")
    ap.add_argument("--today", type=datetime.date.fromisoformat, default=datetime.date.today())
    a = ap.parse_args()
    bad = 0
    for entry in ignores(open(a.path, encoding="utf-8").read()):
        if isinstance(entry, str):
            entry = {"id": entry, "reason": ""}
        m = re.search(r"review by (\d{4}-\d{2}-\d{2})", entry.get("reason", ""))
        if not m:
            print(f"::error::{a.path}: {entry['id']} has no 'review by YYYY-MM-DD' in its reason")
            bad += 1
        elif datetime.date.fromisoformat(m.group(1)) < a.today:
            print(f"::error::{a.path}: the ignore of {entry['id']} was to be reviewed by {m.group(1)}: drop it, or say why it still applies and set a new date")
            bad += 1
        else:
            print(f"ok: {entry['id']} (review by {m.group(1)})")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
