// What the pre-send check shows, as pure functions: the footer's sentences and facts line, and
// the map's drawing (overlaps hatched, water ringed, weak GPS, offline collars, sweep back lines).

import type { Finding, LonLat } from "../../api";
import type { CheckFacts, CheckResult } from "../../api/f";
import type { Fmt } from "../../units";

export const MAX_SENTENCES = 3;
const RANK: Record<Finding["severity"], number> = { critical: 2, warning: 1, info: 0 };

// The footer's sentences: warnings and worse, critical first, at most three. Info is drawn only.
export function sentences(findings: readonly Finding[], max = MAX_SENTENCES): Finding[] {
  return findings
    .map((f, i) => ({ f, i }))
    .filter(({ f }) => f.severity !== "info")
    .sort((a, b) => RANK[b.f.severity] - RANK[a.f.severity] || a.i - b.i)
    .slice(0, max)
    .map(({ f }) => f);
}

const one = (v: number) => (Math.round(v * 10) / 10).toFixed(1);

// "30.6 ac  250 hd  5,340 ft²/hd  3.1 d": area, head and area per head when there are head,
// grazing days when the forage is known.
export function factsLine(u: Fmt, f: CheckFacts): string {
  const parts = [u.area(f.area_ha)];
  if (f.head > 0) parts.push(`${f.head} hd`, u.perHead(f.m2_per_head));
  if (f.grazing_days !== undefined) parts.push(`${one(f.grazing_days)} d`);
  return parts.join("  ");
}

// "~16 min", "~1 h 26 min".
export function minutesText(m: number): string {
  const r = Math.max(1, Math.round(m));
  if (r < 60) return `~${r} min`;
  const h = Math.floor(r / 60), rest = r % 60;
  return rest ? `~${h} h ${rest} min` : `~${h} h`;
}

type FC = GeoJSON.FeatureCollection;
const fc = (features: GeoJSON.Feature[]): FC => ({ type: "FeatureCollection", features });
const feat = (geometry: GeoJSON.Geometry, properties: GeoJSON.GeoJsonProperties = {}): GeoJSON.Feature => ({ type: "Feature", properties, geometry });

const HATCHED = new Set(["overlaps_exclusion", "overlaps_hazard", "crosses_farm_boundary"]);
const CROSSED = new Set(["crosses_road", "crosses_neighbour_line"]);

export interface Drawing {
  hatch: FC; // overlaps kept out or taken in, and the part past the farm boundary
  lines: FC; // roads and neighbour lines inside the shape
  water: FC; // water points inside, ringed
  waterAreas: FC; // water areas inside, outlined
  weak: FC; // weak GPS cells, clipped to the shape
  offline: FC; // offline collars' last fixes, drawn hollow
  back: FC; // the sweep's back lines
  label?: LonLat; // where "~16 min" goes: the north end of the middle back line, clear of the herd
}

const points = (g: GeoJSON.Geometry): LonLat[] =>
  g.type === "Point" ? [g.coordinates as LonLat] : g.type === "MultiPoint" ? (g.coordinates as LonLat[]) : [];

// What the map draws for a check; empty without one.
export function drawing(r?: CheckResult): Drawing {
  const out = { hatch: [], lines: [], water: [], waterAreas: [], weak: [], offline: [], back: [] } as Record<Exclude<keyof Drawing, "label">, GeoJSON.Feature[]>;
  for (const f of r?.findings ?? []) {
    const g = f.geometry;
    if (!g) continue;
    if (HATCHED.has(f.code) && (g.type === "Polygon" || g.type === "MultiPolygon")) out.hatch.push(feat(g, { code: f.code }));
    else if (CROSSED.has(f.code)) out.lines.push(feat(g, { code: f.code }));
    else if (f.code === "water_inside") {
      if (g.type === "Point") out.water.push(feat(g));
      else out.waterAreas.push(feat(g));
    } else if (f.code === "weak_coverage") out.weak.push(feat(g));
    else if (f.code === "collars_offline") out.offline.push(...points(g).map((p) => feat({ type: "Point", coordinates: p })));
  }
  const lines = r?.sweep?.back_lines ?? [];
  for (const l of lines) out.back.push(feat({ type: "LineString", coordinates: l }));
  const middle = lines[Math.floor(lines.length / 2)];
  const label: LonLat | undefined = middle?.length ? northEnd(middle) : undefined;
  return { hatch: fc(out.hatch), lines: fc(out.lines), water: fc(out.water), waterAreas: fc(out.waterAreas), weak: fc(out.weak), offline: fc(out.offline), back: fc(out.back), label };
}

function northEnd(line: LonLat[]): LonLat {
  return line.reduce((a, b) => (b[1] > a[1] ? b : a));
}

export type BBox = [number, number, number, number];

function coordsOf(g: GeoJSON.Geometry): LonLat[] {
  switch (g.type) {
    case "Point": return [g.coordinates as LonLat];
    case "MultiPoint":
    case "LineString": return g.coordinates as LonLat[];
    case "MultiLineString":
    case "Polygon": return (g.coordinates as LonLat[][]).flat();
    case "MultiPolygon": return (g.coordinates as LonLat[][][]).flat(2);
    case "GeometryCollection": return g.geometries.flatMap(coordsOf);
  }
}

function bboxOf(pts: LonLat[]): LonLat | BBox | undefined {
  if (!pts.length) return undefined;
  if (pts.length === 1) return pts[0];
  const lon = pts.map((p) => p[0]), lat = pts.map((p) => p[1]);
  return [Math.min(...lon), Math.min(...lat), Math.max(...lon), Math.max(...lat)];
}

// The collars a finding is about.
export const collarsOf = (f: Finding): string[] => (f.targets ?? []).filter(([k]) => k === "collar").map(([, id]) => id);

// Where to look for a finding's cause: its geometry, else its collars where they are now.
export function causeOf(f: Finding, where: (collarId: string) => LonLat | undefined): LonLat | BBox | undefined {
  if (f.geometry) return bboxOf(coordsOf(f.geometry));
  return bboxOf(collarsOf(f).flatMap((id) => {
    const p = where(id);
    return p ? [p] : [];
  }));
}
