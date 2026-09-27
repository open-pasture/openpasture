// Pure lasso helpers: thinning a dragged path, closing it, and what it caught.

import type { LonLat } from "../../api";
import { inside, toXY } from "../../geo";

// Keep a point only when it is at least minM metres from the last one kept.
export function thin(path: LonLat[], minM: number): LonLat[] {
  const out: LonLat[] = [];
  for (const p of path) {
    const last = out[out.length - 1];
    if (last) {
      const [x1, y1] = toXY(last, last[1]);
      const [x2, y2] = toXY(p, last[1]);
      if (Math.hypot(x2 - x1, y2 - y1) < minM) continue;
    }
    out.push(p);
  }
  return out;
}

// The dragged path as a closed ring, or undefined when it encloses nothing.
export function closeRing(path: LonLat[]): LonLat[] | undefined {
  const pts = path.filter((p, i) => i === 0 || p[0] !== path[i - 1][0] || p[1] !== path[i - 1][1]);
  if (pts.length < 3) return undefined;
  const first = pts[0], last = pts[pts.length - 1];
  const ring = first[0] === last[0] && first[1] === last[1] ? pts : [...pts, first];
  return ring.length >= 4 ? ring : undefined;
}

// Ids whose point lies inside the ring, in the order given.
export function caught(ring: LonLat[], points: Iterable<readonly [string, LonLat]>): string[] {
  const poly = { type: "Polygon" as const, coordinates: [ring] };
  const out: string[] = [];
  for (const [id, p] of points) if (inside(p, poly)) out.push(id);
  return out;
}
