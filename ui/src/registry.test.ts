import { describe, expect, test } from "bun:test";
import { claimKey, createRegistry, keyOwner, type Section } from "./registry";

describe("registries", () => {
  test("a duplicate id throws", () => {
    const r = createRegistry<{ id: string; order: number }>("test-dupe");
    r.register({ id: "a", order: 1 });
    expect(() => r.register({ id: "a", order: 2 })).toThrow(/registered twice/);
  });

  test("a key claimed twice throws in dev", () => {
    const a = createRegistry<{ id: string; order: number; key: string }>("test-keys-a");
    const b = createRegistry<{ id: string; order: number; key: string }>("test-keys-b");
    a.register({ id: "one", order: 1, key: "q" });
    expect(keyOwner("q")).toBe("test-keys-a one");
    expect(() => b.register({ id: "two", order: 1, key: "q" })).toThrow(/q key/);
    expect(b.has("two")).toBe(false);
  });

  test("Escape belongs to the app", () => {
    expect(() => claimKey("Escape", "someone")).toThrow();
  });

  test("items list by order, then registration", () => {
    const r = createRegistry<{ id: string; order: number }>("test-order");
    r.register({ id: "late", order: 50 });
    r.register({ id: "first", order: 10 });
    r.register({ id: "tie", order: 50 });
    expect(r.list().map((i) => i.id)).toEqual(["first", "late", "tie"]);
  });

  test("items below the role are hidden, and nothing before the role is known", () => {
    const r = createRegistry<Section<object>>("test-roles");
    const S = () => null;
    r.register({ id: "all", order: 1, Section: S });
    r.register({ id: "hand", order: 2, minRole: "hand", Section: S });
    r.register({ id: "owner", order: 3, minRole: "owner", Section: S });
    expect(r.visible(undefined)).toEqual([]);
    expect(r.visible("viewer").map((i) => i.id)).toEqual(["all"]);
    expect(r.visible("manager").map((i) => i.id)).toEqual(["all", "hand"]);
    expect(r.visible("owner").map((i) => i.id)).toEqual(["all", "hand", "owner"]);
  });

  test("subscribers hear registrations and changed()", () => {
    const r = createRegistry<{ id: string }>("test-subs");
    let n = 0;
    const off = r.subscribe(() => n++);
    r.register({ id: "x" });
    r.changed();
    off();
    r.changed();
    expect(n).toBe(2);
  });
});
