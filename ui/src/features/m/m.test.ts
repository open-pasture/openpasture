import { describe, expect, test } from "bun:test";
import type { Animal, AppState, Collar, Me } from "../../api";
import type { Store } from "../../store";
import { fmt } from "../../units";
import { afterLocateError, bearing, compass, metres, walkLine } from "./geo";
import { alertOfHash } from "./links";
import { decode, encode, KEY, newestFix, restore, signedInAs } from "./offline";
import { FLING, nextState, PEEK_MIN, settle, stops } from "./sheet-model";
import { keepAction } from "./subscribe";

test("at start a browser the server dropped subscribes again", () => {
  expect(keepAction(true, true)).toBe("keep");
  expect(keepAction(true, false)).toBe("renew");
  expect(keepAction(false, true)).toBe("rekey");
  expect(keepAction(false, false)).toBe("rekey");
});

describe("the you dot", () => {
  const at: [number, number] = [-93.62, 42.03];
  test("a timeout or a lost fix keeps following, with the last place", () => {
    for (const code of [2, 3]) {
      const { next, stop } = afterLocateError({ on: true, at, accuracy_m: 4 }, code);
      expect(stop).toBe(false);
      expect([next.on, next.at, next.accuracy_m]).toEqual([true, at, 4]);
      expect(next.error).toBe("Can't find where you are.");
    }
  });
  test("only a refusal stops it", () => {
    const { next, stop } = afterLocateError({ on: true, at }, 1);
    expect(stop).toBe(true);
    expect([next.on, next.at, next.error]).toEqual([false, undefined, "Location is off for this site."]);
  });
});

describe("the walk line", () => {
  // Around Ames: 0.001° of latitude is 111 m; of longitude, 83 m.
  const you: [number, number] = [-93.62, 42.03];
  test("distance and the eight compass points", () => {
    expect(metres(you, [-93.62, 42.031])).toBeCloseTo(111.2, 0);
    expect(metres(you, [-93.619, 42.03])).toBeCloseTo(82.7, 0);
    expect(compass(bearing(you, [-93.62, 42.031]))).toBe("N");
    expect(compass(bearing(you, [-93.619, 42.031]))).toBe("NE");
    expect(compass(bearing(you, [-93.619, 42.03]))).toBe("E");
    expect(compass(bearing(you, [-93.62, 42.029]))).toBe("S");
    expect(compass(bearing(you, [-93.621, 42.029]))).toBe("SW");
    expect(compass(bearing(you, [-93.621, 42.03]))).toBe("W");
    expect(compass(359)).toBe("N");
    expect(compass(-10)).toBe("N");
  });
  test("reads in the farm's units", () => {
    const at: [number, number] = [-93.6172, 42.0322];
    expect(walkLine("214", you, at, fmt("metric").len)).toBe("214  340 m NE");
    expect(walkLine("214", you, at, fmt("imperial").len)).toBe("214  1,100 ft NE");
    // Not located yet: the name alone. Beside it: here.
    expect(walkLine("214", undefined, at, fmt("metric").len)).toBe("214");
    expect(walkLine("214", you, [-93.62, 42.03002], fmt("metric").len)).toBe("214  here");
  });
});

describe("the sheet", () => {
  test("rests: the peek fits its content up to half the map", () => {
    expect(stops(800, 240)).toEqual({ peek: 240, half: 400, full: 792 });
    expect(stops(800, 40).peek).toBe(PEEK_MIN);
    expect(stops(800, 700).peek).toBe(400);
  });
  test("a drag settles on the nearest rest, a fling on the next", () => {
    const s = stops(800, 240);
    expect(settle(s, 300, 0)).toBe("peek");
    expect(settle(s, 380, 0)).toBe("half");
    expect(settle(s, 700, 0)).toBe("full");
    expect(settle(s, 260, FLING)).toBe("half");
    expect(settle(s, 420, FLING)).toBe("full");
    expect(settle(s, 780, -FLING)).toBe("half");
    expect(settle(s, 380, -FLING)).toBe("peek");
    expect(settle(s, 792, FLING)).toBe("full");
  });
  test("the grab steps peek, half, full, peek", () => {
    expect([nextState("peek"), nextState("half"), nextState("full")]).toEqual(["half", "full", "peek"]);
  });
});

