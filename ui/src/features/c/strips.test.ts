import { describe, expect, test } from "bun:test";
import type { LonLat, Polygon } from "../../api";
import { centroid, inside, offset, rect } from "../../geo";
import { fmt } from "../../units";
import { bearing, days, depth, facts, handleAt, labelAt, lengthwise, snap, tetherFrom, toward } from "./strips";

const sw: LonLat = [-93.625, 42.03];
// 400 m east-west by 150 m north-south.
const wide = rect(sw, 400, 150);

describe("orientation", () => {
  test("bearings are compass degrees", () => {
    expect(bearing(sw, offset(sw, 0, 100))).toBeCloseTo(0, 6);
    expect(bearing(sw, offset(sw, 100, 0))).toBeCloseTo(90, 6);
    expect(bearing(sw, offset(sw, 0, -100))).toBeCloseTo(180, 6);
    expect(bearing(sw, offset(sw, -100, 0))).toBeCloseTo(270, 6);
    expect(bearing(sw, offset(sw, 100, 100))).toBeCloseTo(45, 4);
    expect(bearing(sw, toward(sw, 123, 80))).toBeCloseTo(123, 4);
  });

  test("depth is measured along the advance direction", () => {
    expect(depth(wide, 0)).toBeCloseTo(150, 0);
    expect(depth(wide, 90)).toBeCloseTo(400, 0);
    expect(depth(wide, 270)).toBeCloseTo(400, 0);
  });

  test("the default advances down the paddock's length", () => {
    expect(lengthwise(wide)).toBe(90);
    expect(lengthwise(rect(sw, 150, 400))).toBe(0);
    // A long strip of land running north-east.
    const a = sw, b = toward(sw, 45, 500), c = toward(b, 135, 60), d = toward(sw, 135, 60);
    expect(lengthwise({ type: "Polygon", coordinates: [[a, b, c, d, a]] })).toBeCloseTo(45, -0.5);
  });

  test("snap keeps whole degrees in a circle", () => {
    expect(snap(359.6)).toBe(0);
    expect(snap(-10.2)).toBe(350);
    expect(snap(89.5)).toBe(90);
  });

  test("the handle sits just past the edge the strips advance toward", () => {
    const h = handleAt(wide, 0);
    expect(inside(h, wide)).toBe(false);
    expect(bearing(centroid(wide), h)).toBeCloseTo(0, 4);
    expect(bearing(centroid(wide), handleAt(wide, 90))).toBeCloseTo(90, 4);
  });
});

describe("labels", () => {
  test("a strip's number sits a fifth of the way along it, clear of the middle", () => {
    // Advancing north, a strip runs east-west: its number goes toward the west end.
    const s = rect(sw, 400, 30);
    const p = labelAt(s, 0);
    expect(inside(p, s)).toBe(true);
    expect(p[1]).toBeCloseTo(centroid(s)[1], 6);
    expect(bearing(centroid(s), p)).toBeCloseTo(270, 3);
    const [x] = [p[0] - sw[0]];
    expect(x / (offset(sw, 400, 0)[0] - sw[0])).toBeCloseTo(0.2, 2);
    // Advancing east, a strip runs north-south: toward its north end.
    const t = rect(sw, 30, 400);
    expect(bearing(centroid(t), labelAt(t, 90))).toBeCloseTo(0, 3);
  });

  test("an L-shaped strip's number stays on the strip", () => {
    const a = sw, b = offset(sw, 200, 0), c = offset(sw, 200, 30), d = offset(sw, 30, 30), e = offset(sw, 30, 200), f = offset(sw, 0, 200);
    const ell: Polygon = { type: "Polygon", coordinates: [[a, b, c, d, e, f, a]] };
    expect(inside(centroid(ell), ell)).toBe(false);
    expect(inside(labelAt(ell, 0), ell)).toBe(true);
    expect(inside(labelAt(ell, 45), ell)).toBe(true);
  });

  test("the tether starts at the edge the strips advance toward", () => {
    const e = tetherFrom(wide, 0);
    expect(bearing(centroid(wide), e)).toBeCloseTo(0, 4);
    expect(e[1]).toBeCloseTo(offset(sw, 0, 150)[1], 6);
  });
});

describe("facts line", () => {
  const strip = { geometry: rect(sw, 400, 30), area_ha: 1.2, grazeable_ha: 1.214, days: 2 };
  test("area in the farm's units, head, days", () => {
    expect(facts(fmt("imperial"), strip, 250)).toBe("3.0 ac  250 hd  2 d");
    expect(facts(fmt("metric"), strip, 1250)).toBe("1.2 ha  1,250 hd  2 d");
    expect(facts(fmt("metric"), { ...strip, days: 2.5 }, 250)).toBe("1.2 ha  250 hd  2.5 d");
  });
  test("no days without a forage estimate", () => {
    expect(facts(fmt("imperial"), { ...strip, days: undefined }, 250)).toBe("3.0 ac  250 hd");
  });
  test("days read whole or to a tenth", () => {
    expect(days(0.3)).toBe("0.3 d");
    expect(days(12)).toBe("12 d");
  });
});
