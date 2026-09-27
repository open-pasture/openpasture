// What each kind of map feature is drawn as and where it sits in the Draw menu, plus the list
// operations the features store needs. Small on purpose: it loads with the app; the geometry,
// dates and drawing in model.ts load with the map.

import type { FeatureGeometry, FeatureKind, MapFeature, Paddock } from "../../api";

export type Shape = FeatureGeometry["type"];

export interface KindSpec {
  kind: FeatureKind;
  label: string;
  // What it may be drawn as, the first being the default.
  shapes: Shape[];
  // Place in the Draw menu (Paddock is 10).
  order: number;
  key?: string;
}

export const KINDS: KindSpec[] = [
  { kind: "exclusion", label: "Exclusion", shapes: ["Polygon"], order: 20, key: "x" },
  { kind: "water", label: "Water", shapes: ["Point", "Polygon"], order: 30 },
  { kind: "gate", label: "Gate", shapes: ["Point"], order: 40 },
  { kind: "shade", label: "Shade", shapes: ["Point", "Polygon"], order: 50 },
  { kind: "hazard", label: "Hazard", shapes: ["Point", "Polygon"], order: 60 },
  { kind: "road", label: "Road", shapes: ["LineString"], order: 70 },
  { kind: "neighbour_line", label: "Neighbour line", shapes: ["LineString"], order: 80 },
  { kind: "farm_boundary", label: "Farm boundary", shapes: ["Polygon"], order: 90 },
];

export const spec = (k: FeatureKind): KindSpec => KINDS.find((s) => s.kind === k)!;

// Features whose paddock still exists (deleting a paddock deletes its features on the server).
export function withPaddocks(list: MapFeature[], paddocks: Paddock[]): MapFeature[] {
  const ids = new Set(paddocks.map((p) => p.id));
  const kept = list.filter((f) => !f.paddock_id || ids.has(f.paddock_id));
  return kept.length === list.length ? list : kept;
}

// Insert or replace by id, keeping stored order (new ones last).
export function upsert(list: MapFeature[], f: MapFeature): MapFeature[] {
  const i = list.findIndex((x) => x.id === f.id);
  if (i < 0) return [...list, f];
  const next = list.slice();
  next[i] = f;
  return next;
}
