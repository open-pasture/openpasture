import { expect, test } from "bun:test";
import type { Alert, Decision } from "../api";
import { asksOf, decisionAnswers, decisionAsk, minutesToCall, watched } from "./asks";

const alert = (id: string, extra: Partial<Alert> = {}): Alert => ({
  id, kind: "silent", key: id, severity: "warning", status: "open", herd_id: "h1", title: id, targets: [], data: null,
  opened_at: "2026-10-01T10:00:00Z", updated_at: "2026-10-01T10:00:00Z", ...extra,
});
const decision = (extra: Partial<Decision> = {}): Decision => ({
  id: "d1", herd_id: "h1", source: "heuristic", status: "proposed", action: "MOVE", to_paddock_id: "p2", inputs: {}, created_at: "2026-10-01T06:00:00Z", ...extra,
});

test("a critical alert comes before a waiting decision, which comes before warnings", () => {
  const asks = asksOf([decision()], [alert("warn"), alert("crit", { severity: "critical" })], "h1");
  expect(asks.map((a) => a.id)).toEqual(["crit", "d1", "warn"]);
});

test("a decision on a timer goes ahead by itself, so it ranks under a waiting one's place", () => {
  const timed = asksOf([decision({ apply_at: "2026-10-01T07:00:00Z" })], [alert("warn")], "h1");
  expect(timed.map((a) => a.id)).toEqual(["d1", "warn"]);
  expect(timed[0].rank).toBeLessThan(asksOf([decision()], [], "h1")[0].rank);
});

test("acked alerts aren't asks; they are watched", () => {
  const list = [alert("a", { status: "acked" }), alert("b")];
  expect(asksOf([], list, "h1").map((a) => a.id)).toEqual(["b"]);
  expect(watched(list, "h1").map((a) => a.id)).toEqual(["a"]);
});

test("this herd's decision comes from its record; another herd's from its decision_waiting alert", () => {
  const list = [alert("mine", { kind: "decision_waiting" }), alert("theirs", { kind: "decision_waiting", herd_id: "h2" })];
  const asks = asksOf([decision()], list, "h1");
  expect(asks.map((a) => a.id)).toEqual(["d1", "theirs"]);
  // Only proposed decisions wait.
  expect(asksOf([decision({ status: "applied" })], [], "h1")).toEqual([]);
});

test("decisions read as questions with their own answers", () => {
  const pad = (id?: string) => ({ p1: "P1", p2: "P2" })[id ?? ""];
  expect(decisionAsk(decision(), "Herd 1", pad)).toBe("Move Herd 1 to P2?");
  expect(decisionAsk(decision({ action: "STAY", to_paddock_id: undefined }), "Herd 1", pad, "p1")).toBe("Keep Herd 1 in P1?");
  expect(decisionAsk(decision({ action: "NEEDS_INFO", need: "How is P1 looking?" }), "Herd 1", pad)).toBe("How is P1 looking?");
  expect(decisionAnswers(decision())).toEqual({ yes: "Move", no: "Not today" });
  expect(decisionAnswers(decision({ apply_at: "x" }))).toEqual({ yes: "Move now", no: "Hold" });
});

test("the next daily call is minutes away in the farm's zone, never zero", () => {
  const at = Date.parse("2026-10-01T15:00:00Z"); // 10:00 in Chicago (CDT)
  expect(minutesToCall("06:00", "America/Chicago", at)).toBe(20 * 60);
  expect(minutesToCall("10:30", "America/Chicago", at)).toBe(30);
  expect(minutesToCall("10:00", "America/Chicago", at)).toBe(1440);
  expect(minutesToCall("nope", "America/Chicago", at)).toBeUndefined();
});
