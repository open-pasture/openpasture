// Pure parts of the file imports: which kind of file, what a draft set keeps, FSA lines,
// where a track is at a moment. Tested in logic.test.ts.

import type { Animal } from "../../api";
import type { Draft, PositionSource, TrackPoint } from "../../api/k-files";
import type { MappingField } from "../../ui/MappingRow";

export type FileKind = "paddocks" | "positions";

// What a picked file is: paddock outlines or position history. GeoJSON can be either, so its
// first geometry decides. undefined = let the paddock reader say what's wrong.
export function fileKind(name: string, head: string): FileKind | undefined {
  const ext = name.toLowerCase().split(".").pop() ?? "";
  if (["kml", "kmz", "zip", "shp"].includes(ext)) return "paddocks";
  if (["csv", "tsv", "txt", "gpx"].includes(ext)) return "positions";
  const t = head.replace(/^﻿/, "").trimStart();
  if (t.startsWith("<")) return /<gpx[\s>]/.test(t) ? "positions" : "paddocks";
  const geom = /"type"\s*:\s*"(Polygon|MultiPolygon|Point|MultiPoint)"/.exec(t);
  if (geom) return geom[1].endsWith("Point") ? "positions" : "paddocks";
  return undefined;
}

export const ACCEPT = ".geojson,.json,.kml,.kmz,.zip,.csv,.tsv,.txt,.gpx";

// The drafts still kept (not clicked away) and their total area.
export function kept(drafts: Draft[], dropped: ReadonlySet<number>) {
  const keep = drafts.map((_, i) => i).filter((i) => !dropped.has(i));
  return { keep, areaHa: keep.reduce((a, i) => a + drafts[i].area_ha, 0) };
}

const FSA: [string, string][] = [["fsa_farm", "FSA farm"], ["fsa_tract", "FSA tract"], ["fsa_field", "FSA field"]];

// FSA numbers on a paddock, in farm, tract, field order; none when it has none.
export function fsaLines(props: Record<string, unknown> | undefined): [string, string][] {
  if (!props) return [];
  return FSA.flatMap(([k, label]) => {
    const v = props[k];
    return typeof v === "string" && v.trim() ? [[label, v.trim()] as [string, string]] : typeof v === "number" ? [[label, String(v)] as [string, string]] : [];
  });
}

// The columns a position file's MappingRow asks for.
export function mappingFields(source: PositionSource): MappingField[] {
  if (source === "geojson") return [{ key: "tag", label: "tag" }, { key: "time", label: "time", required: true }, { key: "accuracy", label: "accuracy" }];
  return [
    { key: "tag", label: "tag" },
    { key: "time", label: "time", required: true },
    { key: "lat", label: "lat", required: true },
    { key: "lon", label: "lon", required: true },
    { key: "accuracy", label: "accuracy" },
  ];
}

// The last point at or before `s` (unix seconds): where the animal was then. Before its first
// point there is none. Points are in time order.
export function at(points: TrackPoint[], s: number): TrackPoint | undefined {
  let lo = 0, hi = points.length - 1, found = -1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (points[mid][2] <= s) {
      found = mid;
      lo = mid + 1;
    } else hi = mid - 1;
  }
  return found >= 0 ? points[found] : undefined;
}

// First and last moment (ms) over every track; undefined with no points.
export function span(tracks: { points: TrackPoint[] }[]): [number, number] | undefined {
  let a = Infinity, b = -Infinity;
  for (const t of tracks)
    if (t.points.length) {
      a = Math.min(a, t.points[0][2]);
      b = Math.max(b, t.points[t.points.length - 1][2]);
    }
  return a <= b ? [a * 1000, b * 1000] : undefined;
}

// Animals a label can be given to: those still on the farm, by tag (031 before 118, 2 before 10).
export function choices(animals: Animal[]): Animal[] {
  return animals.filter((a) => !a.removed_at).sort((x, y) => x.tag.localeCompare(y.tag, undefined, { numeric: true }));
}
