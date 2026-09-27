// Pure helpers for the strip schedule views: the next move, the queue by day, the stored
// line, who lacks the next open, and what the time rail shows at a time.

import type { LonLat, Polygon, SlotCount } from "../../api";
import type { CollarSlots } from "../../api/e-srv";
import type { Schedule, ScheduledMove } from "../../api/s";
import { centroid, toXY } from "../../geo";
import { atLabel, clockIn, dateIn } from "../b/when";

export const pending = (m: ScheduledMove) => m.state === "planned" || m.state === "staged";
// An open, not a held place.
export const isOpen = (m: ScheduledMove) => m.step === 0 && m.skipped !== "held";
const t = (m: ScheduledMove) => Date.parse(m.at);
const byTime = (a: ScheduledMove, b: ScheduledMove) => t(a) - t(b) || a.index - b.index || a.step - b.step;

// The next strip to open.
export function nextOpen(moves: ScheduledMove[]): ScheduledMove | undefined {
  return moves.filter((m) => pending(m) && isOpen(m)).sort(byTime)[0];
}

// The next thing the collars do: an open or a back-fence step.
export function nextMove(moves: ScheduledMove[]): ScheduledMove | undefined {
  return moves.filter(pending).sort(byTime)[0];
}

// "Wed 07:00 248/250 stored": how many collars hold the next open (stored, or already applied).
export function storedLine(m: ScheduledMove | undefined, slots: SlotCount[] | undefined, tz: string, now: number): string | undefined {
  if (m?.boundary_version === undefined) return undefined;
  const c = slots?.find((s) => s.version === m.boundary_version);
  if (!c || c.collars === 0) return undefined;
  return `${atLabel(t(m), tz, now)}  ${c.applied + c.stored}/${c.collars} stored`;
}

// Collars that should hold `version` and don't: on duty (not parked, not out on an escape)
// with no slot for it (a copy handed back after an escape counts).
export function missingCollars(collars: CollarSlots[], version: number): string[] {
  return collars
    .filter((c) => !c.parked && !c.escaped)
    .filter((c) => !c.slots.some((s) => s.status !== "rejected" && (s.version === version || s.copy_of === version)))
    .map((c) => c.collar_id);
}

export interface QueueRow {
  kind: "open" | "fence" | "held" | "skipped";
  index: number;
  at: string;
  // A back-fence row: when its last step closes.
  until?: string;
  // The next open: the one Hold and Move now act on.
  first: boolean;
}
export interface QueueDay { day: string; date: string; rows: QueueRow[] }

// What is still to come, by farm-local day: each open with its back fence folded into one
// row, and the held and skipped places ahead.
export function queue(moves: ScheduledMove[], tz: string, now: number): QueueDay[] {
  const first = nextOpen(moves);
  const rows: QueueRow[] = [];
  for (const m of moves.slice().sort(byTime)) {
    if (m.skipped === "held" || m.skipped === "skipped") {
      if (t(m) >= now && m.step === 0) rows.push({ kind: m.skipped, index: m.index, at: m.at, first: false });
      continue;
    }
    if (!pending(m)) continue;
    if (m.step === 0) {
      rows.push({ kind: "open", index: m.index, at: m.at, first: m === first });
      continue;
    }
    const last = rows[rows.length - 1];
    if (last?.kind === "fence" && last.index === m.index) last.until = m.at;
    else rows.push({ kind: "fence", index: m.index, at: m.at, until: m.at, first: false });
  }
  const days: QueueDay[] = [];
  for (const r of rows) {
    const date = dateIn(Date.parse(r.at), tz);
    let d = days[days.length - 1];
    if (!d || d.date !== date) {
      d = { date, day: weekday(Date.parse(r.at), tz), rows: [] };
      days.push(d);
    }
    d.rows.push(r);
  }
  return days;
}

// Time to go: "03:12:40" within a day, "2d 23h" beyond.
export function countdown(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s >= 86_400) return `${Math.floor(s / 86_400)}d ${Math.floor((s % 86_400) / 3600)}h`;
  const p = (n: number) => String(n).padStart(2, "0");
  return `${p(Math.floor(s / 3600))}:${p(Math.floor((s % 3600) / 60))}:${p(s % 60)}`;
}

export const weekday = (at: number, tz: string) => new Date(at).toLocaleDateString("en-US", { weekday: "short", timeZone: tz });

// "07:00" or "07:00–07:20" for a back fence, farm time.
export function rowTime(r: QueueRow, tz: string): string {
  const a = clockIn(Date.parse(r.at), tz);
  if (!r.until || r.until === r.at) return a;
  return `${a}–${clockIn(Date.parse(r.until), tz)}`;
}

// ---- the time rail ------------------------------------------------------------------------

// Rows that shape the fence at their time, in order (skipped and held ones don't).
const shaping = (moves: ScheduledMove[]) => moves.filter((m) => !m.skipped).sort(byTime);

