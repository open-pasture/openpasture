import { expect, test } from "bun:test";
import { closed, hashOf, pruned, visited, type Tab } from "./tabs";

const t = (view: string, rest = ""): Tab => ({ view, rest });

test("a view opens a tab at the end, and moving inside it updates that tab", () => {
  const a = visited([], "map", "");
  expect(a).toEqual([t("map")]);
  const b = visited(a, "herd", "");
  expect(b).toEqual([t("map"), t("herd")]);
  const c = visited(b, "herd", "104");
  expect(c).toEqual([t("map"), t("herd", "104")]);
  // Nothing changed: the same list, so nothing re-renders or rewrites storage.
  expect(visited(c, "herd", "104")).toBe(c);
});

test("closing the showing tab shows the next, else the previous; the last tab stays", () => {
  const list = [t("map"), t("herd", "104"), t("data")];
  expect(closed(list, "herd", "herd")).toEqual({ list: [t("map"), t("data")], show: t("data") });
  expect(closed(list, "data", "data")).toEqual({ list: [t("map"), t("herd", "104")], show: t("herd", "104") });
  // Closing another tab leaves the one showing.
  expect(closed(list, "map", "data")).toEqual({ list: [t("herd", "104"), t("data")], show: undefined });
  expect(closed([t("map")], "map", "map")).toEqual({ list: [t("map")] });
  expect(closed(list, "settings", "map").list).toBe(list);
});

test("tabs for views that went away drop out", () => {
  const list = [t("map"), t("settings"), t("herd")];
  expect(pruned(list, (v) => v !== "settings")).toEqual([t("map"), t("herd")]);
  expect(pruned(list, () => true)).toBe(list);
});

test("a tab's hash puts back where it was, a query straight after the view", () => {
  expect(hashOf(t("map"))).toBe("#/map");
  expect(hashOf(t("herd", "104"))).toBe("#/herd/104");
  expect(hashOf(t("herd", "?select=a,b"))).toBe("#/herd?select=a,b");
});
