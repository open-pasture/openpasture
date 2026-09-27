import { describe, expect, test } from "bun:test";
import type { Decision } from "../../api";
import { detail, outcome, responder, sentence, since, source } from "./timeline";

const names: Record<string, string> = { p1: "P1", p2: "P2" };
const name = (id?: string) => (id ? names[id] : undefined);
const TZ = "America/Chicago";

function d(over: Partial<Decision>): Decision {
  return { id: "dec_1", herd_id: "herd_1", source: "brain", status: "proposed", inputs: {}, created_at: "2026-09-27T11:02:00Z", ...over };
}

describe("sentence", () => {
  test("moves, stays and questions", () => {
    expect(sentence(d({ action: "MOVE", to_paddock_id: "p2" }), name)).toBe("Move to P2.");
    expect(sentence(d({ action: "MOVE" }), name)).toBe("Move to a new boundary.");
    expect(sentence(d({ action: "STAY", inputs: { from_paddock_id: "p1" } }), name)).toBe("Stay in P1.");
    expect(sentence(d({ action: "STAY" }), name)).toBe("Stay in place.");
    expect(sentence(d({ action: "NEEDS_INFO", need: "How tall is the grass in P2?" }), name)).toBe("How tall is the grass in P2?");
    expect(sentence(d({ action: "NEEDS_INFO" }), name)).toBe("Needs more information.");
    expect(sentence(d({ status: "running" }), name)).toBe("Deciding.");
    expect(sentence(d({ status: "failed", error: "Brain timed out." }), name)).toBe("No decision.");
  });
});

describe("source", () => {
  test("brain and model, heuristic, farmer", () => {
    expect(source(d({ brain: "claude", model: "sonnet" }))).toBe("claude sonnet");
    expect(source(d({ brain: "codex" }))).toBe("codex");
    expect(source(d({ source: "heuristic", brain: "heuristic" }))).toBe("heuristic");
    expect(source(d({ source: "farmer" }))).toBe("farmer");
  });
});

describe("responder", () => {
  test("by text, with the person's name and the farm's time", () => {
    const r = d({
      status: "applied",
      inputs: { farmer_response: { action: "approve", at: "2026-09-27T11:42:00Z", by: { via: "text", user_id: "usr_1", name: "Cody" } } },
    });
    expect(responder(r, TZ)).toBe("by text, Cody 06:42");
  });

  test("in the app: the person, or the owner without one", () => {
    const person = d({ inputs: { farmer_response: { at: "2026-09-27T12:10:00Z", by: { via: "user_token", name: "Ana" } } } });
    expect(responder(person, TZ)).toBe("by Ana 07:10");
    const owner = d({ inputs: { farmer_response: { at: "2026-09-27T11:55:00Z", by: { via: "local" } } } });
    expect(responder(owner, TZ)).toBe("by owner 06:55");
    // Answered before who-answered was recorded: the time alone.
    const old = d({ responded_at: "2026-09-27T11:55:00Z", inputs: { farmer_response: { action: "reject" } } });
    expect(responder(old, TZ)).toBe("answered 06:55");
  });

  test("sent by the herd's timer or auto, and nothing while waiting", () => {
    expect(responder(d({ status: "applied", apply_at: "2026-09-27T12:40:00Z" }), TZ)).toBe("on timer 07:40");
    expect(responder(d({ status: "applied", apply_at: "2026-09-27T11:02:05Z" }), TZ)).toBe("on auto 06:02");
    expect(responder(d({ status: "proposed", apply_at: "2026-09-27T12:40:00Z" }), TZ)).toBeUndefined();
    expect(responder(d({}), TZ)).toBeUndefined();
  });
});

describe("outcome and detail", () => {
  test("held, cues", () => {
    expect(outcome(d({ outcome: { herd_held_boundary: true, cue_count: 3, notes: [] } }))).toBe("held, 3 cues");
    expect(outcome(d({ outcome: { herd_held_boundary: false, cue_count: 1 } }))).toBe("not held, 1 cue");
    expect(outcome(d({ outcome: { herd_held_boundary: null, cue_count: null } }))).toBeUndefined();
    expect(outcome(d({}))).toBeUndefined();
  });

  test("one line", () => {
    const x = d({
      status: "applied", brain: "claude", action: "MOVE", to_paddock_id: "p2",
      inputs: { farmer_response: { at: "2026-09-27T11:42:00Z", by: { via: "text", name: "Cody" } } },
      outcome: { herd_held_boundary: true, cue_count: 3 },
    });
    expect(detail(x, TZ)).toBe("claude  by text, Cody 06:42  held, 3 cues");
    expect(detail(d({ source: "farmer" }), TZ)).toBe("farmer");
  });

  test("since the range's start, newer ones included", () => {
    const list = [d({ id: "c", created_at: "2026-09-27T12:00:00Z" }), d({ id: "b", created_at: "2026-09-26T12:00:00Z" }), d({ id: "a", created_at: "2026-09-20T12:00:00Z" })];
    expect(since(list, "2026-09-26T00:00:00Z").map((x) => x.id)).toEqual(["c", "b"]);
  });
});
