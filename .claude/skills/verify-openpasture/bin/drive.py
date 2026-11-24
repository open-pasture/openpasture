#!/usr/bin/env python3
"""Drive a running verify-openpasture stack over HTTP, the way the app and the collars do.

    drive.py doctor
    drive.py <journey>       health | collar-reports | report-rate-limit

Reads OP_BASE_URL, OP_TOKEN, OP_DATA_DIR and OP_VERIFY_RUN from the
environment (`bin/verify` sets them from the run's stack.env). Every request
and response goes to $OP_VERIFY_RUN/evidence/<journey>.jsonl. Exit 0: proved.
Exit 1: the behaviour is wrong. Exit 2: the stack isn't fit to drive.
Exit 3: OP_BASE_URL is not local, refused before any connection.
"""

import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from local_only import NotLocal, local_only  # noqa: E402

try:
    BASE = local_only(os.environ.get("OP_BASE_URL", ""))
except NotLocal as e:
    print(f"refused: {e}", file=sys.stderr)
    sys.exit(3)

# Only now, with the URL checked, is the HTTP client loaded.
import urllib.error  # noqa: E402
import urllib.request  # noqa: E402

TOKEN = os.environ.get("OP_TOKEN", "")
DATA = os.environ.get("OP_DATA_DIR", "")
RUN = os.environ.get("OP_VERIFY_RUN", "")


class Journey:
    def __init__(self, name):
        self.name = name
        os.makedirs(os.path.join(RUN, "evidence"), exist_ok=True)
        self.log = open(os.path.join(RUN, "evidence", f"{name}.jsonl"), "a")
        self.failures = []

    def req(self, method, path, body=None, auth=None, note=None):
        """One request to the stack. `auth` is the whole Authorization header; default the app token."""
        headers = {"Authorization": auth if auth is not None else f"Bearer {TOKEN}"}
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
        r = urllib.request.Request(BASE + path, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(r, timeout=20) as res:
                status, raw = res.status, res.read()
        except urllib.error.HTTPError as e:
            status, raw = e.code, e.read()
        try:
            out = json.loads(raw) if raw else None
        except ValueError:
            out = raw.decode(errors="replace")[:500]
        shown = None if auth is None else auth.replace(auth.strip().split()[-1], "<collar key>")
        self.log.write(json.dumps({"t": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "method": method, "path": path, "auth": shown, "note": note, "status": status, "body": out}) + "\n")
        self.log.flush()
        return status, out

    def check(self, ok, what):
        print(("PASS " if ok else "FAIL ") + what)
        if not ok:
            self.failures.append(what)

    def done(self):
        print(f"{self.name}: {'PASS' if not self.failures else 'FAIL'} (evidence {self.log.name})")
        return 0 if not self.failures else 1


def doctor():
    j = Journey("doctor")
    try:
        s, h = j.req("GET", "/healthz")
    except OSError as e:
        print(f"doctor: nothing answers at {BASE} ({e}); run bin/verify launch")
        return 2
    j.check(s == 200 and isinstance(h, dict) and h.get("status") == "ok", f"/healthz is 200 ok (got {s} {h.get('status') if isinstance(h, dict) else h})")
    s, st = j.req("GET", "/api/state")
    j.check(s == 200, f"the app token is accepted (/api/state {s})")
    farm = (st or {}).get("farm") if isinstance(st, dict) else None
    j.check(bool(farm) and farm.get("name") == "Simulated farm", f"this is the simulated farm the launch made (farm {farm and farm.get('name')!r})")
    j.check(os.path.exists(os.path.join(DATA, "SIMULATED-FARM")), f"the data dir {DATA} carries SIMULATED-FARM")
    return 0 if not j.failures else 2


def health():
    j = Journey("health")
    s, h = j.req("GET", "/healthz")
    j.check(s == 200 and h.get("status") == "ok", f"/healthz answers 200 ok (got {s})")
    names = {x.get("name"): x for x in h.get("jobs", [])}
    j.check("ingest.moves" in names and names["ingest.moves"].get("critical") is True, "the critical ingest.moves job is listed and critical")
    j.check(h.get("version") is not None, f"it names its version ({h.get('version')})")
    return j.done()


def collar_reports():
    """The simulated herd's collars are linked, report fixes, and show as simulated."""
    j = Journey("collar-reports")
    s, herds = j.req("GET", "/api/herds")
    j.check(s == 200 and len(herds) >= 1, f"the app lists the herd ({s}, {len(herds or [])} herds)")
    head = sum(h.get("count", 0) for h in herds or [])
    deadline = time.time() + 90
    cs = []
    while time.time() < deadline:
        s, cs = j.req("GET", "/api/collars", note="waiting for fixes")
        if s == 200 and len(cs) >= head and all(c.get("last_fix") for c in cs):
            break
        time.sleep(3)
    j.check(len(cs) == head, f"one collar per head is linked ({len(cs)} collars, {head} head)")
    fixed = [c for c in cs if (c.get("last_fix") or {}).get("point")]
    j.check(len(fixed) == len(cs) and len(cs) > 0, f"every collar has reported a fix with a position ({len(fixed)}/{len(cs)})")
    j.check(all(c.get("simulated") is True for c in cs), "every collar is marked simulated")
    j.check(all(c.get("herd_id") in {h["id"] for h in herds} for c in cs), "every collar belongs to the farm's herd")
    return j.done()


def report_rate_limit():
    """One collar key may post 30 reports a minute; spelling its header another way is the same collar.

    Astra 02 F16, fixed in 136098e: before it, `bearer <key>` or `Bearer  <key>`
    counted as a new collar, so a collar (or anyone with its key) could post
    without limit."""
    j = Journey("report-rate-limit")
    with open(os.path.join(DATA, "collar-sim.json")) as f:
        key = json.load(f)["collars"][0]["key"]
    canonical = f"Bearer {key}"
    body = {"fixes": []}
    statuses = []
    for i in range(45):
        s, _ = j.req("POST", "/collar/v1/report", body, auth=canonical, note=f"canonical {i}")
        statuses.append(s)
        if s == 429:
            break
    j.check(statuses[0] == 200, f"the collar's key is accepted (first report {statuses[0]})")
    j.check(statuses[-1] == 429, f"the canonical header reaches the limit within 31 reports ({len(statuses)} sent, last {statuses[-1]})")
    for spelled in (f"bearer {key}", f"Bearer  {key}", f"BEARER {key}"):
        s, _ = j.req("POST", "/collar/v1/report", body, auth=spelled, note="respelled header")
        label = spelled.replace(key, "<key>")
        j.check(s == 429, f"{label!r} is the same collar and still limited (got {s})")
    return j.done()


JOURNEYS = {"doctor": doctor, "health": health, "collar-reports": collar_reports, "report-rate-limit": report_rate_limit}

if __name__ == "__main__":
    name = sys.argv[1] if len(sys.argv) > 1 else ""
    if name not in JOURNEYS:
        print(f"usage: drive.py {' | '.join(JOURNEYS)}", file=sys.stderr)
        sys.exit(2)
    sys.exit(JOURNEYS[name]())
