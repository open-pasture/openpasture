#!/usr/bin/env python3
"""The soak's farm day (dev tool, scripts/soak.sh).

Cows (P1) are moved every `--move-every` minutes between the thirds of P1
(east, middle, west, middle, east, ...), about 90 m each time, and every
sweep is recorded: minutes, steps, stragglers, escapes, how it ended.
Heifers (P3) run a strip schedule over eight bands of P3 with a back fence;
every `--advance-every` minutes the next strip opens (move-now), and the back
fence behind it strands whichever heifers are still on the old strip:
those are the escapes the soak walks back with pens every hour. Samples the
server's CPU and memory and the data dir's size every 30 s.

Writes JSON lines to <dir>/driver.jsonl and samples to <dir>/samples.csv.
"""

import argparse
import json
import os
import subprocess
import time

import farm

THIRDS = {"west": (farm.P1_W, farm.P1_W / 3 * 2), "middle": (farm.P1_W / 3 * 2, farm.P1_W / 3), "east": (farm.P1_W / 3, farm.P1_E)}
ORDER = ["east", "middle", "west", "middle"]
BANDS = 8


def band(i):
    w = (farm.P1_E - farm.P1_W) / BANDS
    return farm.rect(farm.P1_W + i * w, farm.P3_S + 0.5, farm.P1_W + (i + 1) * w, farm.P3_N - 0.5)


def cpu_seconds(pid):
    out = subprocess.run(["ps", "-o", "time=,rss=", "-p", str(pid)], capture_output=True, text=True).stdout.split()
    if len(out) < 2:
        return None, None
    parts = [float(x) for x in out[0].replace("-", ":").split(":")]
    secs = 0.0
    for p in parts:
        secs = secs * 60 + p
    return secs, int(out[1]) // 1024


def du(path):
    total = 0
    for root, _, files in os.walk(path):
        for f in files:
            try:
                total += os.path.getsize(os.path.join(root, f))
            except OSError:
                pass
    return total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--api", required=True)
    ap.add_argument("--dir", required=True)
    ap.add_argument("--server-pid", type=int, required=True)
    ap.add_argument("--hours", type=float, default=6.0)
    ap.add_argument("--move-every", type=float, default=30.0)
    ap.add_argument("--advance-every", type=float, default=60.0)
    a = ap.parse_args()
    api = farm.Api(a.api)
    herds = {h["name"]: h["id"] for h in api.get("/api/herds")}
    cows, heifers = herds["Cows"], herds["Heifers"]
    log = open(os.path.join(a.dir, "driver.jsonl"), "a")
    samples = open(os.path.join(a.dir, "samples.csv"), "a")
    samples.write("t,elapsed_s,cpu_pct,rss_mb,db_bytes,wal_bytes,telemetry_bytes\n")

    def say(kind, **kw):
        kw.update({"t": round(time.time(), 1), "kind": kind})
        log.write(json.dumps(kw) + "\n")
        log.flush()
        print(json.dumps(kw), flush=True)

    start = time.time()
    end = start + a.hours * 3600
    # Heifers onto the two west bands of P3 first, once their collars have
    # said where they are (so it is a sweep); the schedule takes them on from there.
    while time.time() - start < 600 and len(farm.positions(api, heifers)) < 12:
        time.sleep(5)
    api.post(f"/api/herds/{heifers}/boundary", {"geometry": farm.rect(farm.P1_W + 0.5, farm.P3_S + 0.5, farm.P1_W + 2 * (farm.P1_E - farm.P1_W) / BANDS, farm.P3_N - 0.5)})
    say("heifers_fenced")
    schedule = None
    next_move = start + min(5.0, a.move_every) * 60
    next_advance = start + min(25.0, a.advance_every) * 60
    moves, sweep = 0, None
    last_cpu, last_t = cpu_seconds(a.server_pid)[0], time.time()
    next_sample = time.time()
    while time.time() < end:
        now = time.time()
        # The running sweep: record how it ends.
        if sweep:
            try:
                s = farm.status(api, cows)
                mv = s.get("move") or {}
                for e in s.get("escapes") or []:
                    sweep["escapes"].add(e["id"])
                if mv.get("id") == sweep["move"] and mv.get("status") != "sweeping":
                    say("sweep", target=sweep["target"], status=mv.get("status"), minutes=round((now - sweep["t0"]) / 60, 2), steps=mv.get("step"),
                        first_remaining_m=sweep["first"], stragglers=len(mv.get("stragglers") or []), escapes=len(sweep["escapes"]), preview_min=sweep["preview"])
                    sweep = None
                elif sweep["first"] is None and mv.get("id") == sweep["move"]:
                    sweep["first"] = mv.get("remaining_m")
            except Exception as e:  # the server is the thing under test; note and go on
                say("error", what="status", error=str(e))
        if now >= next_move:
            third = ORDER[moves % len(ORDER)]
            x0, x1 = THIRDS[third]
            target = farm.rect(x0 + 0.5, 0.5, x1 - 0.5, farm.P1_N - 0.5)
            try:
                if sweep:
                    s = farm.status(api, cows).get("move") or {}
                    say("sweep", target=sweep["target"], status="replaced", minutes=round((now - sweep["t0"]) / 60, 2), steps=s.get("step"),
                        first_remaining_m=sweep["first"], remaining_m=s.get("remaining_m"), stragglers=len(s.get("stragglers") or []), escapes=len(sweep["escapes"]), preview_min=sweep["preview"])
                check = api.post(f"/api/herds/{cows}/check", {"geometry": target, "sweep": True})
                m = api.post(f"/api/herds/{cows}/boundary", {"geometry": target})
                sweep = {"move": m["id"], "target": third, "t0": now, "first": None, "escapes": set(), "preview": (check.get("sweep") or {}).get("minutes")}
                say("move", target=third, status=m.get("status"), preview_min=sweep["preview"])
            except Exception as e:
                say("error", what="move", error=str(e))
            moves += 1
            next_move += a.move_every * 60
        if now >= next_advance:
            try:
                if schedule is None:
                    strips = [band(i) for i in range(BANDS)]
                    schedule = api.post("/api/schedules", {
                        "herd_id": heifers, "strips": strips, "cadence": {"every_days": 1, "at": "05:00"},
                        "back_fence": {"enabled": True, "lag_strips": 0, "close_after_min": 10, "close_steps": 2, "close_every_min": 2},
                    })
                    say("schedule", id=schedule["id"], next_index=schedule.get("next_index"))
                s = api.post(f"/api/schedules/{schedule['id']}/move-now")
                say("advance", next_index=s.get("next_index"), status=s.get("status"))
            except Exception as e:
                say("error", what="advance", error=str(e))
            next_advance += a.advance_every * 60
        if now >= next_sample:
            cpu, rss = cpu_seconds(a.server_pid)
            if cpu is None:
                say("error", what="server gone")
                break
            pct = (cpu - last_cpu) * 100.0 / max(now - last_t, 1e-6)
            last_cpu, last_t = cpu, now
            data = os.path.join(a.dir, "data")
            db = os.path.join(data, "openpasture.db")
            size = lambda p: os.path.getsize(p) if os.path.exists(p) else 0
            samples.write(f"{round(now, 1)},{round(now - start, 1)},{pct:.1f},{rss},{size(db)},{size(db + '-wal')},{du(os.path.join(data, 'telemetry'))}\n")
            samples.flush()
            next_sample += 30
        time.sleep(2)
    say("end", moves=moves)


if __name__ == "__main__":
    main()
