import { describe, expect, test } from "bun:test";
import type { LonLat, MapFeature, Paddock, Polygon } from "../../api";
import { offset, toXY } from "../../geo";
import { fmt } from "../../units";
import { hatchBitmap, HATCH, iconBitmap, ICONS, PR } from "./icons";
import {
  circle, endOfDay, facts, isActive, KINDS, lastDay, lineLength, localDate, mapData, modeFor, nextChange, scopePaddock, spec, upsert, when,
  withPaddocks, zonedTime,
} from "./model";

const O: LonLat = [-93.62, 42.03];
const at = (e: number, n: number) => offset(O, e, n);
const square = (e: number, n: number, side: number): Polygon => ({
  type: "Polygon",
  coordinates: [[at(e, n), at(e + side, n), at(e + side, n + side), at(e, n + side), at(e, n)]],
});
const paddock = (id: string, name: string, g: Polygon, area_ha: number): Paddock => ({
  id, name, geometry: g, area_ha, status: "resting", created_at: "2026-09-01T00:00:00Z",
});
const P1 = paddock("pad_1", "P1", square(0, 0, 400), 16);
const P2 = paddock("pad_2", "P2", square(1000, 0, 400), 16);
const CORNER = paddock("pad_3", "Corner", square(0, 0, 100), 1);

let n = 0;
const feature = (f: Partial<MapFeature> & Pick<MapFeature, "kind" | "geometry">): MapFeature => ({
  id: `fea_${++n}`, props: {}, created_at: "2026-09-27T00:00:00Z", updated_at: "2026-09-27T00:00:00Z", ...f,
});
const dist = (a: LonLat, b: LonLat) => {
  const [ax, ay] = toXY(a, a[1]), [bx, by] = toXY(b, a[1]);
  return Math.hypot(bx - ax, by - ay);
};

describe("kinds", () => {
  test("the Draw menu order follows Paddock and only exclusion has a key", () => {
    expect(KINDS.map((k) => k.kind)).toEqual(["exclusion", "water", "gate", "shade", "hazard", "road", "neighbour_line", "farm_boundary"]);
    const orders = KINDS.map((k) => k.order);
    expect(new Set(orders).size).toBe(orders.length);
    expect(Math.min(...orders)).toBeGreaterThan(10);
    expect(KINDS.filter((k) => k.key).map((k) => [k.kind, k.key])).toEqual([["exclusion", "x"]]);
    expect(spec("water").shapes).toEqual(["Point", "Polygon"]);
    expect(spec("gate").shapes).toEqual(["Point"]);
    expect(spec("road").shapes).toEqual(["LineString"]);
  });

  test("upsert keeps stored order and pruning keeps farm-wide features", () => {
    const a = feature({ kind: "gate", geometry: { type: "Point", coordinates: at(1, 1) }, paddock_id: "pad_1" });
    const b = feature({ kind: "road", geometry: { type: "LineString", coordinates: [at(0, 0), at(10, 0)] } });
    const list = upsert(upsert([], a), b);
    expect(list.map((f) => f.id)).toEqual([a.id, b.id]);
    const renamed = { ...a, name: "east gate" };
    expect(upsert(list, renamed)).toEqual([renamed, b]);
    expect(withPaddocks(list, [P1])).toBe(list);
    expect(withPaddocks(list, [P2])).toEqual([b]);
  });
});

describe("active windows", () => {
  const t = Date.parse("2026-09-27T12:00:00Z");
  const iso = (h: number) => new Date(t + h * 3600_000).toISOString();
  const g = square(10, 10, 20);
  const lasting = feature({ kind: "exclusion", geometry: g });
  const current = feature({ kind: "exclusion", geometry: g, active_from: iso(-1), active_until: iso(1) });
  const future = feature({ kind: "exclusion", geometry: g, active_from: iso(2) });
  const expired = feature({ kind: "exclusion", geometry: g, active_until: iso(-1) });

  test("from is inclusive, until exclusive", () => {
    expect([lasting, current, future, expired].map((f) => isActive(f, t))).toEqual([true, true, false, false]);
    expect(isActive(current, t + 3600_000)).toBe(false);
    expect(isActive(future, t + 2 * 3600_000)).toBe(true);
  });

  test("the next start or end after now", () => {
    expect(nextChange([lasting, current, future, expired], t)).toBe(t + 3600_000);
    expect(nextChange([future, expired], t)).toBe(t + 2 * 3600_000);
    expect(nextChange([lasting, expired], t)).toBeUndefined();
  });

  test("the map draws only what is in effect", () => {
    const d = mapData([lasting, current, future, expired], t);
    expect(d.zones.features.map((f) => f.properties?.id)).toEqual([lasting.id, current.id]);
    expect(mapData([lasting, current, future, expired], t + 3 * 3600_000).zones.features.map((f) => f.properties?.id)).toEqual([lasting.id, future.id]);
  });
});

