import { expect, test } from "bun:test";
import { pointAt, positionsAt } from "./replay";

test("pointAt finds the first point at or after a time, else the last", () => {
  const pts: [number, number, number][] = [[1, 1, 10], [2, 2, 20], [3, 3, 30]];
  expect(pointAt(pts, 0)).toEqual([1, 1, 10]);
  expect(pointAt(pts, 10)).toEqual([1, 1, 10]);
  expect(pointAt(pts, 11)).toEqual([2, 2, 20]);
  expect(pointAt(pts, 30)).toEqual([3, 3, 30]);
  expect(pointAt(pts, 99)).toEqual([3, 3, 30]);
  expect(pointAt([], 5)).toBeUndefined();
});

test("pointAt agrees with a scan on random tracks", () => {
  let seed = 7;
  const rnd = () => ((seed = (seed * 1103515245 + 12345) % 2 ** 31) / 2 ** 31);
  for (let k = 0; k < 200; k++) {
    let t = 0;
    const pts: [number, number, number][] = Array.from({ length: 1 + Math.floor(rnd() * 300) }, (_, i) => [i, i, (t += Math.floor(rnd() * 3))]);
    const q = rnd() * (t + 5) - 2;
    expect(pointAt(pts, q)).toEqual(pts.find((p) => p[2] >= q) ?? pts[pts.length - 1]);
  }
});

test("positionsAt places every track's animal", () => {
  const tracks = [
    { collar_id: "a", points: [[1, 2, 10], [3, 4, 20]] as [number, number, number][] },
    { collar_id: "b", points: [] as [number, number, number][] },
  ];
  expect(positionsAt(tracks, 15)).toEqual([{ id: "a", point: [3, 4], state: "inside" }]);
});
