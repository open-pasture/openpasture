// Pure strip helpers for the strip tool: orientation, the handle, labels and the facts line.

import type { LonLat, Polygon } from "../../api";
import type { StripFacts } from "../../api/c";
import { centroid, inside, offset, signedDistance, toXY } from "../../geo";
import { num, type Fmt } from "../../units";

const RAD = Math.PI / 180;

// Compass bearing from a to b in degrees: 0 north, 90 east, [0, 360).
export function bearing(a: LonLat, b: LonLat): number {
  const [x1, y1] = toXY(a, a[1]);
  const [x2, y2] = toXY(b, a[1]);
  const deg = Math.atan2(x2 - x1, y2 - y1) / RAD;
  return ((deg % 360) + 360) % 360;
}

// The point m metres from p toward bearing deg.
export function toward(p: LonLat, deg: number, m: number): LonLat {
  return offset(p, m * Math.sin(deg * RAD), m * Math.cos(deg * RAD));
}

// How deep the paddock is along bearing deg, in metres.
export function depth(poly: Polygon, deg: number): number {
  const ring = poly.coordinates[0];
  if (!ring?.length) return 0;
  const lat0 = ring[0][1];
  const s = Math.sin(deg * RAD), c = Math.cos(deg * RAD);
  let lo = Infinity, hi = -Infinity;
  for (const p of ring) {
    const [x, y] = toXY(p, lat0);
    const v = x * s + y * c;
    lo = Math.min(lo, v);
    hi = Math.max(hi, v);
  }
  return hi > lo ? hi - lo : 0;
}

// The bearing that runs down the paddock's length, 0-179: the one across which the paddock
// is narrowest, so strips run across it and the herd advances along it.
export function lengthwise(poly: Polygon): number {
  let best = 0, least = Infinity;
  for (let d = 0; d < 180; d++) {
    const across = depth(poly, d + 90);
    if (across < least - 1e-6) {
      least = across;
      best = d;
    }
  }
  return best;
}

// Whole degrees in [0, 360).
export const snap = (deg: number) => ((Math.round(deg) % 360) + 360) % 360;

// Where the orientation handle sits: just past the paddock's edge in the advance direction.
export function handleAt(poly: Polygon, deg: number): LonLat {
  return toward(centroid(poly), deg, depth(poly, deg) / 2 + 30);
}

// A point well inside a strip for its number: a fifth of the way along the strip from one
// end, so the numbers line up clear of the paddock's name and the herd in its middle. Where
// that point isn't well inside (odd pieces), the grid point farthest from every edge.
export function labelAt(strip: Polygon, deg: number): LonLat {
  const ring = strip.coordinates[0];
  const c = centroid(strip);
  const lat0 = c[1];
  const across = (deg + 90) * RAD;
  const [ux, uy] = [Math.sin(across), Math.cos(across)];
  const [cx, cy] = toXY(c, lat0);
  let lo = Infinity, hi = -Infinity;
  for (const p of ring) {
    const [x, y] = toXY(p, lat0);
    const a = (x - cx) * ux + (y - cy) * uy;
    lo = Math.min(lo, a);
    hi = Math.max(hi, a);
  }
  const at = lo + (hi - lo) * 0.2;
  const p = offset(c, at * ux, at * uy);
  if (inside(p, strip) && signedDistance(p, strip) > 3) return p;
  const xs = ring.map((q) => q[0]), ys = ring.map((q) => q[1]);
  const [w, e, s, n] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
  let best = c, far = -Infinity;
  for (let i = 1; i < 12; i++)
    for (let j = 1; j < 12; j++) {
      const q: LonLat = [w + ((e - w) * i) / 12, s + ((n - s) * j) / 12];
      const d = signedDistance(q, strip);
      if (d > far) {
        far = d;
        best = q;
      }
    }
  return best;
}

// Where the tether to the handle starts: the paddock's edge on the way out to it.
export function tetherFrom(poly: Polygon, deg: number): LonLat {
  return toward(centroid(poly), deg, depth(poly, deg) / 2);
}

// "2 d", "2.5 d".
export const days = (d: number) => `${num(d, Number.isInteger(d) ? 0 : 1)} d`;

// One strip as a mono facts line: "3.0 ac  250 hd  2 d" (days only when known).
export function facts(u: Fmt, s: StripFacts, head: number): string {
  return [u.area(s.grazeable_ha), `${num(head)} hd`, s.days !== undefined ? days(s.days) : undefined].filter(Boolean).join("  ");
}
