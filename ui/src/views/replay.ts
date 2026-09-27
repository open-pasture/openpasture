// Where each animal is at a moment of a replay (pure, tested on its own).

import type { LonLat, Track } from "../api";

type Pt = [number, number, number]; // lon, lat, t (unix seconds), sorted by t

// The track's point at time `t`: the first at or after it, else the last. Binary search, so
// scrubbing 250 tracks of 300 points each is 250 × 9 steps, not a scan of every point.
export function pointAt(points: readonly Pt[], t: number): Pt | undefined {
  if (!points.length) return undefined;
  let lo = 0;
  let hi = points.length - 1;
  if (points[hi][2] < t) return points[hi];
  while (lo < hi) {
    const mid = (lo + hi) >> 1;
    if (points[mid][2] >= t) hi = mid;
    else lo = mid + 1;
  }
  return points[lo];
}

// Every track's animal at time `t` (unix seconds).
export function positionsAt(tracks: readonly Track[], t: number): { id: string; point: LonLat; state: "inside" }[] {
  const out: { id: string; point: LonLat; state: "inside" }[] = [];
  for (const tr of tracks) {
    const p = pointAt(tr.points, t);
    if (p) out.push({ id: tr.collar_id, point: [p[0], p[1]], state: "inside" });
  }
  return out;
}
