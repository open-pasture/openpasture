import { describe, expect, test } from "bun:test";
import { compareValues, filterRows, nextSort, sortRows, toggleAll, toggleSelect, windowRange } from "./rows";

describe("windowing", () => {
  test("renders the rows in view plus overscan", () => {
    // 250 rows of 37 px in a 370 px box, scrolled to row 100.
    const [start, end] = windowRange(100 * 37, 370, 37, 250, 8);
    expect(start).toBe(92);
    expect(end).toBe(100 + 11 + 8);
  });

  test("clamps at both ends", () => {
    expect(windowRange(0, 370, 37, 250, 8)).toEqual([0, 19]);
    expect(windowRange(250 * 37, 370, 37, 250, 8)).toEqual([241, 250]);
    expect(windowRange(0, 370, 37, 0)).toEqual([0, 0]);
    expect(windowRange(0, 370, 37, 5)).toEqual([0, 5]);
  });

  test("never more than a screen and the overscan, however long the list", () => {
    const [s, e] = windowRange(5000 * 37, 900, 37, 25000, 8);
    expect(e - s).toBeLessThanOrEqual(Math.ceil(900 / 37) + 1 + 16);
  });
});

describe("filter", () => {
  const rows = [{ tag: "214", breed: "Angus" }, { tag: "031", breed: "Hereford" }, { tag: "118", breed: "Angus cross" }];
  const text = (r: (typeof rows)[number]) => `${r.tag} ${r.breed}`;
  test("every word must appear, any case", () => {
    expect(filterRows(rows, "angus", text).map((r) => r.tag)).toEqual(["214", "118"]);
    expect(filterRows(rows, "ANGUS cross", text).map((r) => r.tag)).toEqual(["118"]);
    expect(filterRows(rows, "  ", text)).toHaveLength(3);
    expect(filterRows(rows, "zebu", text)).toHaveLength(0);
  });
});

describe("sort", () => {
  test("ascending, descending, then off", () => {
    expect(nextSort(undefined, "tag")).toEqual({ col: "tag", dir: "asc" });
    expect(nextSort({ col: "tag", dir: "asc" }, "tag")).toEqual({ col: "tag", dir: "desc" });
    expect(nextSort({ col: "tag", dir: "desc" }, "tag")).toBeUndefined();
    expect(nextSort({ col: "tag", dir: "desc" }, "breed")).toEqual({ col: "breed", dir: "asc" });
  });

  test("natural order with empties last, stable on ties", () => {
    const rows = [{ t: "P10", i: 0 }, { t: "P9", i: 1 }, { t: "", i: 2 }, { t: "P9", i: 3 }];
    const cmp = (a: (typeof rows)[number], b: (typeof rows)[number]) => compareValues(a.t, b.t);
    expect(sortRows(rows, cmp, "asc").map((r) => r.i)).toEqual([1, 3, 0, 2]);
    expect(sortRows(rows, cmp, "desc").map((r) => r.i)).toEqual([2, 0, 1, 3]);
    expect(compareValues(2, 10)).toBeLessThan(0);
  });
});

describe("selection", () => {
  const order = ["a", "b", "c", "d", "e"];
  test("a click toggles one row", () => {
    const s = toggleSelect(new Set(), "b", order, false);
    expect([...s]).toEqual(["b"]);
    expect([...toggleSelect(s, "b", order, false)]).toEqual([]);
  });

  test("shift-click sets the run from the last click", () => {
    const s = toggleSelect(new Set(["b"]), "d", order, true, "b");
    expect([...s].sort()).toEqual(["b", "c", "d"]);
    // Shift-clicking a selected row clears the run back to the anchor.
    expect([...toggleSelect(s, "c", order, true, "d")].sort()).toEqual(["b"]);
  });

  test("the header check selects the shown rows, or clears them", () => {
    const s = toggleAll(new Set(["z"]), ["a", "b"]);
    expect([...s].sort()).toEqual(["a", "b", "z"]);
    expect([...toggleAll(s, ["a", "b"])]).toEqual(["z"]);
  });
});
