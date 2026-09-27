import { describe, expect, test } from "bun:test";
import type { LonLat } from "../../api";
import { offset } from "../../geo";
import { caught, closeRing, thin } from "./lasso";

const o: LonLat = [-93.625, 42.03];
const at = (x: number, y: number) => offset(o, x, y);

describe("lasso", () => {
  test("a dragged path thins to points a few metres apart", () => {
    const path = Array.from({ length: 101 }, (_, i) => at(i * 0.5, 0));
    const kept = thin(path, 1.9);
    expect(kept.length).toBe(26);
    expect(kept[0]).toEqual(path[0]);
  });

  test("a path closes into a ring; too few points enclose nothing", () => {
    const ring = closeRing([at(0, 0), at(50, 0), at(50, 50)])!;
    expect(ring.length).toBe(4);
    expect(ring[3]).toEqual(ring[0]);
    expect(closeRing([at(0, 0), at(50, 0)])).toBeUndefined();
    expect(closeRing([at(0, 0), at(0, 0), at(5, 5)])).toBeUndefined();
  });

  test("it catches the animals inside, in the order given", () => {
    const ring = closeRing([at(0, 0), at(100, 0), at(100, 100), at(0, 100)])!;
    const animals: [string, LonLat][] = [
      ["col_a", at(10, 10)],
      ["col_out", at(150, 10)],
      ["col_b", at(90, 90)],
      ["col_edge_out", at(50, -1)],
    ];
    expect(caught(ring, animals)).toEqual(["col_a", "col_b"]);
    expect(caught(ring, [])).toEqual([]);
  });

  test("twelve of 250 scattered animals", () => {
    const animals: [string, LonLat][] = Array.from({ length: 250 }, (_, i) => [`col_${i}`, at((i % 25) * 20, Math.floor(i / 25) * 20)]);
    // Around the 3 x 4 block of animals at x 0-40 m, y 0-60 m.
    const ring = closeRing([at(-5, -5), at(45, -5), at(45, 65), at(-5, 65)])!;
    expect(caught(ring, animals).length).toBe(12);
  });
});
