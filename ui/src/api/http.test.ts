import { afterEach, beforeEach, expect, test } from "bun:test";
import { Aborted, ApiError, get, laneOf, onSlow, post, scoped } from "./http";

// A fetch whose answers the test hands out one by one.
type Pending = { url: string; method: string; signal?: AbortSignal; answer: (status?: number, body?: unknown) => void };
let pending: Pending[] = [];
const realFetch = globalThis.fetch;

beforeEach(() => {
  pending = [];
  (globalThis as { localStorage?: unknown }).localStorage ??= { getItem: () => null, setItem() {}, removeItem() {} };
  globalThis.fetch = ((url: string, init: RequestInit = {}) =>
    new Promise<Response>((resolve, reject) => {
      const signal = init.signal ?? undefined;
      signal?.addEventListener("abort", () => reject(new DOMException("aborted", "AbortError")));
      pending.push({
        url, method: init.method ?? "GET", signal,
        answer: (status = 200, body = {}) => resolve(new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } })),
      });
    })) as typeof fetch;
});
afterEach(() => {
  globalThis.fetch = realFetch;
});

const tick = () => new Promise((r) => setTimeout(r, 0));

test("reads are laned by path, writes are acts", () => {
  expect(laneOf("GET", "/api/analytics/behaviour")).toBe("analytics");
  expect(laneOf("GET", "/api/tracks")).toBe("analytics");
  expect(laneOf("POST", "/api/sql")).toBe("analytics");
  expect(laneOf("GET", "/api/collars")).toBe("view");
  expect(laneOf("POST", "/api/herds/h/move/stop")).toBe("act");
  expect(laneOf("DELETE", "/api/collars/c")).toBe("act");
});

test("a heavy read waits its turn, and a write never waits behind it", async () => {
  const a = get("/api/analytics/behaviour");
  const b = get("/api/analytics/pasture");
  await tick();
  expect(pending.map((p) => p.url)).toEqual(["/api/analytics/behaviour"]);
  const stop = post("/api/herds/h/move/stop");
  await tick();
  expect(pending.map((p) => p.url)).toEqual(["/api/analytics/behaviour", "/api/herds/h/move/stop"]);
  pending[1].answer(200, { status: "stopped" });
  expect(await stop).toEqual({ status: "stopped" });
  pending[0].answer(200, [1]);
  expect(await a).toEqual([1]);
  await tick();
  expect(pending.at(-1)!.url).toBe("/api/analytics/pasture");
  pending.at(-1)!.answer(200, [2]);
  expect(await b).toEqual([2]);
});

test("a scope's signal drops a queued read and cancels one in flight", async () => {
  const ac = new AbortController();
  const first = scoped({ signal: ac.signal }, () => get("/api/one")).catch((e) => e);
  const second = scoped({ signal: ac.signal }, () => get("/api/two")).catch((e) => e);
  await tick();
  expect(pending.length).toBe(1);
  ac.abort();
  expect(await first).toBeInstanceOf(Aborted);
  expect(await second).toBeInstanceOf(Aborted);
  await tick();
  // The lane is free again.
  const third = get("/api/three");
  await tick();
  expect(pending.at(-1)!.url).toBe("/api/three");
  pending.at(-1)!.answer(200, 3);
  expect(await third).toBe(3);
});

test("the store's lane holds two at once, and a write ignores the scope's lane", async () => {
  scoped({ lane: "core" }, () => {
    void get("/api/state");
    void get("/api/me");
    void get("/api/collars");
    void post("/api/collars", { herd_id: "h" });
  });
  await tick();
  expect(pending.map((p) => `${p.method} ${p.url}`)).toEqual(["GET /api/state", "GET /api/me", "POST /api/collars"]);
  for (const p of pending) p.answer();
  await tick();
  await tick();
  expect(pending.at(-1)!.url).toBe("/api/collars");
  pending.at(-1)!.answer();
});

