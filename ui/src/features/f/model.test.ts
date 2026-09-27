import { describe, expect, test } from "bun:test";
import type { Finding, LonLat } from "../../api";
import type { CheckResult } from "../../api/f";
import { fmt } from "../../units";
import { causeOf, drawing, factsLine, MAX_SENTENCES, minutesText, sentences } from "./model";

const f = (code: string, severity: Finding["severity"], extra: Partial<Finding> = {}): Finding => ({ code, severity, text: code, ...extra });

describe("sentences", () => {
  test("warnings and worse, critical first, at most three", () => {
    const list = [
      f("no_water", "warning"),
      f("water_inside", "info"),
      f("rested_short", "warning"),
      f("crosses_road", "critical"),
      f("collars_offline", "warning"),
      f("weak_coverage", "warning"),
    ];
    expect(sentences(list).map((s) => s.code)).toEqual(["crosses_road", "no_water", "rested_short"]);
    expect(sentences(list)).toHaveLength(MAX_SENTENCES);
    expect(sentences([f("simplified", "info"), f("water_inside", "info")])).toEqual([]);
  });
});

describe("facts line", () => {
  const facts = { area_ha: 12.4, head: 250, m2_per_head: 496, grazing_days: 3.14, vertices: 4, holes: 0 };
  test("in the farm's units", () => {
    expect(factsLine(fmt("metric"), facts)).toBe("12.4 ha  250 hd  496 m²/hd  3.1 d");
    expect(factsLine(fmt("imperial"), facts)).toBe("30.6 ac  250 hd  5,340 ft²/hd  3.1 d");
  });
  test("nothing unknown", () => {
    expect(factsLine(fmt("imperial"), { ...facts, grazing_days: undefined })).toBe("30.6 ac  250 hd  5,340 ft²/hd");
    expect(factsLine(fmt("metric"), { ...facts, head: 0, m2_per_head: 0, grazing_days: undefined })).toBe("12.4 ha");
  });
});

test("sweep minutes", () => {
  expect(minutesText(16.4)).toBe("~16 min");
  expect(minutesText(0.2)).toBe("~1 min");
  expect(minutesText(86)).toBe("~1 h 26 min");
  expect(minutesText(120)).toBe("~2 h");
});

describe("drawing", () => {
  const sq = (x: number): GeoJSON.Polygon => ({ type: "Polygon", coordinates: [[[x, 0], [x + 1, 0], [x + 1, 1], [x, 1], [x, 0]]] });
  const result: CheckResult = {
    sent: sq(0),
    facts: { area_ha: 1, head: 1, m2_per_head: 1, vertices: 4, holes: 0 },
    findings: [
      f("overlaps_exclusion", "info", { geometry: sq(1) }),
      f("overlaps_hazard", "warning", { geometry: { type: "MultiPolygon", coordinates: [sq(2).coordinates, sq(3).coordinates] } }),
      f("crosses_farm_boundary", "warning", { geometry: sq(4) }),
      f("crosses_road", "critical", { geometry: { type: "LineString", coordinates: [[0, 0], [5, 5]] } }),
      f("water_inside", "info", { geometry: { type: "Point", coordinates: [0.5, 0.5] } }),
      f("water_inside", "info", { geometry: sq(6) }),
      f("weak_coverage", "warning", { geometry: sq(7) }),
      f("collars_offline", "warning", { geometry: { type: "MultiPoint", coordinates: [[1, 1], [2, 2]] }, targets: [["collar", "a"], ["collar", "b"]] }),
      f("no_water", "warning"),
    ],
    sweep: { back_lines: [[[0, 0], [0, 2]], [[1, 0], [1, 3]], [[2, 0], [2, 2]]], minutes: 16 },
  };
  test("each finding where it is drawn", () => {
    const d = drawing(result);
    expect(d.hatch.features.map((x) => x.properties?.code)).toEqual(["overlaps_exclusion", "overlaps_hazard", "crosses_farm_boundary"]);
    expect(d.lines.features).toHaveLength(1);
    expect(d.water.features).toHaveLength(1);
    expect(d.waterAreas.features).toHaveLength(1);
    expect(d.weak.features).toHaveLength(1);
    expect(d.offline.features.map((x) => (x.geometry as GeoJSON.Point).coordinates)).toEqual([[1, 1], [2, 2]]);
    expect(d.back.features).toHaveLength(3);
    expect(d.label).toEqual([1, 3]);
  });
  test("nothing without a check", () => {
    const d = drawing(undefined);
    expect(d.hatch.features).toEqual([]);
    expect(d.label).toBeUndefined();
  });
});

describe("cause", () => {
  const where = (id: string): LonLat | undefined => ({ a: [1, 1] as LonLat, b: [3, 2] as LonLat })[id];
  test("the geometry's box, a point, or the collars where they are", () => {
    expect(causeOf(f("crosses_road", "critical", { geometry: { type: "LineString", coordinates: [[0, 0], [5, 4]] } }), where)).toEqual([0, 0, 5, 4]);
    expect(causeOf(f("water_inside", "info", { geometry: { type: "Point", coordinates: [2, 3] } }), where)).toEqual([2, 3]);
    expect(causeOf(f("slots_full", "warning", { targets: [["collar", "a"], ["collar", "b"], ["collar", "gone"]] }), where)).toEqual([1, 1, 3, 2]);
    expect(causeOf(f("slots_full", "warning", { targets: [["collar", "a"]] }), where)).toEqual([1, 1]);
    expect(causeOf(f("no_water", "warning"), where)).toBeUndefined();
  });
});
