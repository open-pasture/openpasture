import { describe, expect, test } from "bun:test";
import type { Ack, Boundary, BoundaryStatus, Polygon, SlotCount } from "../api";
import { ackLine, fenced, nextStaged } from "./boundary";

const square = (x: number): Polygon => ({ type: "Polygon", coordinates: [[[x, 0], [x + 1, 0], [x + 1, 1], [x, 1], [x, 0]]] });
const bnd = (version: number, effective_at?: string): Boundary => ({
  id: `bnd${version}`, herd_id: "h1", version, geometry: square(version), warn_m: 10, hysteresis_m: 2, effective_at, decision_id: "d", created_at: "2026-09-27T12:00:00Z",
});
const ack = (collar_id: string, version: number, status: Ack["status"]): Ack => ({ collar_id, version, status, at: "2026-09-27T12:00:00Z" });
const slot = (version: number, applied: number, stored: number, effective_at?: string): SlotCount => ({ version, effective_at, applied, stored, rejected: 0, collars: 4 });
const four = new Set(["c1", "c2", "c3", "c4"]);

// A daily strip schedule: v9 in effect, v10 (Mon 07:00, the next open) to v24 (Fri, the last
// back-fence step) staged. Every collar applied v9 and stored the lot, so each one's last ack
// is `received v24`.
const staged = Array.from({ length: 15 }, (_, i) => bnd(10 + i, new Date(Date.parse("2026-09-28T12:00:00Z") + i * 6 * 3600_000).toISOString()));
const schedule: BoundaryStatus = {
  active: bnd(9), pending: staged[staged.length - 1], staged,
  acks: ["c1", "c2", "c3", "c4"].map((c) => ack(c, 24, "received")),
  slots: [slot(9, 4, 0), ...staged.map((b) => slot(b.version, 0, 4, b.effective_at))],
};

describe("a herd's boundaries on the map and in the panel", () => {
  test("while a schedule runs the dashed line is the next move, not the last one staged", () => {
    expect(nextStaged(schedule)?.version).toBe(10);
  });

  test("while a schedule runs the ack line counts the boundary in effect, which every collar applied", () => {
    expect(ackLine(schedule, four)).toEqual({ version: 9, n: 4, of: 4, held: false });
  });

  test("acks for the boundary in effect count as they arrive, ahead of the slot counts", () => {
    const b: BoundaryStatus = { active: bnd(5), acks: [ack("c1", 5, "applied"), ack("c2", 5, "applied"), ack("c3", 5, "applied"), ack("c4", 4, "applied")], slots: [slot(5, 1, 0)] };
    expect(ackLine(b, four)).toEqual({ version: 5, n: 3, of: 4, held: false });
  });

  test("collars off the list (parked, removed) don't count", () => {
    const b: BoundaryStatus = { active: bnd(5), acks: ["c1", "c2", "c3", "c4", "c5"].map((c) => ack(c, 5, "applied")), slots: [slot(5, 5, 0)] };
    expect(ackLine(b, new Set(["c1", "c2"]))).toEqual({ version: 5, n: 2, of: 2, held: false });
  });

  test("a first boundary sent for later: the line is it, and who holds it", () => {
    const later = bnd(1, "2026-09-28T12:00:00Z");
    const b: BoundaryStatus = { pending: later, staged: [later], acks: [ack("c1", 1, "received"), ack("c2", 1, "received")], slots: [slot(1, 0, 2)] };
    expect(ackLine(b, four)).toEqual({ version: 1, n: 2, of: 4, held: true });
    expect(nextStaged(b)?.version).toBe(1);
  });

  test("a server without the staged list: pending is the next", () => {
    const b: BoundaryStatus = { active: bnd(3), pending: bnd(4, "2026-09-28T12:00:00Z"), acks: [] };
    expect(nextStaged(b)?.version).toBe(4);
    expect(ackLine(b, four)).toEqual({ version: 3, n: 0, of: 4, held: false });
  });

  test("nothing staged, no dashed line", () => {
    expect(nextStaged({ active: bnd(3), acks: [] })).toBeUndefined();
    expect(ackLine(undefined, four)).toBeUndefined();
    expect(ackLine({ active: bnd(3), acks: [] }, new Set())).toBeUndefined();
  });

  test("a schedule can start once the herd is fenced, not before and not from a boundary only staged", () => {
    const later = bnd(1, "2026-09-28T12:00:00Z");
    expect(fenced(undefined)).toBe(false);
    expect(fenced({ acks: [] })).toBe(false);
    expect(fenced({ pending: later, staged: [later], acks: [] })).toBe(false);
    expect(fenced(schedule)).toBe(true);
  });
});
