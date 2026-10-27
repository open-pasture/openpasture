import { afterEach, beforeEach, expect, test } from "bun:test";
import { BACKOFF_MAX_MS, BACKOFF_MIN_MS, backoff, live, protocols } from "./live";

test("backoff doubles to 8 s, jittered within the upper half of each step", () => {
  expect(backoff(0, () => 0)).toBe(BACKOFF_MIN_MS / 2);
  expect(backoff(0, () => 1)).toBe(BACKOFF_MIN_MS);
  expect(backoff(3, () => 0.5)).toBe(3000);
  expect(backoff(50, () => 1)).toBe(BACKOFF_MAX_MS);
  for (let n = 0; n < 12; n++) {
    const d = backoff(n);
    expect(d).toBeGreaterThanOrEqual(BACKOFF_MIN_MS / 2);
    expect(d).toBeLessThanOrEqual(BACKOFF_MAX_MS);
  }
});

test("the token rides as a second subprotocol, never in the URL", () => {
  expect(protocols("")).toEqual(["openpasture"]);
  expect(protocols("abc")).toEqual(["openpasture", "openpasture.token.abc"]);
});

// A socket the test drives.
class FakeSocket {
  static CONNECTING = 0;
  static OPEN = 1;
  static all: FakeSocket[] = [];
  readyState = FakeSocket.CONNECTING;
  onopen?: () => void;
  onmessage?: (m: { data: string }) => void;
  onclose?: () => void;
  onerror?: () => void;
  constructor(public url: string, public protocols: string[]) {
    FakeSocket.all.push(this);
  }
  close() {
    this.readyState = 3;
    this.onclose?.();
  }
  open() {
    this.readyState = FakeSocket.OPEN;
    this.onopen?.();
  }
  send(msg: unknown) {
    this.onmessage?.({ data: JSON.stringify(msg) });
  }
}

const g = globalThis as Record<string, unknown>;
const saved: Record<string, unknown> = {};
const listeners: Record<string, (() => void)[]> = {};
beforeEach(() => {
  FakeSocket.all = [];
  for (const k of ["WebSocket", "location", "window", "document", "localStorage"]) saved[k] = g[k];
  g.WebSocket = FakeSocket;
  g.location = { protocol: "http:", host: "127.0.0.1:7878" };
  const on = (k: string, f: () => void) => (listeners[k] ??= []).push(f);
  g.window = { addEventListener: on, removeEventListener() {} };
  g.document = { visibilityState: "visible", addEventListener: on, removeEventListener() {} };
  g.localStorage = { getItem: () => "tok", setItem() {}, removeItem() {} };
});
afterEach(() => {
  for (const k of Object.keys(saved)) g[k] = saved[k];
});

test("pings keep it alive but aren't events; a batch arrives as its events", () => {
  const got: string[] = [];
  const ups: boolean[] = [];
  const stop = live((e) => got.push(e.type), (up) => ups.push(up));
  const s = FakeSocket.all[0];
  expect(s.url).toBe("ws://127.0.0.1:7878/api/live");
  expect(s.protocols).toEqual(["openpasture", "openpasture.token.tok"]);
  s.open();
  s.send({ type: "ping" });
  s.send({ type: "batch", events: [{ type: "resync" }, { type: "ping" }, { type: "resync" }] });
  expect(got).toEqual(["resync", "resync"]);
  stop();
  expect(ups).toEqual([true]);
});

test("coming back online reconnects at once instead of waiting out the backoff", async () => {
  const stop = live(() => {});
  FakeSocket.all[0].close(); // dropped: a reconnect is scheduled with backoff
  for (const f of listeners.online ?? []) f();
  await new Promise((r) => setTimeout(r, 5));
  expect(FakeSocket.all.length).toBe(2);
  stop();
});
