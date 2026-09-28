#!/usr/bin/env python3
"""One timed sweep (dev tool, stream L): send a herd a target whose rear edge
is `--dist` metres ahead of the herd's back line and follow the move to the
end. Prints the pre-send preview, then one line per step and a summary line
as JSON (minutes to done, steps, stragglers, escapes seen).

The target is the part of `--within` (a paddock of farm.py: P1 or P3) from
x = rearmost animal - 3 m + dist to the paddock's east edge; the herd sweeps
east. Usage: sweep.py --api http://127.0.0.1:17150 --herd <id> --dist 90
"""

import argparse
import json
import sys
import time

import farm


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--api", required=True)
    ap.add_argument("--herd", required=True)
    ap.add_argument("--dist", type=float, default=90.0)
    ap.add_argument("--within", default="P1")
    ap.add_argument("--x1", type=float, default=None, help="east end of the target (default: the paddock's)")
    ap.add_argument("--max-min", type=float, default=60.0)
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    api = farm.Api(a.api)
    pos = farm.positions(api, a.herd)
    if not pos:
        sys.exit("no positions")
    rear = min(x for x, _ in pos.values())
    x0 = rear - 3.0 + a.dist
    y0, y1 = (0.0, farm.P1_N) if a.within == "P1" else (farm.P3_S, farm.P3_N)
    x1 = a.x1 if a.x1 is not None else farm.P1_E
    target = farm.rect(x0, y0 + 0.5, x1 - 0.5, y1 - 0.5)
    check = api.post(f"/api/herds/{a.herd}/check", {"geometry": target, "sweep": True})
    preview = (check.get("sweep") or {}).get("minutes")
    print(json.dumps({"collars": len(pos), "rear_x": round(rear, 1), "target_x0": round(x0, 1), "preview_min": preview}), flush=True)
    started = time.time()
    m = api.post(f"/api/herds/{a.herd}/boundary", {"geometry": target})
    escapes, last_step, rows = set(), -1, []
    first_remaining = None
    while True:
        s = farm.status(api, a.herd)
        mv = s.get("move") or {}
        for e in s.get("escapes") or []:
            escapes.add(e["id"])
        el = time.time() - started
        if mv.get("step") != last_step:
            last_step = mv.get("step")
            if first_remaining is None:
                first_remaining = mv.get("remaining_m")
            row = {"t_s": round(el, 1), "step": mv.get("step"), "remaining_m": mv.get("remaining_m"), "stragglers": len(mv.get("stragglers") or []), "status": mv.get("status")}
            rows.append(row)
            print(json.dumps(row), flush=True)
        if mv.get("status") != "sweeping" or el > a.max_min * 60:
            break
        time.sleep(2)
    end = {
        "done": mv.get("status") == "done",
        "status": mv.get("status"),
        "minutes": round((time.time() - started) / 60, 2),
        "steps": mv.get("step"),
        "first_remaining_m": first_remaining,
        "stragglers": len(mv.get("stragglers") or []),
        "escapes": len(escapes),
        "preview_min": preview,
        "move": m.get("id"),
    }
    print(json.dumps(end), flush=True)
    if a.out:
        with open(a.out, "w") as f:
            json.dump({"summary": end, "steps": rows}, f)


if __name__ == "__main__":
    main()