describe("what the overlay draws", () => {
  test("each kind lands in its source", () => {
    const wet = feature({ kind: "exclusion", geometry: square(10, 10, 20) });
    const pond = feature({ kind: "water", name: "pond", geometry: square(100, 100, 40) });
    const trough = feature({ kind: "water", geometry: { type: "Point", coordinates: at(50, 50) } });
    const gate = feature({ kind: "gate", geometry: { type: "Point", coordinates: at(400, 200) } });
    const well = feature({ kind: "hazard", geometry: { type: "Point", coordinates: at(300, 50) }, props: { radius_m: 15 } });
    const road = feature({ kind: "road", geometry: { type: "LineString", coordinates: [at(0, -20), at(900, -20)] } });
    const edge = feature({ kind: "farm_boundary", geometry: square(-60, -60, 1600) });
    const d = mapData([wet, pond, trough, gate, well, road, edge], Date.now());
    const ids = (fc: GeoJSON.FeatureCollection) => fc.features.map((f) => f.properties?.id);
    expect(ids(d.zones)).toEqual([wet.id, pond.id, well.id]);
    expect(ids(d.points)).toEqual([pond.id, trough.id, gate.id, well.id]);
    expect(ids(d.lines)).toEqual([road.id, edge.id]);
    // The farm boundary is a line, not an area a click inside would find.
    expect(d.lines.features[1].geometry.type).toBe("MultiLineString");
    // A hazard point's radius is drawn as a circle around it.
    const ring = (d.zones.features[2].geometry as GeoJSON.Polygon).coordinates[0] as LonLat[];
    for (const p of ring) expect(Math.abs(dist(at(300, 50), p) - 15)).toBeLessThan(0.05);
    // The pond's icon sits at its centre.
    const c = d.points.features[0].geometry as GeoJSON.Point;
    expect(dist(c.coordinates as LonLat, at(120, 120))).toBeLessThan(1);
  });

  test("a circle is closed and a line has its length", () => {
    const c = circle(O, 25, 12).coordinates[0];
    expect(c.length).toBe(13);
    expect(c[0]).toEqual(c[12]);
    expect(lineLength([at(0, 0), at(300, 0), at(300, 400)])).toBeCloseTo(700, 0);
    expect(lineLength([at(0, 0)])).toBe(0);
  });
});

describe("scope", () => {
  test("an exclusion belongs to the smallest paddock holding its centre", () => {
    expect(scopePaddock(square(150, 150, 30), [P1, P2])?.id).toBe("pad_1");
    expect(scopePaddock(square(20, 20, 30), [P1, CORNER, P2])?.id).toBe("pad_3");
    expect(scopePaddock(square(1100, 100, 30), [P1, P2])?.id).toBe("pad_2");
  });

  test("else the paddock holding most of its corners, else none", () => {
    // Centre in the gap between P1 and P2; two of three corners in P2.
    const g: Polygon = { type: "Polygon", coordinates: [[at(1010, 10), at(1030, 60), at(600, 60), at(1010, 10)]] };
    expect(scopePaddock(g, [P1, P2])?.id).toBe("pad_2");
    expect(scopePaddock(square(600, 100, 30), [P1, P2])).toBeUndefined();
  });
});

describe("farm time", () => {
  const tz = "America/Chicago";
  test("until a date lasts through that day in farm time", () => {
    expect(endOfDay("2026-10-03", tz)).toBe("2026-10-04T05:00:00.000Z");
    // Across the end of daylight time (2026-11-01 02:00 CDT → 01:00 CST).
    expect(endOfDay("2026-10-31", tz)).toBe("2026-11-01T05:00:00.000Z");
    expect(endOfDay("2026-11-01", tz)).toBe("2026-11-02T06:00:00.000Z");
    expect(endOfDay("2026-12-31", "UTC")).toBe("2027-01-01T00:00:00.000Z");
    expect(endOfDay("2026-10-03", "Pacific/Auckland")).toBe("2026-10-03T11:00:00.000Z");
  });

  test("the date input shows the last day covered", () => {
    for (const d of ["2026-10-03", "2026-10-31", "2026-11-01", "2027-03-14"]) expect(lastDay(endOfDay(d, tz), tz)).toBe(d);
    expect(lastDay("2026-09-27T19:05:00Z", tz)).toBe("2026-09-27");
    expect(localDate(Date.parse("2026-09-28T04:59:00Z"), tz)).toBe("2026-09-27");
    expect(zonedTime(2026, 3, 8, 3, 0, tz)).toBe(Date.parse("2026-03-08T08:00:00Z"));
  });

  test("dates read as the day, with the time only when it isn't midnight", () => {
    expect(when(endOfDay("2026-10-03", tz), tz, true)).toBe("Oct 3");
    expect(when(endOfDay("2026-10-03", tz), tz)).toBe("Oct 4");
    expect(when("2026-09-27T19:05:00Z", tz, true)).toBe("Sep 27 14:05");
    expect(when("2026-09-27T19:05:00Z", "no/such_zone")).toBe("Sep 27 19:05");
  });
});