describe("the offline copy", () => {
  const collar = (id: string, at?: string): Collar => ({ id, name: id, herd_id: "h1", state: "inside", ...(at ? { last_fix: { at, point: [-93.62, 42.03], accuracy_m: 3, sats: 9 } } : {}) });
  const state: AppState = {
    farm: { id: "farm_1", name: "Test farm", timezone: "America/Chicago", center: [-93.62, 42.03], created_at: "2026-09-01T00:00:00Z" } as AppState["farm"],
    herds: [{ id: "h1", name: "Cows" } as AppState["herds"][number], { id: "h2", name: "Heifers" } as AppState["herds"][number]],
    paddocks: [],
    settings: { brain: { id: "heuristic" }, decision_time: "06:00", server: { bind: "127.0.0.1", port: 7878, app_token: "secret-token" }, units: "imperial" } as AppState["settings"],
  };
  const me: Me = { role: "hand", via: "user_token", user: { id: "usr_1", name: "Hank" } };
  const s = { ready: true, needToken: false, up: true, state, herdId: "h2", collars: [collar("c1", "2026-09-27T12:00:00Z"), collar("c2", "2026-09-27T12:05:00Z"), collar("c3")],
    animals: [{ id: "a1", tag: "214", herd_id: "h1" } as Animal], boundary: { h1: {} as never, h2: { acks: [] } as never }, decisions: [], logs: {} } as Store;

  test("keeps the farm without the app token, and one herd's boundary", () => {
    const snap = encode(s, me, new Date("2026-09-27T12:06:00Z"), "a1")!;
    expect(snap.state.settings.server.app_token).toBe("");
    expect(s.state!.settings.server.app_token).toBe("secret-token");
    expect(Object.keys(snap.boundary)).toEqual(["h2"]);
    expect(encode({ ...s, state: null }, me, new Date(), "")).toBeUndefined();
    expect(encode(s, null, new Date(), "")).toBeUndefined();
    expect(KEY).toBe("openpasture.last");
  });
  test("reads back only what it wrote", () => {
    const snap = encode(s, me, new Date("2026-09-27T12:06:00Z"), "a1")!;
    const back = decode(JSON.stringify(snap), "a1")!;
    expect(back.me.user?.name).toBe("Hank");
    const r = restore(back);
    expect([r.herdId, r.up, r.collars?.length, r.decisions]).toEqual(["h2", false, 3, []]);
    expect(restore({ ...back, herdId: "gone" }).herdId).toBe("h1");
    for (const bad of [null, "", "{", JSON.stringify({ ...snap, v: 1 }), JSON.stringify({ ...snap, state: { ...snap.state, farm: null } }), JSON.stringify({ ...snap, me: {} })])
      expect(decode(bad, "a1")).toBeUndefined();
  });
  test("opens only for the sign-in that kept it", () => {
    const [a, b] = [signedInAs("opu_aaaa"), signedInAs("opu_bbbb")];
    expect(a).not.toBe(b);
    expect(signedInAs("")).toBe("");
    const raw = JSON.stringify(encode(s, me, new Date("2026-09-27T12:06:00Z"), a));
    expect(decode(raw, a)?.me.user?.name).toBe("Hank");
    // Signed out (no token) or someone else signed in: the last person's farm stays shut.
    expect(decode(raw, "")).toBeUndefined();
    expect(decode(raw, b)).toBeUndefined();
  });
  test("the age shown is the newest fix's", () => {
    expect(newestFix(s.collars)).toBe(Date.parse("2026-09-27T12:05:00Z"));
    expect(newestFix([collar("x")])).toBeUndefined();
  });
});

test("a notification's link names its alert", () => {
  expect(alertOfHash("#/map?alert=alr_01ABC")).toBe("alr_01ABC");
  expect(alertOfHash("#/map")).toBeUndefined();
  expect(alertOfHash("#/herd?alert=alr_1")).toBeUndefined();
  expect(alertOfHash("#/map?select=c1")).toBeUndefined();
});
