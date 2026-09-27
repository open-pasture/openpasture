// Map features without the map: which are in effect, the paddock an exclusion belongs to,
// dates in farm time, the facts line, and the GeoJSON the overlay draws.

import type { FeatureKind, LonLat, MapFeature, Paddock, Polygon } from "../../api";
import { areaHa, centroid, fromXY, inside, toXY } from "../../geo";
import type { Fmt } from "../../units";
import { spec } from "./kinds";

export { KINDS, spec, upsert, withPaddocks, type KindSpec, type Shape } from "./kinds";

// A hazard point with no radius yet starts at this many metres.
export const HAZARD_RADIUS_M = 10;

// ---- in effect ---------------------------------------------------------------------------

// active_from inclusive, active_until exclusive, either absent.
export function isActive(f: MapFeature, now: number): boolean {
  return (!f.active_from || Date.parse(f.active_from) <= now) && (!f.active_until || now < Date.parse(f.active_until));
}

// The next time after `now` when a feature starts or ends, if any.
export function nextChange(list: MapFeature[], now: number): number | undefined {
  let next: number | undefined;
  for (const f of list)
    for (const iso of [f.active_from, f.active_until]) {
      const t = iso ? Date.parse(iso) : NaN;
      if (t > now && (next === undefined || t < next)) next = t;
    }
  return next;
}

// ---- scope ---------------------------------------------------------------------------------

// The paddock a drawn shape belongs to: the smallest one holding its centre, else the one
// holding most of its corners. None when it lies outside every paddock.
export function scopePaddock(g: Polygon, paddocks: Paddock[]): Paddock | undefined {
  const c = centroid(g);
  const holding = paddocks.filter((p) => inside(c, p.geometry)).sort((a, b) => a.area_ha - b.area_ha);
  if (holding.length) return holding[0];
  const corners = g.coordinates[0].slice(0, -1);
  let best: Paddock | undefined, most = 0;
  for (const p of paddocks) {
    const n = corners.filter((q) => inside(q, p.geometry)).length;
    if (n > most) [best, most] = [p, n];
  }
  return best;
}

// ---- shapes --------------------------------------------------------------------------------

// A circle of `r` metres as a polygon, for a hazard point's radius.
export function circle(c: LonLat, r: number, n = 48): Polygon {
  const [cx, cy] = toXY(c, c[1]);
  const ring: LonLat[] = [];
  for (let i = 0; i < n; i++) {
    const a = (2 * Math.PI * i) / n;
    ring.push(fromXY(cx + r * Math.cos(a), cy + r * Math.sin(a), c[1]));
  }
  ring.push(ring[0]);
  return { type: "Polygon", coordinates: [ring] };
}

// Metres along a line.
export function lineLength(pts: LonLat[]): number {
  if (pts.length < 2) return 0;
  const lat0 = pts[0][1];
  let m = 0;
  for (let i = 1; i < pts.length; i++) {
    const [ax, ay] = toXY(pts[i - 1], lat0), [bx, by] = toXY(pts[i], lat0);
    m += Math.hypot(bx - ax, by - ay);
  }
  return m;
}

export const radiusOf = (f: MapFeature): number | undefined => {
  const r = f.props?.radius_m;
  return typeof r === "number" && r > 0 ? r : undefined;
};

// ---- farm time ----------------------------------------------------------------------------

const zoneOk = (tz: string) => {
  try {
    new Intl.DateTimeFormat("en-US", { timeZone: tz });
    return tz;
  } catch {
    return "UTC";
  }
};

function parts(t: number, tz: string) {
  const f = new Intl.DateTimeFormat("en-US", {
    timeZone: zoneOk(tz), hourCycle: "h23", year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit",
  });
  const get = (type: string) => Number(f.formatToParts(new Date(t)).find((p) => p.type === type)?.value);
  return { y: get("year"), m: get("month"), d: get("day"), h: get("hour"), min: get("minute"), s: get("second") };
}

// How far the zone is ahead of UTC at instant t, in ms.
function offset(t: number, tz: string): number {
  const p = parts(t, tz);
  return Date.UTC(p.y, p.m - 1, p.d, p.h, p.min, p.s) - Math.floor(t / 1000) * 1000;
}

// The instant a wall-clock time happens in the zone.
export function zonedTime(y: number, m: number, d: number, h: number, min: number, tz: string): number {
  const wall = Date.UTC(y, m - 1, d, h, min);
  const t = wall - offset(wall, tz);
  const again = wall - offset(t, tz);
  return again === t ? t : again;
}

