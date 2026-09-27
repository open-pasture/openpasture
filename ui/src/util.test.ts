import { describe, expect, test } from "bun:test";
import { ApiError } from "./api/http";
import { attempt, perKey } from "./util";

describe("a form's request", () => {
  test("a refused save gives the server's sentence and never rejects", async () => {
    const seen: (string | undefined)[] = [];
    const busy: boolean[] = [];
    const ok = await attempt(() => Promise.reject(new ApiError(400, "The shape crosses itself.")), { busy: (b) => busy.push(b), failed: (m) => seen.push(m) });
    expect(ok).toBe(false);
    expect(seen).toEqual([undefined, "The shape crosses itself."]);
    expect(busy).toEqual([true, false]);
  });

  test("a save that goes through clears the last sentence", async () => {
    const seen: (string | undefined)[] = [];
    let ran = false;
    expect(await attempt(async () => void (ran = true), { failed: (m) => seen.push(m) })).toBe(true);
    expect(ran).toBe(true);
    expect(seen).toEqual([undefined]);
  });
});

describe("timers per key", () => {
  test("a call for one herd doesn't drop another herd's pending call", async () => {
    const later = perKey<string>();
    const ran: string[] = [];
    later.set("cows", 5, () => ran.push("cows"));
    later.set("heifers", 5, () => ran.push("heifers"));
    // A burst for one key is one call, the last.
    later.set("cows", 5, () => ran.push("cows again"));
    await Bun.sleep(30);
    expect(ran.sort()).toEqual(["cows again", "heifers"]);
  });
});
