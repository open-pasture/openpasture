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
