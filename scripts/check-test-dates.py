#!/usr/bin/env python3
"""Fail when a change adds a future date literal to a test.

A test that sends the server a date written out in the future passes until
that day, then fails for everyone: the server compares it with the real clock
(op-engine/tests/schedules.rs had 2026-10-31 as a schedule's start, and its
first fix moved it to 2027-11-06, the same trap a year later). Only lines a
change adds are checked, so dates already on main don't fail unrelated work.

    scripts/check-test-dates.py [--base origin/main] [--head <commit>] [--today YYYY-MM-DD]

Without --head it checks the working tree against the merge base with --base,
committed or not. CI passes the pull request's base. --today defaults to the
UTC date.

A date that never meets the clock (pure calendar arithmetic) can stay: put
`fixed-date: <why>` on its line.
"""

import argparse
import datetime
import re
import subprocess
import sys

TEST_PATH = re.compile(r"(^|/)tests?/|\.test\.(ts|tsx|js|mjs)$|_test\.rs$|(^|/)test_[^/]*\.py$")
DATE = re.compile(r"(?<![\d-])(20\d\d)-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])(?![\d])")
ALLOW = re.compile(r"fixed-date:\s*\S.{8,}")


def git(*args):
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def added_lines(base, head):
    if head:
        diff = git("diff", "-U0", "--no-color", "--no-ext-diff", f"{base}..{head}")
    else:
        mb = git("merge-base", base, "HEAD").strip()
        diff = git("diff", "-U0", "--no-color", "--no-ext-diff", mb)
    path, line = None, 0
    for raw in diff.splitlines():
        if raw.startswith("+++ "):
            path = raw[6:] if raw.startswith("+++ b/") else None
        elif raw.startswith("@@"):
            line = int(re.search(r"\+(\d+)", raw).group(1))
        elif raw.startswith("+") and path:
            yield path, line, raw[1:]
            line += 1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--base", default="origin/main")
    ap.add_argument("--head")
    ap.add_argument("--today", default=datetime.datetime.now(datetime.timezone.utc).date().isoformat())
    a = ap.parse_args()
    today = datetime.date.fromisoformat(a.today)
    bad = 0
    for path, n, text in added_lines(a.base, a.head):
        if not TEST_PATH.search(path) or ALLOW.search(text):
            continue
        for m in DATE.finditer(text):
            try:
                d = datetime.date(int(m[1]), int(m[2]), int(m[3]))
            except ValueError:
                continue
            if d >= today:
                bad += 1
                print(
                    f"::error file={path},line={n}::{path}:{n}: {m[0]} is a date on or after today ({today}) written into a test. "
                    "It goes stale on that day when the code compares it with the clock, as 2026-10-31 in "
                    "crates/op-engine/tests/schedules.rs did. Derive it from Utc::now() (see saturday_before_next_fall_back "
                    "there) or use a past date. If it never meets the clock, add `fixed-date: <why>` on the line."
                )
    if bad:
        print(f"{bad} future date(s) added to tests", file=sys.stderr)
        return 1
    print(f"no future dates added to tests (base {a.base}, today {today})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
