import { describe, expect, test } from "bun:test";
import { perKey } from "./util";

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
