import { describe, expect, test } from "bun:test";
import { type XY, chevrons, rearRun } from "./sweep-model";

describe("the lit back line", () => {
  test("is the side of a plain step that faces back", () => {
    // Counter-clockwise, 10 m apart; the herd walks east.
    const pts: XY[] = [];
    for (let x = 0; x < 100; x += 10) pts.push([x, 0]);
    for (let y = 0; y < 50; y += 10) pts.push([100, y]);
    for (let x = 100; x > 0; x -= 10) pts.push([x, 50]);
    for (let y = 50; y > 0; y -= 10) pts.push([0, y]);
    const run = rearRun(pts, [1, 0]).map((i) => pts[i]);
    expect(run[0]).toEqual([0, 50]);
    expect(run[run.length - 1]).toEqual([0, 0]);
    expect(run.every((p) => p[0] === 0)).toBe(true);
  });

  test("follows a back edge that zig-zags behind the animals at the back", () => {
    const pts: XY[] = [[0, 0], [100, 0], [100, 60], [0, 60], [10, 50], [0, 40], [10, 30], [0, 20], [10, 10]];
    expect(rearRun(pts, [1, 0]).map((i) => pts[i])).toEqual([[0, 60], [10, 50], [0, 40], [10, 30], [0, 20], [10, 10], [0, 0]]);
  });

  test("is nothing when no edge faces back", () => {
    expect(rearRun([[0, 0], [1, 0]], [1, 0])).toEqual([]);
  });
});

describe("chevrons", () => {
  test("two to five drift from the back line toward the target, none on a short run", () => {
    expect(chevrons(undefined, 0)).toEqual([]);
    expect(chevrons({ from: [0, 0], d: [10, 0], lat0: 42 }, 0)).toEqual([]);
    const far = chevrons({ from: [0, 0], d: [200, 0], lat0: 42 }, 1000);
    expect(far.length).toBe(5);
    const near = chevrons({ from: [0, 0], d: [30, 0], lat0: 42 }, 1000);
    expect(near.length).toBe(2);
    // They move on with time.
    const later = chevrons({ from: [0, 0], d: [200, 0], lat0: 42 }, 3000);
    expect(later[0].geometry).not.toEqual(far[0].geometry);
  });
});