test("no answer in time is an ApiError with status 0, and a slow store read says so", async () => {
  const marks: boolean[] = [];
  const off = onSlow((s) => marks.push(s));
  const r = scoped({ lane: "core", timeoutMs: 20 }, () => get("/api/state"));
  const err = await r.catch((e) => e);
  expect(err).toBeInstanceOf(ApiError);
  expect((err as ApiError).status).toBe(0);
  off();
  expect(marks).toEqual([]); // too short to count as slow
});

// Astra 6, 03 P1-02: headers came at once and bodies never finished, yet each request gave its
// slot back at the headers: 8 fetches ran in a lane of 1, none timed out, none could be cancelled.
type Stalled = { url: string; signal?: AbortSignal; finish: (body: unknown) => void };
function stallingFetch(started: Stalled[]) {
  return ((url: string, init: RequestInit = {}) => {
    const signal = init.signal ?? undefined;
    let finish: (body: unknown) => void = () => {};
    const body = new ReadableStream<Uint8Array>({
      start(c) {
        finish = (b) => {
          c.enqueue(new TextEncoder().encode(JSON.stringify(b)));
          c.close();
        };
        signal?.addEventListener("abort", () => c.error(new DOMException("aborted", "AbortError")));
      },
    });
    started.push({ url, signal, finish: (b) => finish(b) });
    return Promise.resolve(new Response(body, { status: 200, headers: { "Content-Type": "application/json" } }));
  }) as typeof fetch;
}

test("a stalled body keeps its slot, then times out and frees the lane", async () => {
  const started: Stalled[] = [];
  globalThis.fetch = stallingFetch(started);
  const reads = Array.from({ length: 8 }, (_, i) => scoped({ timeoutMs: 15 }, () => get(`/api/r${i}`)).catch((e) => e));
  await new Promise((r) => setTimeout(r, 5));
  expect(started.length).toBe(1); // the view lane holds one, body or no body
  const first = await reads[0];
  expect(first).toBeInstanceOf(ApiError);
  expect((first as ApiError).status).toBe(0);
  expect(started[0].signal?.aborted).toBe(true); // the stalled body was cut off
  await tick();
  expect(started.length).toBe(2); // and the next one started only then
  started[1].finish({ ok: 2 });
  expect(await reads[1]).toEqual({ ok: 2 });
  for (const r of reads.slice(2)) expect(await r).toBeInstanceOf(ApiError);
});

test("cancelling a read whose body is still coming aborts it and frees the lane", async () => {
  const started: Stalled[] = [];
  globalThis.fetch = stallingFetch(started);
  const ac = new AbortController();
  const r = scoped({ signal: ac.signal }, () => get("/api/slow")).catch((e) => e);
  await tick();
  const next = get("/api/next");
  await tick();
  expect(started.map((s) => s.url)).toEqual(["/api/slow"]);
  ac.abort();
  expect(await r).toBeInstanceOf(Aborted);
  expect(started[0].signal?.aborted).toBe(true);
  await tick();
  expect(started.map((s) => s.url)).toEqual(["/api/slow", "/api/next"]);
  started[1].finish(1);
  expect(await next).toBe(1);
});

test("a store read stalled in its body still shows slow", async () => {
  const started: Stalled[] = [];
  globalThis.fetch = stallingFetch(started);
  const marks: boolean[] = [];
  const off = onSlow((s) => marks.push(s));
  const prev = (globalThis as { setTimeout: typeof setTimeout }).setTimeout;
  // SLOW_MS is 5 s; run the clock fast for this one.
  (globalThis as { setTimeout: unknown }).setTimeout = ((f: () => void, ms?: number) => prev(f, ms === 5_000 ? 5 : ms)) as typeof setTimeout;
  try {
    const r = scoped({ lane: "core", timeoutMs: 60_000 }, () => get("/api/state")).catch((e) => e);
    await new Promise((res) => prev(res, 30));
    expect(marks).toEqual([true]);
    started[0].finish({ ok: true });
    await r;
    expect(marks).toEqual([true, false]);
  } finally {
    (globalThis as { setTimeout: unknown }).setTimeout = prev;
    off();
  }
});