const pad = (n: number) => String(n).padStart(2, "0");

// "YYYY-MM-DD" in the zone.
export function localDate(t: number, tz: string): string {
  const p = parts(t, tz);
  return `${p.y}-${pad(p.m)}-${pad(p.d)}`;
}

// "until 2026-10-03" lasts through the 3rd: the start of the 4th in farm time.
export function endOfDay(date: string, tz: string): string {
  const [y, m, d] = date.split("-").map(Number);
  const next = new Date(Date.UTC(y, m - 1, d + 1));
  return new Date(zonedTime(next.getUTCFullYear(), next.getUTCMonth() + 1, next.getUTCDate(), 0, 0, tz)).toISOString();
}

// The last day an active_until covers, for the date input: the day before a farm midnight.
export function lastDay(until: string, tz: string): string {
  return localDate(Date.parse(until) - 1, tz);
}

const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

// "Oct 3", or "Oct 3 14:05" when the time isn't midnight. `through`: an end time, so a
// midnight shows the day before (the last day covered).
export function when(iso: string, tz: string, through = false): string {
  const t = Date.parse(iso);
  const p = parts(t, tz);
  if (p.h === 0 && p.min === 0) {
    const q = through ? parts(t - 1, tz) : p;
    return `${MONTHS[q.m - 1]} ${q.d}`;
  }
  return `${MONTHS[p.m - 1]} ${p.d} ${pad(p.h)}:${pad(p.min)}`;
}

// ---- words ---------------------------------------------------------------------------------

// The mono line under a feature's name: "exclusion  0.2 ha  P1  until Oct 3". `show` leaves out
// what the sheet already has a field for.
export function facts(f: MapFeature, u: Fmt, paddocks: Paddock[], tz: string, show: { radius?: boolean; until?: boolean } = {}): string {
  const out: string[] = [spec(f.kind).label.toLowerCase()];
  const g = f.geometry;
  if (g.type === "Polygon") out.push(u.area(areaHa(g)));
  if (g.type === "LineString") out.push(u.len(lineLength(g.coordinates)));
  const r = radiusOf(f);
  if (g.type === "Point" && r && show.radius !== false) out.push(`${u.len(r)} around`);
  const pad = f.paddock_id ? paddocks.find((p) => p.id === f.paddock_id) : undefined;
  if (pad) out.push(pad.name);
  if (f.active_from) out.push(`from ${when(f.active_from, tz)}`);
  if (f.active_until && show.until !== false) out.push(`until ${when(f.active_until, tz, true)}`);
  return out.join("  ");
}

// ---- what the overlay draws ------------------------------------------------------------------

export interface MapData {
  // Exclusions, water, shade and hazard areas, and each hazard point's circle.
  zones: GeoJSON.FeatureCollection;
  // Roads, neighbour lines and the farm boundary's ring.
  lines: GeoJSON.FeatureCollection;
  // One icon per water, gate, shade and hazard: at the point, or at an area's centre.
  points: GeoJSON.FeatureCollection;
}

const feat = (id: string, kind: FeatureKind, geometry: GeoJSON.Geometry): GeoJSON.Feature => ({ type: "Feature", properties: { id, kind }, geometry });

// What is in effect at `now`, as the three GeoJSON sources.
export function mapData(list: MapFeature[], now: number): MapData {
  const zones: GeoJSON.Feature[] = [], lines: GeoJSON.Feature[] = [], points: GeoJSON.Feature[] = [];
  for (const f of list) {
    if (!isActive(f, now)) continue;
    const g = f.geometry;
    if (f.kind === "farm_boundary" && g.type === "Polygon") {
      lines.push(feat(f.id, f.kind, { type: "MultiLineString", coordinates: g.coordinates }));
      continue;
    }
    if (g.type === "LineString") {
      lines.push(feat(f.id, f.kind, g));
      continue;
    }
    if (g.type === "Polygon") {
      zones.push(feat(f.id, f.kind, g));
      if (f.kind !== "exclusion") points.push(feat(f.id, f.kind, { type: "Point", coordinates: centroid(g) }));
      continue;
    }
    const r = radiusOf(f);
    if (f.kind === "hazard" && r) zones.push(feat(f.id, f.kind, circle(g.coordinates, r)));
    points.push(feat(f.id, f.kind, g));
  }
  const fc = (features: GeoJSON.Feature[]): GeoJSON.FeatureCollection => ({ type: "FeatureCollection", features });
  return { zones: fc(zones), lines: fc(lines), points: fc(points) };
}