describe("facts line", () => {
  const tz = "America/Chicago";
  test("kind, size, paddock and dates, in the farm's units", () => {
    const wet = feature({ kind: "exclusion", geometry: square(10, 10, 60), paddock_id: "pad_1", active_until: endOfDay("2026-10-03", tz) });
    expect(facts(wet, fmt("metric"), [P1], tz)).toBe("exclusion  0.4 ha  P1  until Oct 3");
    expect(facts(wet, fmt("imperial"), [P1], tz)).toBe("exclusion  0.9 ac  P1  until Oct 3");
    const well = feature({ kind: "hazard", geometry: { type: "Point", coordinates: at(1, 1) }, props: { radius_m: 10 } });
    expect(facts(well, fmt("imperial"), [], tz)).toBe("hazard  33 ft around");
    // What the sheet has a field for can be left out.
    expect(facts(well, fmt("imperial"), [], tz, { radius: false })).toBe("hazard");
    expect(facts(wet, fmt("metric"), [P1], tz, { until: false })).toBe("exclusion  0.4 ha  P1");
    const road = feature({ kind: "road", geometry: { type: "LineString", coordinates: [at(0, 0), at(300, 0)] } });
    expect(facts(road, fmt("metric"), [], tz)).toBe("road  300 m");
    const pen = feature({ kind: "neighbour_line", geometry: { type: "LineString", coordinates: [at(0, 0), at(30, 0)] }, active_from: "2026-09-27T11:00:00Z" });
    expect(facts(pen, fmt("metric"), [], tz)).toBe("neighbour line  30 m  from Sep 27 06:00");
  });
});

describe("pixel art", () => {
  test("icons are drawn in their colour inside an ink edge", () => {
    for (const [kind, rows] of Object.entries(ICONS)) {
      expect(rows.every((r) => r.length === rows[0].length)).toBe(true);
      const b = iconBitmap(rows, "#F3F2EA");
      // 2 css px per grid pixel plus a 1 css px edge each side.
      expect([b.width, b.height]).toEqual([(rows[0].length * 2 + 2) * PR, (rows.length * 2 + 2) * PR]);
      expect(b.data.length).toBe(b.width * b.height * 4);
      const px = (x: number, y: number) => Array.from(b.data.slice((y * b.width + x) * 4, (y * b.width + x) * 4 + 4));
      const gx = rows[0].indexOf("#");
      // The first lit grid pixel of the top row, its middle, and the edge just above it.
      const cx = PR + gx * 2 * PR + PR, cy = PR + PR;
      expect(px(cx, cy)).toEqual([0xf3, 0xf2, 0xea, 255]);
      expect(px(cx, 0)).toEqual([0x0c, 0x16, 0x06, 210]);
      expect(px(0, b.height - 1)[3], kind).toBe(rows[rows.length - 1][0] === "#" ? 210 : 0);
    }
  });

  test("the hatch tile repeats a one-pixel diagonal", () => {
    const b = hatchBitmap("#E5484D");
    const n = HATCH.length * 2 * PR;
    expect([b.width, b.height]).toEqual([n, n]);
    const alpha = (x: number, y: number) => b.data[(y * n + x) * 4 + 3];
    for (let i = 0; i < n; i++) expect(alpha(i, i)).toBe(200);
    expect(alpha(n - 1, 0)).toBe(0);
    expect(Array.from(b.data.slice(0, 3))).toEqual([0xe5, 0x48, 0x4d]);
  });
});

test("a feature is drawn and reshaped in its shape's mode: exclusions red, other areas fg, points and lines", () => {
  expect(modeFor("exclusion", "Polygon")).toBe("exclusion");
  expect(modeFor("water", "Polygon")).toBe("paddock");
  expect(modeFor("farm_boundary", "Polygon")).toBe("paddock");
  expect(modeFor("gate", "Point")).toBe("point");
  expect(modeFor("hazard", "Point")).toBe("point");
  expect(modeFor("road", "LineString")).toBe("line");
  expect(modeFor("neighbour_line", "LineString")).toBe("line");
});
