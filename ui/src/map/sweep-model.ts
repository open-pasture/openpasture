import type { LonLat } from "../api";

// The pure parts of the sweep view (map/sweep.ts): local metres, the rear
// edge a step's back line lights, and the chevrons that drift toward the target.

export type Ring = LonLat[];
export type XY = [number, number];

// Local metres (equirectangular) around a latitude.
const K = 111320;
export const xy = (p: LonLat, lat0: number): XY => [p[0] * K * Math.cos((lat0 * Math.PI) / 180), p[1] * K];
export const ll = (q: XY, lat0: number): LonLat => [q[0] / (K * Math.cos((lat0 * Math.PI) / 180)), q[1] / K];
export const dot = (a: XY, b: XY) => a[0] * b[0] + a[1] * b[1];

export function signedArea(r: Ring) {
  let a = 0;
  for (let i = 0; i < r.length - 1; i++) a += r[i][0] * r[i + 1][1] - r[i + 1][0] * r[i][1];
  return a / 2;
}

// Indices of the ring points on its rear edge: the longest run of edges whose
// outward side faces back along `axis` (more than 60° from sideways).
export function rearRun(pts: XY[], axis: XY): number[] {
  const n = pts.length;
  if (n < 3) return [];
  const ccw = signedArea(pts as Ring) > 0;
  const back = pts.map((a, i) => {
    const b = pts[(i + 1) % n];
    const len = Math.hypot(b[0] - a[0], b[1] - a[1]);
    if (len === 0) return false;
    const out: XY = ccw ? [(b[1] - a[1]) / len, -(b[0] - a[0]) / len] : [-(b[1] - a[1]) / len, (b[0] - a[0]) / len];
    return dot(out, axis) < -0.5;
  });
  if (back.every(Boolean)) return pts.map((_, i) => i);
  let best: [number, number] = [0, 0];
  for (let i = 0; i < n; i++) {
    if (!back[i] || back[(i + n - 1) % n]) continue;
    let k = 0;
    while (back[(i + k) % n]) k++;
    if (k > best[1]) best = [i, k];
  }
  // A run of k edges joins k + 1 points.
  return best[1] ? Array.from({ length: best[1] + 1 }, (_, j) => (best[0] + j) % n) : [];
}

// The chevrons at `now` along a run from the back line's middle (local metres
// around `lat0`) toward the target: two to five, a new one every 4.2 s.
export function chevrons(run: { from: XY; d: XY; lat0: number } | undefined, now: number): GeoJSON.Feature[] {
  if (!run) return [];
  const { from, d, lat0 } = run;
  const span = Math.hypot(d[0], d[1]);
  if (span <= 12) return [];
  const rot = (Math.atan2(d[0], d[1]) * 180) / Math.PI;
  const count = Math.max(2, Math.min(5, Math.round(span / 18)));
  const phase = (now / 4200) % 1;
  const feats: GeoJSON.Feature[] = [];
  for (let k = 0; k < count; k++) {
    const f = (k + phase) / count;
    const along = 0.12 + f * 0.7; // stay clear of the back line and the target's middle
    const p: XY = [from[0] + d[0] * along, from[1] + d[1] * along];
    feats.push({
      type: "Feature",
      properties: { rot, o: 0.85 * Math.sin(Math.PI * f) },
      geometry: { type: "Point", coordinates: ll(p, lat0) },
    });
  }
  return feats;
}

