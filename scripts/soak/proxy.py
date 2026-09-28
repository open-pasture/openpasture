#!/usr/bin/env python3
"""Counting HTTP/1.1 reverse proxy for scripts/soak.sh (dev tool).

Collars reach the server through it (the farm's public_url points here before
they are linked), so it sees every byte they send and get: request line,
headers and body up, status line, headers and body down. Per request kind
(report, boundary download with and without a boundary, ack, the config
inside report replies) and per collar key it counts requests, bytes and the
server's latency (last request byte forwarded to last response byte back).
Every `--every` seconds it appends one JSON line of totals and that
interval's latencies to `--out`.

Usage: proxy.py --listen 17152 --upstream 127.0.0.1:17150 --out stats.jsonl
"""

import argparse
import asyncio
import hashlib
import json
import sys
import time

KINDS = ("report", "boundary_200", "boundary_204", "ack", "other")


def kind_of(method, path, status):
    p = path.split("?", 1)[0]
    if p.endswith("/collar/v1/report"):
        return "report"
    if p.endswith("/collar/v1/boundary"):
        return "boundary_200" if status == 200 else "boundary_204"
    if p.endswith("/collar/v1/ack"):
        return "ack"
    return "other"


class Stats:
    def __init__(self):
        self.t0 = time.time()
        self.by_kind = {k: {"n": 0, "up": 0, "down": 0} for k in KINDS}
        self.config = {"n": 0, "bytes": 0}
        self.status = {}
        self.per_collar = {}
        self.lat = {k: [] for k in KINDS}
        self.errors = 0

    def add(self, kind, key, up, down, status, ms, config_bytes):
        k = self.by_kind[kind]
        k["n"] += 1
        k["up"] += up
        k["down"] += down
        self.status[str(status)] = self.status.get(str(status), 0) + 1
        if key:
            c = self.per_collar.setdefault(key, {"n": 0, "bytes": 0})
            c["n"] += 1
            c["bytes"] += up + down
        self.lat[kind].append(ms)
        if config_bytes:
            self.config["n"] += 1
            self.config["bytes"] += config_bytes

    def snapshot(self):
        lat = {k: v for k, v in self.lat.items() if v}
        self.lat = {k: [] for k in KINDS}
        per = sorted(c["bytes"] for c in self.per_collar.values())
        return {
            "t": round(time.time(), 3),
            "elapsed_s": round(time.time() - self.t0, 1),
            "by_kind": self.by_kind,
            "config": self.config,
            "status": self.status,
            "collars": len(self.per_collar),
            "collar_bytes_min_median_max": [per[0], per[len(per) // 2], per[-1]] if per else None,
            "errors": self.errors,
            "latency_ms": lat,
        }


async def read_head(reader):
    data = await reader.readuntil(b"\r\n\r\n")
    lines = data.decode("latin-1").split("\r\n")
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    return data, lines[0], headers


async def read_body(reader, headers, no_body):
    if no_body:
        return b""
    if headers.get("transfer-encoding", "").lower() == "chunked":
        out = b""
        while True:
            size_line = await reader.readuntil(b"\r\n")
            out += size_line
            size = int(size_line.split(b";")[0].strip(), 16)
            chunk = await reader.readexactly(size + 2)
            out += chunk
            if size == 0:
                return out
    n = int(headers.get("content-length", "0") or 0)
    return await reader.readexactly(n) if n else b""


async def handle(creader, cwriter, upstream, stats):
    host, port = upstream
    try:
        ureader, uwriter = await asyncio.open_connection(host, port)
    except OSError:
        stats.errors += 1
        cwriter.close()
        return
    try:
        while True:
            try:
                head, reqline, headers = await read_head(creader)
            except (asyncio.IncompleteReadError, ConnectionError):
                break
            method, path = reqline.split(" ")[:2]
            body = await read_body(creader, headers, method in ("GET", "HEAD"))
            auth = headers.get("authorization", "")
            key = hashlib.sha256(auth.encode()).hexdigest()[:16] if auth else None
            start = time.perf_counter()
            uwriter.write(head + body)
            await uwriter.drain()
            rhead, statusline, rheaders = await read_head(ureader)
            status = int(statusline.split(" ")[1])
            rbody = await read_body(ureader, rheaders, method == "HEAD" or status in (204, 304) or 100 <= status < 200)
            ms = (time.perf_counter() - start) * 1000.0
            cwriter.write(rhead + rbody)
            await cwriter.drain()
            kind = kind_of(method, path, status)
            config_bytes = 0
            if kind == "report" and rbody:
                try:
                    cfg = json.loads(rbody).get("config")
                    if cfg:
                        config_bytes = len(json.dumps(cfg, separators=(",", ":")))
                except ValueError:
                    pass
            stats.add(kind, key, len(head) + len(body), len(rhead) + len(rbody), status, round(ms, 2), config_bytes)
            if rheaders.get("connection", "").lower() == "close":
                break
    except (asyncio.IncompleteReadError, ConnectionError):
        stats.errors += 1
    finally:
        for w in (cwriter, uwriter):
            try:
                w.close()
            except Exception:
                pass


async def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=int, required=True)
    ap.add_argument("--upstream", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--every", type=float, default=60.0)
    a = ap.parse_args()
    host, port = a.upstream.rsplit(":", 1)
    stats = Stats()
    server = await asyncio.start_server(lambda r, w: handle(r, w, (host, int(port)), stats), "127.0.0.1", a.listen, limit=1 << 20)

    async def dump():
        with open(a.out, "a") as f:
            while True:
                await asyncio.sleep(a.every)
                f.write(json.dumps(stats.snapshot()) + "\n")
                f.flush()

    print(f"proxy :{a.listen} -> {a.upstream}", flush=True)
    async with server:
        await asyncio.gather(server.serve_forever(), dump())


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        sys.exit(0)
