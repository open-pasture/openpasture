import { describe, expect, test } from "bun:test";
import { ApiError } from "../../api/http";
import { byLine, confirmable, heldBy, who } from "./model";

describe("who and when", () => {
  test("a person by name, the owner by token or this machine", () => {
    expect(who({ via: "user_token", name: "Ana" })).toBe("Ana");
    expect(who({ via: "app_token" })).toBe("the owner");
    expect(who({ via: "local" })).toBe("the owner");
    expect(who({ via: "text" })).toBe("someone");
  });
  test("a changed rule says who changed it", () => {
    const r = { created_by: { via: "local" as const, name: "Cody" }, created_at: "2026-10-26T15:00:00Z", updated_at: "2026-10-26T15:00:00Z" };
    expect(byLine(r, "America/Chicago")).toBe("Cody · Oct 26");
    expect(byLine({ ...r, updated_by: { via: "user_token", name: "Ana" }, updated_at: "2026-10-27T15:00:00Z" }, "America/Chicago")).toBe("changed by Ana · Oct 27");
  });
});

describe("what the gate held", () => {
  const refusal = (body: Record<string, unknown>, status = 409) => {
    const e = new ApiError(status, String(body.error));
    e.body = body;
    return e;
  };
  test("a road to confirm", () => {
    const h = heldBy(refusal({ error: "Crosses County Road.", findings: [{ code: "crosses_road", severity: "critical", text: "Crosses County Road" }], confirm: ["crosses_road"], never: [], confirm_token: "7:abc" }));
    expect(h?.confirm).toEqual(["crosses_road"]);
    expect(h?.token).toBe("7:abc");
    expect(h && confirmable(h)).toBe(true);
  });
  test("a no-go zone nobody sends into", () => {
    const h = heldBy(refusal({ error: "Takes in Creek.", findings: [{ code: "enters_no_go", severity: "critical", text: "Takes in Creek" }], confirm: [], never: ["enters_no_go"] }));
    expect(h && confirmable(h)).toBe(false);
  });
  test("any other error is not the gate's", () => {
    expect(heldBy(refusal({ error: "This decision is applied now." }))).toBeUndefined();
    expect(heldBy(refusal({ error: "x", findings: [] }, 400))).toBeUndefined();
    expect(heldBy(new Error("x"))).toBeUndefined();
  });
});
