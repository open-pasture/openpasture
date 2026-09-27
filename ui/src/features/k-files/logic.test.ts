import { describe, expect, test } from "bun:test";
import type { Animal, Polygon } from "../../api";
import type { Draft, TrackPoint } from "../../api/k-files";
import { at, choices, fileKind, fsaLines, kept, mappingFields, span } from "./logic";

const sq: Polygon = { type: "Polygon", coordinates: [[[-93.62, 42.03], [-93.61, 42.03], [-93.61, 42.04], [-93.62, 42.03]]] };
const draft = (name: string, area_ha: number): Draft => ({ name, geometry: sq, area_ha });

describe("which kind of file", () => {
  test("by extension", () => {
    expect(fileKind("Fields.KMZ", "")).toBe("paddocks");
    expect(fileKind("export.zip", "")).toBe("paddocks");
    expect(fileKind("fields.kml", "")).toBe("paddocks");
    expect(fileKind("collars.csv", "")).toBe("positions");
    expect(fileKind("walk.gpx", "")).toBe("positions");
  });
  test("GeoJSON by its first geometry", () => {
    expect(fileKind("a.geojson", '{"type":"FeatureCollection","features":[{"type":"Feature","geometry":{"type": "MultiPolygon"')).toBe("paddocks");
    expect(fileKind("a.json", '﻿{"type":"FeatureCollection","features":[{"geometry":{"type":"Point","coordinates":[0,0]}}]}')).toBe("positions");
    expect(fileKind("a.json", "{}")).toBeUndefined();
    expect(fileKind("track.xml", '<?xml version="1.0"?><gpx version="1.1">')).toBe("positions");
    expect(fileKind("doc.xml", "<kml>")).toBe("paddocks");
  });
});

describe("drafts", () => {
  test("clicked-away drafts leave the kept set and its area", () => {
    const ds = [draft("North 40", 16.2), draft("Creek", 9.1), draft("Hill", 5.5)];
    expect(kept(ds, new Set())).toEqual({ keep: [0, 1, 2], areaHa: 16.2 + 9.1 + 5.5 });
    const k = kept(ds, new Set([1]));
    expect(k.keep).toEqual([0, 2]);
    expect(k.areaHa).toBeCloseTo(21.7, 9);
    expect(kept(ds, new Set([0, 1, 2]))).toEqual({ keep: [], areaHa: 0 });
  });
});

describe("FSA lines", () => {
  test("farm, tract, field in that order, only those present", () => {
    expect(fsaLines({ fsa_field: "3", fsa_farm: "1234", other: "x" })).toEqual([["FSA farm", "1234"], ["FSA field", "3"]]);
    expect(fsaLines({ fsa_tract: 5678 })).toEqual([["FSA tract", "5678"]]);
    expect(fsaLines({ fsa_farm: "  " })).toEqual([]);
    expect(fsaLines(undefined)).toEqual([]);
  });
});

describe("tracks", () => {
  const pts: TrackPoint[] = [[0, 0, 100], [1, 1, 200], [2, 2, 300]];
  test("where an animal was at a moment", () => {
    expect(at(pts, 50)).toBeUndefined();
    expect(at(pts, 100)).toEqual([0, 0, 100]);
    expect(at(pts, 250)).toEqual([1, 1, 200]);
    expect(at(pts, 999)).toEqual([2, 2, 300]);
    expect(at([], 5)).toBeUndefined();
  });
  test("the span over every track", () => {
    expect(span([{ points: pts }, { points: [[0, 0, 50], [0, 0, 150]] }, { points: [] }])).toEqual([50_000, 300_000]);
    expect(span([])).toBeUndefined();
  });
});

describe("mapping and animals", () => {
  test("GeoJSON points need no lat/lon columns", () => {
    expect(mappingFields("geojson").map((f) => f.key)).toEqual(["tag", "time", "accuracy"]);
    expect(mappingFields("csv").filter((f) => f.required).map((f) => f.key)).toEqual(["time", "lat", "lon"]);
  });
  test("choices leave out removed animals and sort tags as numbers", () => {
    const a = (tag: string, removed?: string): Animal => ({ id: `ani_${tag}`, tag, herd_id: "herd_1", removed_at: removed });
    expect(choices([a("118"), a("10"), a("031"), a("2"), a("7", "2025-01-01T00:00:00Z")]).map((x) => x.tag)).toEqual(["2", "10", "031", "118"]);
  });
});
