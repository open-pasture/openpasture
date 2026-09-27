// Pure pieces of coverage and fleet care (stream G): cell tones and squares, and the words the
// Herd table, the animal page and the map show.

import type { Coverage, CoverageMetric, FleetRow } from "../../api/g";
import type { HerdRow } from "../../registry";
import type { Fmt } from "../../units";

export type Tone = "good" | "fair" | "poor";

// Accuracy in metres (lower is better; open sky is 1-4 m, and past 10 m a fence margin is
// guesswork); share of fixes that came (higher is better).
export const ACCURACY_M = { fair: 5, poor: 10 } as const;
export const FIXES = { fair: 0.95, poor: 0.8 } as const;

export function tone(metric: CoverageMetric, v: number): Tone {
  if (metric === "accuracy") return v < ACCURACY_M.fair ? "good" : v < ACCURACY_M.poor ? "fair" : "poor";
  return v >= FIXES.fair ? "good" : v >= FIXES.poor ? "fair" : "poor";
}

export interface Square {
  type: "Feature";
  properties: { v: number; n: number; tone: Tone };
  geometry: { type: "Polygon"; coordinates: number[][][] };
}
export interface Squares { type: "FeatureCollection"; features: Square[] }

// One square per cell, edge to edge: each cell's centre ± half its size.
export function squares(c: Coverage): Squares {
  const [w, h] = c.size ?? [0, 0];
  const features = c.cells.map(([lon, lat, v, n]): Square => {
    const [x0, x1, y0, y1] = [lon - w / 2, lon + w / 2, lat - h / 2, lat + h / 2];
    return {
      type: "Feature",
      properties: { v, n, tone: tone(c.metric, v) },
      geometry: { type: "Polygon", coordinates: [[[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]] },
    };
  });
  return { type: "FeatureCollection", features };
}

// What a cell reads on hover: "2 m" / "7 ft", or "86%".
export function cellText(metric: CoverageMetric, v: number, f: Fmt): string {
  return metric === "accuracy" ? f.len(v) : `${Math.round(v * 100)}%`;
}

export const percent = (b: number) => `${Math.round(b * 100)}%`;

// The y range for a battery sparkline: the days' own range, never under 20 points so a few
// points of noise stay flat and a week's drain reads as a slope, kept inside 0-100 %.
export function sparkDomain(values: (number | null)[], span = 0.2): [number, number] {
  const v = values.filter((x): x is number => x !== null && Number.isFinite(x));
  if (!v.length) return [0, 1];
  const [lo, hi] = [Math.min(...v), Math.max(...v)];
  const w = Math.min(1, Math.max(hi - lo, span));
  let a = (lo + hi) / 2 - w / 2;
  a = Math.min(Math.max(a, 0), 1 - w);
  return [a, a + w];
}

// Percentage points a day, one decimal: "-2.1%/d".
export const trendText = (pct: number) => `${pct > 0 ? "+" : ""}${pct.toFixed(1)}%/d`;

export const daysText = (d: number) => `${Math.round(d)} d`;

const DAY = 86_400_000;
const localDay = (ms: number) => {
  const d = new Date(ms);
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
};

// When a fit check is due, in whole local days: "in 12 d", "today", "3 d late".
export function dueText(due: string, now: number): { text: string; late: boolean } {
  const days = Math.round((localDay(Date.parse(due)) - localDay(now)) / DAY);
  if (days > 0) return { text: `in ${days} d`, late: false };
  if (days === 0) return { text: "today", late: false };
  return { text: `${-days} d late`, late: true };
}

export const shortDay = (iso: string) => new Date(iso).toLocaleDateString(undefined, { month: "short", day: "numeric" });

// Collars of a Herd table selection, each once.
export function collarIds(rows: HerdRow[]): string[] {
  return [...new Set(rows.map((r) => r.collar?.id).filter((id): id is string => !!id))];
}

// Sorts for the Herd table columns: rows without fleet data last.
export function byDaysLeft(rows: Record<string, FleetRow>) {
  return (a: HerdRow, b: HerdRow) => key(rows, a, (r) => r.days_left) - key(rows, b, (r) => r.days_left);
}
export function byFitDue(rows: Record<string, FleetRow>) {
  return (a: HerdRow, b: HerdRow) => key(rows, a, (r) => Date.parse(r.fit_due_at)) - key(rows, b, (r) => Date.parse(r.fit_due_at));
}
function key(rows: Record<string, FleetRow>, r: HerdRow, f: (x: FleetRow) => number | undefined) {
  const x = r.collar && rows[r.collar.id];
  const v = x ? f(x) : undefined;
  return v === undefined || !Number.isFinite(v) ? Number.MAX_SAFE_INTEGER : v;
}