// The scheduled boundary in effect at `at`, when a scheduled move has happened by then.
export function boundaryAt(moves: ScheduledMove[], at: number): ScheduledMove | undefined {
  let found: ScheduledMove | undefined;
  for (const m of shaping(moves)) if (t(m) <= at) found = m;
  return found;
}

// The strip open at `at` (the last open by then), if any.
export function stripAt(moves: ScheduledMove[], at: number): number | undefined {
  let k: number | undefined;
  for (const m of shaping(moves)) if (m.step === 0 && t(m) <= at) k = m.index;
  return k;
}

// Strips grazed by `at`: behind the back fence once it has closed, else before the strip
// the herd came from; without a back fence, every strip before the one open.
export function grazedAt(s: Schedule, moves: ScheduledMove[], at: number): number[] {
  const k = stripAt(moves, at);
  if (k === undefined) return range(0, Math.max(0, s.next_index - 1));
  if (!s.back_fence.enabled) return range(0, k);
  const lag = s.back_fence.lag_strips;
  const closes = shaping(moves).filter((m) => m.index === k && m.step > 0);
  const closed = closes.length === 0 || closes.every((m) => t(m) <= at);
  if (closed) return range(0, Math.max(0, k - lag));
  const prev = shaping(moves).filter((m) => m.step === 0 && m.index < k).pop()?.index ?? k - 1;
  return range(0, Math.max(0, prev - lag));
}

const range = (a: number, b: number) => Array.from({ length: Math.max(0, b - a) }, (_, i) => a + i);

// The back fence of `g`: the edges of its outer ring that face back along the strips, from
// strip k-1 toward strip k.
export function backLine(strips: Polygon[], g: Polygon, k: number): LonLat[][] {
  if (k < 1 || k >= strips.length) return [];
  const a = centroid(strips[k - 1]), b = centroid(strips[k]);
  const lat0 = a[1];
  const [ax, ay] = toXY(a, lat0), [bx, by] = toXY(b, lat0);
  const len = Math.hypot(bx - ax, by - ay);
  if (!len) return [];
  const d: [number, number] = [(bx - ax) / len, (by - ay) / len];
  const r = g.coordinates[0];
  const pts = r.map((p) => toXY(p, lat0));
  // Outward normals: the ring's winding decides the side.
  let area = 0;
  for (let i = 0; i < pts.length - 1; i++) area += pts[i][0] * pts[i + 1][1] - pts[i + 1][0] * pts[i][1];
  const ccw = area > 0;
  const out: LonLat[][] = [];
  let cur: LonLat[] | undefined;
  for (let i = 0; i < pts.length - 1; i++) {
    const [x1, y1] = pts[i], [x2, y2] = pts[i + 1];
    const l = Math.hypot(x2 - x1, y2 - y1);
    if (!l) continue;
    const n = ccw ? [(y2 - y1) / l, -(x2 - x1) / l] : [-(y2 - y1) / l, (x2 - x1) / l];
    if (n[0] * d[0] + n[1] * d[1] < -0.7) {
      if (cur && cur[cur.length - 1] === r[i]) cur.push(r[i + 1]);
      else out.push((cur = [r[i], r[i + 1]]));
    } else cur = undefined;
  }
  return out;
}

// From now to a little past the last move still to come (at least a day).
export function railSpan(moves: ScheduledMove[], now: number): [number, number] {
  const last = moves.filter(pending).reduce((m, x) => Math.max(m, t(x)), now);
  return [now, Math.max(now + 86_400_000, last + 3_600_000)];
}

// Farm-local midnights in (start, end), each labelled with the day it starts ("Wed").
export function dayTicks(start: number, end: number, tz: string): { at: number; label: string }[] {
  const out: { at: number; label: string }[] = [];
  let day = dateIn(start, tz);
  for (let i = 0; i < 400; i++) {
    const noon = Date.parse(`${day}T12:00:00Z`);
    const next = dateIn(noon + 86_400_000, tz);
    const midnight = startOfDay(next, tz);
    if (midnight >= end) break;
    if (midnight > start) out.push({ at: midnight, label: weekday(midnight + 3_600_000, tz) });
    day = next;
  }
  return out;
}

// The instant the farm's clock reads 00:00 on `date` (YYYY-MM-DD).
function startOfDay(date: string, tz: string): number {
  const [y, m, d] = date.split("-").map(Number);
  // Offsets change at most an hour or two; step to the instant whose local date is `date` at 00:00.
  let guess = Date.UTC(y, m - 1, d);
  for (let i = 0; i < 3; i++) {
    const local = new Date(guess).toLocaleString("sv-SE", { timeZone: tz, hour12: false });
    const [ld, lt] = local.split(" ");
    const [hh, mm] = lt.split(":").map(Number);
    const dayDiff = (Date.parse(`${date}T00:00:00Z`) - Date.parse(`${ld}T00:00:00Z`)) / 86_400_000;
    guess += (dayDiff * 24 * 60 - (hh * 60 + mm)) * 60_000;
  }
  return guess;
}
