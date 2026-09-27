import { describe, expect, test } from "bun:test";
import { hasData } from "./have";
import { droughtFill, floodOpacity, ndviFill, REST_STEPS, restFill, restLabel, TONE } from "./ramp";

describe("rest ramp", () => {
  test("more rest shows more grass, in steps", () => {
    const at = (d: number) => restFill(d)!.opacity;
    expect(at(0)).toBe(0.04);
    expect(at(6.9)).toBe(0.04);
    expect(at(7)).toBe(0.1);
    expect(at(13.9)).toBe(0.1);
    expect(at(14)).toBe(0.17);
    expect(at(28)).toBe(0.25);
    expect(at(44.9)).toBe(0.25);
    expect(at(45)).toBe(0.34);
    expect(at(400)).toBe(0.34);
    expect(restFill(20)!.color).toBe(TONE.grass);
  });

  test("never goes back down as rest grows", () => {
    let last = 0;
    for (let d = 0; d <= 120; d += 0.5) {
      const o = restFill(d)!.opacity;
      expect(o).toBeGreaterThanOrEqual(last);
      last = o;
    }
    expect(REST_STEPS.map(([from]) => from)).toEqual([0, 7, 14, 28, 45]);
  });

  test("a paddock grazed now, or with no record, has no fill", () => {
    expect(restFill(30, true)).toBeUndefined();
    expect(restFill(undefined)).toBeUndefined();
    expect(restFill(Number.NaN)).toBeUndefined();
    expect(restFill(-1)).toBeUndefined();
  });

  test("labels", () => {
    expect(restLabel({ paddock_id: "p", grazing: true, rest_days: 0 })).toBe("now");
    expect(restLabel({ paddock_id: "p", rest_days: 0.4 })).toBe("<1 d");
    expect(restLabel({ paddock_id: "p", rest_days: 12.9 })).toBe("12 d");
    expect(restLabel({ paddock_id: "p" })).toBeUndefined();
  });
});

describe("ndvi, drought, flood", () => {
  test("ndvi warms bare ground and greens cover", () => {
    expect(ndviFill(0.2)).toEqual({ color: TONE.warn, opacity: 0.22 });
    expect(ndviFill(0.35)).toEqual({ color: TONE.warn, opacity: 0.12 });
    expect(ndviFill(0.45)!.color).toBe(TONE.grass);
    expect(ndviFill(0.85)).toEqual({ color: TONE.grass, opacity: 0.32 });
    expect(ndviFill(undefined)).toBeUndefined();
  });

  test("drought categories step from warm to red; none draws nothing", () => {
    expect(droughtFill({ category: "D0" })!.color).toBe(TONE.warn);
    expect(droughtFill({ category: "d2" })!.color).toBe(TONE.red);
    expect(droughtFill({ category: "D4" })!.opacity).toBeGreaterThan(droughtFill({ category: "D3" })!.opacity);
    expect(droughtFill({ category: null })).toBeUndefined();
    expect(droughtFill(undefined)).toBeUndefined();
  });

  test("floodplain hatch brightens with the forecast's flag", () => {
    expect(floodOpacity({ in_floodplain: true })).toBe(0.5);
    expect(floodOpacity({ in_floodplain: true, risk: "high" })).toBe(0.95);
    expect(floodOpacity({ in_floodplain: false, risk: "medium" })).toBe(0.75);
    expect(floodOpacity({ in_floodplain: false })).toBeUndefined();
  });

  test("a layer is offered only with data", () => {
    const openData = [{ paddock_id: "a", rest_days: 3 }, { paddock_id: "b" }];
    expect(hasData("rest", openData)).toBe(true);
    expect(hasData("ndvi", openData)).toBe(false);
    expect(hasData("drought", openData)).toBe(false);
    expect(hasData("flood", openData)).toBe(false);
    expect(hasData("rest", [{ paddock_id: "a", grazing: true }])).toBe(true);
    expect(hasData("rest", [])).toBe(false);
    expect(hasData("drought", [{ paddock_id: "a", drought: { category: null } }])).toBe(true);
    expect(hasData("flood", [{ paddock_id: "a", flood: { in_floodplain: false } }])).toBe(true);
  });
});
