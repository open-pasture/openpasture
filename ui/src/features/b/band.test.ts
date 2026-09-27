import { describe, expect, test } from "bun:test";
import type { Polygon } from "../../api";
import { ccw, offsetExpr } from "./band";

// MapLibre's metres per pixel at zoom z and latitude lat (512 px tiles).
const mpp = (z: number, lat: number) => (40_075_016.686 * Math.cos((lat * Math.PI) / 180)) / (512 * 2 ** z);

// The expression's value at zoom z: exponential base 2 between its two stops.
function at(expr: ReturnType<typeof offsetExpr>, z: number): number {
  const [, , , z0, v0, z1, v1] = expr as [string, unknown, unknown, number, number, number, number];
  const t = (2 ** (z - z0) - 1) / (2 ** (z1 - z0) - 1);
  return v0 + (v1 - v0) * t;
}

describe("warn band", () => {
  test("the offset is warn_m in pixels at every zoom, inward", () => {
    for (const lat of [0, 42.03, 60]) {
      const e = offsetExpr(5, lat);
      for (const z of [12, 16, 18.5, 21]) {
        expect(at(e, z)).toBeCloseTo(-5 / mpp(z, lat), 6);
      }
    }
    // 5 m at Ames, zoom 17: about 11 px.
    expect(-at(offsetExpr(5, 42.03), 17)).toBeCloseTo(11.3, 1);
  });

  test("rings come out counter-clockwise, so inward is the left side", () => {
    const cw: Polygon = { type: "Polygon", coordinates: [[[0, 0], [0, 1], [1, 1], [1, 0], [0, 0]]] };
    const r = ccw(cw);
    expect(r).toEqual([[0, 0], [1, 0], [1, 1], [0, 1], [0, 0]]);
    const again: Polygon = { type: "Polygon", coordinates: [r as [number, number][]] };
    expect(ccw(again)).toEqual(r);
  });
});
