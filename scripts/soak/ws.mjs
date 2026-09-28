// Counts /api/live frames for scripts/soak.sh (dev tool): one JSON line a
// minute with that minute's frames per second (mean, median, max), the
// message types inside them (a `batch` frame counts each event it carries),
// bytes, and resyncs; reconnects when the socket drops.
// Usage: node ws.mjs ws://127.0.0.1:17150/api/live out.jsonl [seconds]
import { appendFileSync } from "node:fs";

const [url, out, secs] = process.argv.slice(2);
const until = secs ? Date.now() + Number(secs) * 1000 : Infinity;
let perSec = new Map();
let types = {};
let bytes = 0;
let reconnects = 0;

function connect() {
  const ws = new WebSocket(url);
  ws.onmessage = (m) => {
    const s = Math.floor(Date.now() / 1000);
    perSec.set(s, (perSec.get(s) ?? 0) + 1);
    const text = typeof m.data === "string" ? m.data : "";
    bytes += text.length;
    try {
      const msg = JSON.parse(text);
      const inner = msg.type === "batch" ? msg.events : [msg];
      for (const e of inner) types[e.type] = (types[e.type] ?? 0) + 1;
    } catch {}
  };
  ws.onclose = () => {
    if (Date.now() < until) {
      reconnects += 1;
      setTimeout(connect, 1000);
    }
  };
  ws.onerror = () => {};
}
connect();

setInterval(() => {
  const now = Math.floor(Date.now() / 1000);
  const counts = [];
  for (let s = now - 60; s < now; s++) counts.push(perSec.get(s) ?? 0);
  for (const s of perSec.keys()) if (s < now - 60) perSec.delete(s);
  const sorted = [...counts].sort((a, b) => a - b);
  const line = {
    t: now,
    frames: counts.reduce((a, b) => a + b, 0),
    mean_per_s: counts.reduce((a, b) => a + b, 0) / 60,
    median_per_s: sorted[30],
    max_per_s: sorted[59],
    types,
    bytes,
    reconnects,
  };
  appendFileSync(out, JSON.stringify(line) + "\n");
  types = {};
  bytes = 0;
  if (Date.now() >= until) process.exit(0);
}, 60_000);
