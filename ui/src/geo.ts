import type { LonLat, Polygon } from "./api";

const R = 6371008.8;
const rad = (d: number) => (d * Math.PI) / 180;

// Local equirectangular metres around a reference latitude. Good enough at paddock scale.
export function toXY(p: LonLat, lat0: number): [number, number] {
  return [rad(p[0]) * R * Math.cos(rad(lat0)), rad(p[1]) * R];
}
export function fromXY(x: number, y: number, lat0: number): LonLat {
  return [((x / (R * Math.cos(rad(lat0)))) * 180) / Math.PI, ((y / R) * 180) / Math.PI];
}

export function offset(p: LonLat, dx: number, dy: number): LonLat {
  const [x, y] = toXY(p, p[1]);
  return fromXY(x + dx, y + dy, p[1]);
}

export function ring(poly: Polygon): LonLat[] {
  return poly.coordinates[0];
}

// Outer ring minus its holes.
export function areaHa(poly: Polygon): number {
  const lat0 = ring(poly)[0][1];
  const [outer, ...holes] = poly.coordinates;
  return Math.max(0, holes.reduce((a, h) => a - ringArea(h, lat0), ringArea(outer, lat0))) / 10000;
}

// m², unsigned.
function ringArea(r: LonLat[], lat0: number): number {
  let a = 0;
  for (let i = 0; i < r.length - 1; i++) {
    const [x1, y1] = toXY(r[i], lat0);
    const [x2, y2] = toXY(r[i + 1], lat0);
    a += x1 * y2 - x2 * y1;
  }
  return Math.abs(a / 2);
}

export function centroid(poly: Polygon): LonLat {
  const r = ring(poly).slice(0, -1);
  const s = r.reduce((a, p) => [a[0] + p[0], a[1] + p[1]], [0, 0]);
  return [s[0] / r.length, s[1] / r.length];
}

// A point inside the shape, for its label and for "the paddock it is in": the centroid when that
// is inside, else the middle of the inside stretch, along a few lines of latitude, that lies
// farthest from every edge (a paddock with a pond in the middle has its centroid in the pond).
export function interiorPoint(poly: Polygon): LonLat {
  const c = centroid(poly);
  if (inside(c, poly)) return c;
  const ys = ring(poly).map((p) => p[1]);
  const [lo, hi] = [Math.min(...ys), Math.max(...ys)];
  let best: { d: number; at: LonLat } | undefined;
  for (let k = 1; k < 16; k++) {
    const y = lo + ((hi - lo) * k) / 16;
    const xs: number[] = [];
    for (const r of poly.coordinates)
      for (let i = 0; i < r.length - 1; i++) {
        const [[x1, y1], [x2, y2]] = [r[i], r[i + 1]];
        if (y1 > y !== y2 > y) xs.push(x1 + ((y - y1) * (x2 - x1)) / (y2 - y1));
      }
    xs.sort((a, b) => a - b);
    for (let i = 0; i + 1 < xs.length; i += 2) {
      const at: LonLat = [(xs[i] + xs[i + 1]) / 2, y];
      const d = signedDistance(at, poly);
      if (!best || d > best.d) best = { d, at };
    }
  }
  return best?.at ?? c;
}

// Inside the outer ring and in none of its holes.
export function inside(p: LonLat, poly: Polygon): boolean {
  const [outer, ...holes] = poly.coordinates;
  return inRing(p, outer) && !holes.some((h) => inRing(p, h));
}

function inRing(p: LonLat, r: LonLat[]): boolean {
  let c = false;
  for (let i = 0, j = r.length - 1; i < r.length; j = i++) {
    const [xi, yi] = r[i];
    const [xj, yj] = r[j];
    if (yi > p[1] !== yj > p[1] && p[0] < ((xj - xi) * (p[1] - yi)) / (yj - yi) + xi) c = !c;
  }
  return c;
}

// Where closed ring a lies against closed ring b: wholly inside it, wholly outside it, or cut
// by its edge (touching counts as cut).
export function ringAgainst(a: LonLat[], b: LonLat[]): "in" | "out" | "cut" {
  for (let i = 0; i < a.length - 1; i++)
    for (let j = 0; j < b.length - 1; j++) if (segmentsMeet(a[i], a[i + 1], b[j], b[j + 1])) return "cut";
  return inRing(a[0], b) ? "in" : "out";
}

const orient = (a: LonLat, b: LonLat, c: LonLat) => Math.sign((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]));
const within = (a: LonLat, b: LonLat, c: LonLat) =>
  Math.min(a[0], b[0]) <= c[0] && c[0] <= Math.max(a[0], b[0]) && Math.min(a[1], b[1]) <= c[1] && c[1] <= Math.max(a[1], b[1]);

// Whether segments pq and rs share a point.
function segmentsMeet(p: LonLat, q: LonLat, r: LonLat, s: LonLat): boolean {
  const d1 = orient(r, s, p), d2 = orient(r, s, q), d3 = orient(p, q, r), d4 = orient(p, q, s);
  if (d1 * d2 < 0 && d3 * d4 < 0) return true;
  return (d1 === 0 && within(r, s, p)) || (d2 === 0 && within(r, s, q)) || (d3 === 0 && within(p, q, r)) || (d4 === 0 && within(p, q, s));
}

// Metres from p to the nearest edge of any ring. Positive inside, negative outside (a hole is outside).
export function signedDistance(p: LonLat, poly: Polygon): number {
  const lat0 = p[1];
  const [px, py] = toXY(p, lat0);
  let best = Infinity;
  for (const r of poly.coordinates)
    for (let i = 0; i < r.length - 1; i++) {
      const [ax, ay] = toXY(r[i], lat0);
      const [bx, by] = toXY(r[i + 1], lat0);
      const dx = bx - ax, dy = by - ay;
      const t = Math.max(0, Math.min(1, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy || 1)));
      const d = Math.hypot(px - (ax + t * dx), py - (ay + t * dy));
      best = Math.min(best, d);
    }
  return inside(p, poly) ? best : -best;
}

export function bbox(polys: Polygon[]): [number, number, number, number] | null {
  let w = Infinity, s = Infinity, e = -Infinity, n = -Infinity;
  for (const p of polys)
    for (const [x, y] of ring(p)) {
      w = Math.min(w, x); s = Math.min(s, y); e = Math.max(e, x); n = Math.max(n, y);
    }
  return Number.isFinite(w) ? [w, s, e, n] : null;
}

export function rect(sw: LonLat, wM: number, hM: number): Polygon {
  const a = sw, b = offset(sw, wM, 0), c = offset(sw, wM, hM), d = offset(sw, 0, hM);
  return { type: "Polygon", coordinates: [[a, b, c, d, a]] };
}
