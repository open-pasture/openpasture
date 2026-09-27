import { expect, test } from "bun:test";
import type { Animal, Collar, PositionItem } from "../api";
import { applyAcks, applyCollar, applyPositions, drawable, indexById, labels, summarize, undrawn } from "./live";

const fix = (at: string, x = 0) => ({ at, point: [-93.62 + x, 42.03] as [number, number], accuracy_m: 3, sats: 9 });
const collar = (id: string, extra: Partial<Collar> = {}): Collar => ({ id, name: `n${id}`, herd_id: "h", state: "inside", ...extra });

test("a positions batch updates each collar by id in one pass", () => {
  const collars = [collar("a", { last_fix: fix("2026-09-27T10:00:00Z") }), collar("b"), collar("c", { battery: 0.5 })];
  const items: PositionItem[] = [
    { collar_id: "b", fix: fix("2026-09-27T10:00:05Z", 1e-4), state: "warning", battery: 0.8, last_seen: "2026-09-27T10:00:06Z" },
    // Older than what the collar has: telemetry yes, position no.
    { collar_id: "a", fix: fix("2026-09-27T09:59:00Z", 1e-3), state: "outside", battery: 0.4 },
    { collar_id: "zz", fix: fix("2026-09-27T10:00:00Z"), state: "inside" },
  ];
  const next = applyPositions(collars, indexById(collars), items);
  expect(next).not.toBe(collars);
  expect(next[1]).toMatchObject({ state: "warning", battery: 0.8, last_seen: "2026-09-27T10:00:06Z", last_fix: items[0].fix });
  expect(next[0]).toMatchObject({ state: "inside", battery: 0.4, last_fix: collars[0].last_fix });
  expect(next[2]).toBe(collars[2]);
  expect(applyPositions(collars, indexById(collars), [])).toBe(collars);
});

test("an ack batch replaces each collar's ack", () => {
  const acks = [{ collar_id: "a", version: 3, status: "applied" as const, at: "x" }, { collar_id: "b", version: 3, status: "applied" as const, at: "x" }];
  const next = applyAcks(acks, [{ collar_id: "b", version: 4, status: "received" }, { collar_id: "c", version: 4, status: "rejected", reason: "too many vertices" }], "now");
  expect(next).toEqual([
    { collar_id: "a", version: 3, status: "applied", at: "x" },
    { collar_id: "b", version: 4, status: "received", reason: undefined, at: "now" },
    { collar_id: "c", version: 4, status: "rejected", reason: "too many vertices", at: "now" },
  ]);
});

test("labels, what the map draws, and the big-herd summary", () => {
  const animals: Animal[] = [
    { id: "an1", tag: "214", herd_id: "h", collar_id: "a" },
    { id: "an2", tag: "031", herd_id: "h", collar_id: "b", removed_at: "2026-09-01T00:00:00Z" },
  ];
  const collars = [
    collar("a", { animal_id: "an1", last_fix: fix("t"), state: "outside", battery: 0.1 }),
    collar("b", { last_fix: fix("t") }),
    collar("c", { last_fix: fix("t"), parked_at: "2026-09-02T00:00:00Z", state: "warning" }),
    collar("d"),
  ];
  expect([...labels(collars, animals)]).toEqual([["a", "214"], ["b", "031"], ["c", "nc"], ["d", "nd"]]);
  expect([...undrawn(collars, animals)].sort()).toEqual(["b", "c"]);
  expect(drawable(collars, animals).map((c) => c.id)).toEqual(["a"]);
  expect(summarize(collars)).toEqual({ total: 4, outside: ["a"], warning: ["c"], low: ["a"] });
});

test("a collar event replaces the row, so an unparked or unlinked collar comes back to the map", () => {
  const animals: Animal[] = [{ id: "an1", tag: "214", herd_id: "h", collar_id: "a" }];
  const parked = collar("a", { animal_id: "an1", last_fix: fix("2026-09-27T10:00:00Z"), parked_at: "2026-09-27T10:01:00Z", parked_reason: "charging", state: "unknown" });
  expect(drawable([parked], animals)).toEqual([]);
  // The server leaves cleared fields out: no parked_at, no animal_id.
  const back = applyCollar(parked, collar("a", { last_fix: fix("2026-09-27T10:00:00Z"), state: "unknown" }));
  expect(back.parked_at).toBeUndefined();
  expect(back.parked_reason).toBeUndefined();
  expect(back.animal_id).toBeUndefined();
  expect(drawable([back], animals).map((c) => c.id)).toEqual(["a"]);
  // A newer fix from a positions batch isn't taken back by an older event.
  const moved = collar("a", { last_fix: fix("2026-09-27T10:05:00Z", 1e-4), state: "warning", battery: 0.9 });
  const late = applyCollar(moved, collar("a", { name: "renamed", last_fix: fix("2026-09-27T10:04:00Z"), state: "inside", battery: 0.8 }));
  expect(late).toMatchObject({ name: "renamed", last_fix: moved.last_fix, state: "warning", battery: 0.8 });
  // A newer fix on the event wins; a collar with no fix yet takes the event whole.
  expect(applyCollar(moved, collar("a", { last_fix: fix("2026-09-27T10:06:00Z") })).last_fix?.at).toBe("2026-09-27T10:06:00Z");
  expect(applyCollar(undefined, parked)).toBe(parked);
});
