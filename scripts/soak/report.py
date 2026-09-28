#!/usr/bin/env python3
"""Summarise a soak run (dev tool, scripts/soak.sh) as markdown.

Reads <dir>/proxy.jsonl, ws.jsonl, samples.csv, driver.jsonl, hook.jsonl,
ui/ui.jsonl, server.log and sim.log. The data budget scales the proxy's
bytes to a day per collar and adds an estimate of what TLS and TCP/IP would
add on a real collar's link (the assumption is printed with the table).
"""

import csv
import json
import os
import re
import statistics
import sys

# Per request on a collar's LTE-M link, over the bytes of the HTTP request
# and response: TLS 1.3 resumed with a session ticket on a new connection
# (the modem sleeps between reports), about 700 B of handshake (ClientHello
# with the ticket ~400 B, ServerHello and Finished ~250 B, the client's
# Finished ~50 B); 22 B a record for the request and the response; TCP/IP
# about 10 packets of 40 B (SYN, SYN-ACK, ACKs, FIN) and one full handshake
# (~5 KB with the certificate chain) a day. No MQTT, no HTTP/2.
TLS_PER_REQUEST = 700 + 2 * 22 + 10 * 40
TLS_FULL_PER_DAY = 5000


def lines(path):
    if not os.path.exists(path):
        return []
    out = []
    for l in open(path):
        l = l.strip()
        if l:
            try:
                out.append(json.loads(l))
            except ValueError:
                pass
    return out


def pct(v, q):
    if not v:
        return None
    v = sorted(v)
    return v[min(len(v) - 1, int(q * len(v)))]


def fmt(v, nd=1):
    return "-" if v is None else (f"{v:.{nd}f}" if isinstance(v, float) else str(v))


