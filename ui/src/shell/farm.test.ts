import { expect, test } from "bun:test";
import type { Decision } from "../api";
import { visitsOf } from "./Farm";

const move = (id: string, pad: string, at: string, status: Decision["status"] = "applied"): Decision => ({
  id, herd_id: "h", source: "heuristic", status, action: "MOVE", to_paddock_id: pad, inputs: {}, created_at: at, responded_at: at,
});
const now = Date.parse("2026-10-01T12:00:00Z");

test("a stay runs from the move into a paddock to the next move, the last one to now", () => {
  const v = visitsOf([move("b", "p2", "2026-09-25T12:00:00Z"), move("a", "p1", "2026-09-20T12:00:00Z")], now);
  expect(v).toEqual([
    { pad: "p1", from: Date.parse("2026-09-20T12:00:00Z"), to: Date.parse("2026-09-25T12:00:00Z") },
    { pad: "p2", from: Date.parse("2026-09-25T12:00:00Z"), to: now },
  ]);
});

test("only moves that went ahead count; a move into the same paddock carries the stay on", () => {
  const v = visitsOf([
    move("a", "p1", "2026-09-20T12:00:00Z"), move("b", "p1", "2026-09-22T12:00:00Z"),
    move("c", "p3", "2026-09-24T12:00:00Z", "rejected"), move("a", "p1", "2026-09-20T12:00:00Z"),
  ], now);
  expect(v).toEqual([{ pad: "p1", from: Date.parse("2026-09-20T12:00:00Z"), to: now }]);
});

test("stays that ended more than thirty days ago drop off the strip", () => {
  expect(visitsOf([move("a", "p1", "2026-08-01T12:00:00Z"), move("b", "p2", "2026-08-10T12:00:00Z")], now).map((v) => v.pad)).toEqual(["p2"]);
});
