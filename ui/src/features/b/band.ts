// The warn band's geometry: an inner line `m` metres inside the drawn edge, drawn with MapLibre's
// line-offset (pixels), so it needs the pixels per metre at every zoom.

import type { Polygon } from "../../api";

// Metres across the world at the equator, and MapLibre's 512 px world at zoom 0.
const EARTH_M = 40_075_016.686;
const WORLD_PX = 512;

// Outer ring counter-clockwise, so the band's inward side is the line's left.
export function ccw(g: Polygon): [number, number][] {
  const r = g.coordinates[0];
  let a = 0;
  for (let i = 0; i < r.length - 1; i++) a += r[i][0] * r[i + 1][1] - r[i + 1][0] * r[i][1];
  return a < 0 ? r.slice().reverse() : r;
}

// line-offset in pixels for `m` metres at latitude `lat`, at every zoom: px = m / (metres per px),
// which doubles with each zoom level. Negative: to the left of a counter-clockwise ring, inside it.
export function offsetExpr(m: number, lat: number) {
  const k0 = (m * WORLD_PX) / (EARTH_M * Math.cos((lat * Math.PI) / 180));
  return ["interpolate", ["exponential", 2], ["zoom"], 0, -k0, 24, -k0 * 2 ** 24];
}