def main():
    d = sys.argv[1]
    proxy = lines(os.path.join(d, "proxy.jsonl"))
    ws = lines(os.path.join(d, "ws.jsonl"))
    drv = lines(os.path.join(d, "driver.jsonl"))
    hooks = lines(os.path.join(d, "hook.jsonl"))
    ui = lines(os.path.join(os.path.join(d, "ui"), "ui.jsonl"))
    samples = list(csv.DictReader(open(os.path.join(d, "samples.csv")))) if os.path.exists(os.path.join(d, "samples.csv")) else []
    log = open(os.path.join(d, "server.log"), errors="replace").read() if os.path.exists(os.path.join(d, "server.log")) else ""
    sim = open(os.path.join(d, "sim.log"), errors="replace").read() if os.path.exists(os.path.join(d, "sim.log")) else ""
    strip = lambda s: re.sub(r"\x1b\[[0-9;]*m", "", s)
    log = strip(log)

    print(f"# Soak {os.path.basename(d.rstrip('/'))}\n")
    hours = (proxy[-1]["elapsed_s"] / 3600) if proxy else 0
    last = proxy[-1] if proxy else None
    collars = last["collars"] if last else 0
    print(f"{hours:.2f} h, {collars} collars through the proxy.\n")

    # Latency
    lat = {}
    for p in proxy:
        for k, v in p.get("latency_ms", {}).items():
            lat.setdefault(k, []).extend(v)
    print("## Server latency (proxy, ms)\n")
    print("| request | n | p50 | p95 | p99 | max |")
    print("| --- | --- | --- | --- | --- | --- |")
    for k in ("report", "boundary_200", "boundary_204", "ack"):
        v = lat.get(k, [])
        print(f"| {k} | {len(v)} | {fmt(pct(v, 0.5), 2)} | {fmt(pct(v, 0.95), 2)} | {fmt(pct(v, 0.99), 2)} | {fmt(max(v) if v else None, 1)} |")
    hourly = []
    for p in proxy:
        h = int(p["elapsed_s"] // 3600)
        while len(hourly) <= h:
            hourly.append([])
        hourly[h].extend(p.get("latency_ms", {}).get("report", []))
    print("\nReport p95 by hour: " + ", ".join(f"h{i + 1} {fmt(pct(v, 0.95), 1)}" for i, v in enumerate(hourly) if v) + "\n")

    # Errors
    status = last["status"] if last else {}
    fivexx = sum(n for s, n in status.items() if s.startswith("5"))
    locked = len(re.findall(r"database is locked", log))
    warns = re.findall(r"^\S+\s+WARN (\S+?):? (.*)$", log, re.M)
    errors = re.findall(r"^\S+\s+ERROR (.*)$", log, re.M)
    panics = len(re.findall(r"panicked", log))
    sim_refused = len(re.findall(r"refused", sim))
    print("## Errors\n")
    print(f"- HTTP status through the proxy: {json.dumps(status)}; 5xx: **{fivexx}**; proxy errors: {last['errors'] if last else '-'}")
    print(f"- `database is locked`: **{locked}**; server ERROR lines: {len(errors)}; WARN lines: {len(warns)}; panics: {panics}")
    if warns:
        kinds = {}
        for target, msg in warns:
            key = target + ": " + re.sub(r"\d[\d.,:_TZ-]*", "#", msg)[:90]
            kinds[key] = kinds.get(key, 0) + 1
        for k, n in sorted(kinds.items(), key=lambda x: -x[1])[:8]:
            print(f"  - {n} × {k}")
    print(f"- collar-sim refusals: {sim_refused}\n")

    # Live socket
    if ws:
        med = [w["median_per_s"] for w in ws]
        mean = [w["mean_per_s"] for w in ws]
        print("## /api/live\n")
        print(f"- {len(ws)} minutes; frames a second: median of the minutes' medians **{statistics.median(med)}**, mean {statistics.mean(mean):.2f}, worst minute's max {max(w['max_per_s'] for w in ws)}; reconnects {ws[-1]['reconnects']}\n")

    # CPU / memory / disk
    if samples:
        rows = [{k: float(v) if v not in ("", None) else None for k, v in r.items()} for r in samples]
        after_h1 = [r for r in rows if r["elapsed_s"] >= 3600]
        rss_h1 = after_h1[0]["rss_mb"] if after_h1 else None
        rss_end = rows[-1]["rss_mb"]
        cpu = [r["cpu_pct"] for r in rows[1:]]
        print("## Server process and data dir\n")
        print(f"- CPU: mean {statistics.mean(cpu):.1f} %, p95 {pct(cpu, 0.95):.1f} %, max {max(cpu):.1f} % (of one core)")
        print(f"- RSS: start {rows[0]['rss_mb']:.0f} MB, at 1 h {fmt(rss_h1, 0)} MB, end {rss_end:.0f} MB, max {max(r['rss_mb'] for r in rows):.0f} MB")

        # One sample swings by a fifth either way (the allocator; on macOS RSS
        # also drops when the system compresses pages), so hours are compared
        # by their medians: hour 2 against the last full hour.
        def by_hour(key, label):
            hours = {}
            for r in rows:
                if r.get(key) is not None:
                    hours.setdefault(int(r["elapsed_s"] // 3600), []).append(r[key])
            if not hours:
                return
            med = {h: statistics.median(v) for h, v in sorted(hours.items())}
            full = [h for h in med if h >= 1 and len(hours[h]) >= 60]
            growth = f"{(med[full[-1]] - med[full[0]]) * 100 / med[full[0]]:+.1f} %" if len(full) >= 2 else "-"
            print(f"- {label} median by hour (MB): {', '.join(f'h{h + 1} {m:.0f}' for h, m in med.items())}; growth from hour 2 to the last full hour: **{growth}**")

        by_hour("rss_mb", "RSS")
        by_hour("footprint_mb", "Memory footprint (macOS phys_footprint, counts compressed pages)")
        print(f"- DB {rows[-1]['db_bytes'] / 1e6:.0f} MB, WAL {rows[-1]['wal_bytes'] / 1e6:.1f} MB (max {max(r['wal_bytes'] for r in rows) / 1e6:.1f} MB), Parquet {rows[-1]['telemetry_bytes'] / 1e6:.1f} MB\n")

    # Data budget
    if last and collars:
        scale = 86400 / last["elapsed_s"] / collars
        print("## Data budget per collar per day (proxy bytes scaled to 24 h)\n")
        print("| kind | requests | up B | down B | total kB |")
        print("| --- | --- | --- | --- | --- |")
        total = 0
        nreq = 0
        for k in ("report", "boundary_200", "boundary_204", "ack"):
            v = last["by_kind"][k]
            t = (v["up"] + v["down"]) * scale
            total += t
            nreq += v["n"] * scale
            print(f"| {k} | {v['n'] * scale:.0f} | {v['up'] * scale:.0f} | {v['down'] * scale:.0f} | {t / 1000:.1f} |")
        cfg = last["config"]
        print(f"| config (inside report replies) | {cfg['n'] * scale:.0f} | | {cfg['bytes'] * scale:.0f} | {cfg['bytes'] * scale / 1000:.1f} |")
        tls = nreq * TLS_PER_REQUEST + TLS_FULL_PER_DAY
        print(f"| TLS + TCP/IP estimate | | | | {tls / 1000:.1f} |")
        print(f"\n**{total / 1e6:.3f} MB of HTTP a collar a day, {(total + tls) / 1e6:.3f} MB with the TLS/TCP estimate** "
              f"({TLS_PER_REQUEST} B a request for a resumed TLS 1.3 connection with TCP/IP, one {TLS_FULL_PER_DAY} B full handshake a day).")
        per = last.get("collar_bytes_min_median_max")
        if per:
            print(f"Per collar over the run: min {per[0] * 86400 / last['elapsed_s'] / 1e6:.3f}, median {per[1] * 86400 / last['elapsed_s'] / 1e6:.3f}, max {per[2] * 86400 / last['elapsed_s'] / 1e6:.3f} MB a day (HTTP).\n")

    # Sweeps and the farm day
    sweeps = [x for x in drv if x["kind"] == "sweep"]
    if sweeps:
        print("## Sweeps (Cows, 238)\n")
        print("| # | to | ended | minutes | steps | back line at step 1 (m) | left when it ended (m) | m a minute | stragglers | escapes | preview (min) |")
        print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
        for i, s in enumerate(sweeps, 1):
            first, left = s.get("first_remaining_m"), s.get("remaining_m", 0.0 if s["status"] == "done" else None)
            pace = (first - (left or 0.0)) / s["minutes"] if first is not None and s.get("minutes") else None
            print(f"| {i} | {s['target']} | {s['status']} | {s['minutes']} | {s.get('steps')} | {fmt(first)} | {fmt(left)} | {fmt(pace)} | {s['stragglers']} | {s['escapes']} | {fmt(s.get('preview_min'))} |")
        done = [s for s in sweeps if s["status"] == "done" and s.get("preview_min")]
        if done:
            err = [(s["preview_min"] - s["minutes"]) * 100 / s["minutes"] for s in done]
            print(f"\nPreview against finished sweeps: median {statistics.median(err):+.0f} %, from {min(err):+.0f} % to {max(err):+.0f} % ({len(done)} sweeps). A sweep still running at the next move is replaced by it (the 30 min cadence is shorter than a sweep of 120 m or more).")
        print()
    adv = [x for x in drv if x["kind"] in ("schedule", "advance")]
    errs = [x for x in drv if x["kind"] == "error"]
    print(f"Heifers' schedule: {len(adv)} events ({', '.join(str(x.get('next_index')) for x in adv)}); driver errors: {len(errs)}")
    for e in errs[:5]:
        print(f"- {e}")
    kinds = {}
    for h in hooks:
        b = h.get("body")
        k = (b.get("alert") or {}).get("kind") if isinstance(b, dict) else None
        k = k or (b.get("kind") if isinstance(b, dict) else None) or "?"
        kinds[k] = kinds.get(k, 0) + 1
    print(f"\nWebhook deliveries: {len(hooks)} {json.dumps(kinds)}")
    escapes = len(re.findall(r"escape: own boundary sent", log))
    print(f"Escapes started (server log): {escapes}\n")
    # The planner's time on each sweep step (the `move` log lines), by herd size.
    plans = {}
    for m in re.finditer(r"tracked\S*=\S*?(\d+).*?plan_ms\S*=\S*?\"([\d.]+)\"", log):
        plans.setdefault(int(m.group(1)), []).append(float(m.group(2)))
    for n, v in sorted(plans.items(), key=lambda x: -x[0])[:3]:
        print(f"Planner, {n} animals tracked: {len(v)} steps, p50 {pct(v, 0.5):.1f} ms, p95 {pct(v, 0.95):.1f} ms, max {max(v):.1f} ms")
    if plans:
        print()
    if ui:
        print(f"UI: {len(ui)} checks, {sum(len(u['errors']) for u in ui)} console errors, {sum(len(u['failed']) for u in ui)} failed requests, "
              f"page requests p95 {fmt(pct([u['ms_p95'] for u in ui if u['ms_p95'] is not None], 0.5))} ms (median of the checks), "
              f"client restarts {ui[-1].get('restarts', 0)}")
        heaps = [u["heap_mb"] for u in ui if u.get("heap_mb") is not None]
        if heaps:
            print(f"UI JS heap (MB) at each check: {', '.join(str(h) for h in heaps)}")
        for u in ui:
            for e in u["errors"]:
                print(f"  - {e[:160]}")


if __name__ == "__main__":
    main()
