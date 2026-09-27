import { describe, expect, test } from "bun:test";
import type { LonLat, Polygon } from "./api";
import { areaHa, inside, offset, rect, ringAgainst, signedDistance } from "./geo";

// A 100 m square near Ames with a 20 m square hole in its middle.
const sw: LonLat = [-93.62, 42.03];
const outer = rect(sw, 100, 100);
const hole = rect(offset(sw, 40, 40), 20, 20);
const holed: Polygon = { type: "Polygon", coordinates: [outer.coordinates[0], hole.coordinates[0].slice().reverse()] };
const at = (dx: number, dy: number) => offset(sw, dx, dy);

describe("polygon with a hole", () => {
  test("area is the outer ring less the hole", () => {
    expect(areaHa(outer)).toBeCloseTo(1, 3);
    expect(areaHa(holed)).toBeCloseTo(0.96, 3);
  });

  test("a point in the hole is outside; between the rings is inside", () => {
    expect(inside(at(50, 50), outer)).toBe(true);
    expect(inside(at(50, 50), holed)).toBe(false);
    expect(inside(at(20, 50), holed)).toBe(true);
    expect(inside(at(150, 50), holed)).toBe(false);
  });

  test("distance counts hole edges, negative inside the hole", () => {
    // Centre of the hole: 10 m to its edge, and outside.
    expect(signedDistance(at(50, 50), holed)).toBeCloseTo(-10, 1);
    // 5 m from the hole's west edge, 35 m from the outer west edge: nearest is the hole.
    expect(signedDistance(at(35, 50), holed)).toBeCloseTo(5, 1);
    // Near the outer edge.
    expect(signedDistance(at(3, 50), holed)).toBeCloseTo(3, 1);
    // Outside the outer ring.
    expect(signedDistance(at(-7, 50), holed)).toBeCloseTo(-7, 1);
  });

  test("a plain ring is unchanged", () => {
    expect(signedDistance(at(50, 50), outer)).toBeCloseTo(50, 1);
    expect(inside(at(99, 99), outer)).toBe(true);
  });
});

describe("a ring against another", () => {
  const r = (x: number, y: number, w: number, h: number) => rect(offset(sw, x, y), w, h).coordinates[0];
  test("inside, outside, or cut by its edge; touching counts as cut", () => {
    const big = outer.coordinates[0];
    expect(ringAgainst(r(40, 40, 20, 20), big)).toBe("in");
    expect(ringAgainst(r(140, 40, 20, 20), big)).toBe("out");
    expect(ringAgainst(r(90, 40, 20, 20), big)).toBe("cut");
    // Its west edge on the outer ring's east edge.
    expect(ringAgainst([big[1], big[2], offset(big[2], 20, 0), offset(big[1], 20, 0), big[1]], big)).toBe("cut");
  });
});

