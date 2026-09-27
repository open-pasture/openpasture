import { describe, expect, test } from "bun:test";
import type { LonLat } from "../api";
import { AnimalsModel, EASE_MS, FRAME_MS, TRAIL_ALL_UP_TO, frameDue, hull } from "./animals-model";

const p = (i: number, d = 0): LonLat => [-93.62 + i * 1e-4 + d, 42.03];
const herd = (n: number, d = 0) => Array.from({ length: n }, (_, i) => ({ id: `c${i}`, point: p(i, d), state: "inside" as const }));

describe("the animals source gets diffs, not rebuilds", () => {
  test("new animals are added once, where they are", () => {
    const m = new AnimalsModel();
    m.set(herd(3), 0, true);
    const { diff, moving } = m.frame(10);
    expect(diff?.add.map((f) => f.properties.id)).toEqual(["c0", "c1", "c2"]);
    expect(diff?.add[1].geometry.coordinates).toEqual(p(1));
    expect(diff?.update).toEqual([]);
    expect(moving).toBe(false);
    expect(m.frame(20).diff).toBeNull();
  });

  test("250 animals moving is 250 point updates each frame until they arrive, then nothing", () => {
    const m = new AnimalsModel();
    m.set(herd(250), 0, true);
    m.frame(1);
    m.set(herd(250, 1e-4), 1000, true);
    const mid = m.frame(1000 + EASE_MS / 2);
    expect(mid.moving).toBe(true);
    expect(mid.diff?.add).toEqual([]);
    expect(mid.diff?.update.length).toBe(250);
    expect(mid.diff?.update.every((u) => u.newGeometry && !u.addOrUpdateProperties)).toBe(true);
    const x = mid.diff!.update[0].newGeometry!.coordinates[0];
    expect(x).toBeGreaterThan(p(0)[0]);
    expect(x).toBeLessThan(p(0, 1e-4)[0]);
    const end = m.frame(1000 + EASE_MS + 1);
    expect(end.moving).toBe(false);
    expect(end.diff?.update[0].newGeometry?.coordinates).toEqual(p(0, 1e-4));
    expect(m.frame(3000).diff).toBeNull();
  });

  test("without easing (zoomed out, hidden tab, scrubbing) a square is at its new point at once", () => {
    const m = new AnimalsModel();
    m.set(herd(1), 0, false);
    m.frame(1);
    m.move("c0", p(0, 5e-4), "inside", 100, false);
    const f = m.frame(101);
    expect(f.moving).toBe(false);
    expect(f.diff?.update).toEqual([{ id: "c0", newGeometry: { type: "Point", coordinates: p(0, 5e-4) } }]);
  });

  test("a state or straggler change updates only that property", () => {
    const m = new AnimalsModel();
    m.set(herd(2), 0, true);
    m.frame(1);
    m.move("c1", p(1), "outside", 10, true);
    expect(m.frame(11).diff?.update).toEqual([{ id: "c1", addOrUpdateProperties: [{ key: "state", value: "outside" }] }]);
    m.setLag(["c0"]);
    expect(m.frame(12).diff?.update).toEqual([{ id: "c0", addOrUpdateProperties: [{ key: "lag", value: true }] }]);
    m.setLag([]);
    expect(m.frame(13).diff?.update).toEqual([{ id: "c0", addOrUpdateProperties: [{ key: "lag", value: false }] }]);
  });

  test("animals no longer drawn are removed", () => {
    const m = new AnimalsModel();
    m.set(herd(3), 0, true);
    m.frame(1);
    m.set(herd(3).slice(1), 5, true);
    expect(m.frame(6).diff).toEqual({ add: [], remove: ["c0"], update: [] });
    expect(m.size).toBe(2);
  });
});

describe("trails", () => {
  test("rebuilt only when a new fix arrives or who has one changes", () => {
    const m = new AnimalsModel();
    m.showTrails(true);
    m.set(herd(3), 0, true);
    expect(m.trails(0)).toEqual([]); // one point each: nothing to draw yet
    expect(m.trails(10)).toBeNull();
    m.frame(100);
    expect(m.trails(200)).toBeNull(); // frames alone rebuild nothing
    m.move("c0", p(0, 1e-4), "inside", 1000, true);
    const t = m.trails(1000);
    expect(t?.length).toBe(1);
    expect(t?.[0].path.at(-1)).toEqual(p(0, 1e-4));
    expect(m.trails(1100)).toBeNull();
  });

  test(`above ${TRAIL_ALL_UP_TO} animals only stragglers and focused animals have trails`, () => {
    const m = new AnimalsModel();
    m.showTrails(true);
    m.set(herd(60), 0, true);
    m.set(herd(60, 1e-4), 1000, true);
    expect(m.trails(1000)).toEqual([]);
    m.setLag(["c3"]);
    m.setFocus(["c7"]);
    expect(m.trails(1001)?.map((t) => t.lag).sort()).toEqual([false, true]);
    const small = new AnimalsModel();
    small.showTrails(true);
    small.set(herd(10), 0, true);
    small.set(herd(10, 1e-4), 1000, true);
    expect(small.trails(1000)?.length).toBe(10);
  });
});

describe("herd outline", () => {
  test("is the convex hull of each herd", () => {
    const ring = hull([[0, 0], [2, 0], [2, 2], [0, 2], [1, 1], [1, 0.5]]);
    expect(ring).toEqual([[0, 0], [2, 0], [2, 2], [0, 2]]);
    const m = new AnimalsModel();
    const corners: LonLat[] = [[0, 0], [1, 0], [1, 1], [0, 1]];
    m.set([...corners.map((point, i) => ({ id: `c${i}`, point, state: "inside" as const, herd: "h1" })), { id: "x", point: [5, 5] as LonLat, state: "inside" as const, herd: "h2" }], 0, true);
    const out = m.outlines();
    expect(out?.map((o) => o.herd)).toEqual(["h1"]); // h2 has one animal: no outline
    expect(out?.[0].ring).toEqual([[0, 0], [1, 0], [1, 1], [0, 1], [0, 0]]);
    expect(m.outlines()).toBeNull();
  });
});

test("frames are capped at 30 a second", () => {
  expect(frameDue(0, FRAME_MS)).toBe(true);
  expect(frameDue(0, 16.7)).toBe(false);
  let frames = 0;
  let last = -Infinity;
  for (let t = 0; t < 1000; t += 1000 / 120) if (frameDue(last, t)) {
    frames++;
    last = t;
  }
  expect(frames).toBeGreaterThanOrEqual(29);
  expect(frames).toBeLessThanOrEqual(31);
});
