// HTTP plumbing every API module builds on. Same origin: the Rust server serves this UI.
// Streams write their calls in ui/src/api/<id>.ts with these helpers.
//
// Every request goes through one scheduler, in a lane. A browser opens six connections to a
// plain-http server and the live socket holds one, so slow reads (behaviour at 250 collars
// takes ~20 s) must not take them all: *Stop the move* has to get through.
//   act        writes: never queued, so one connection is always left for them
//   core       the store's own reads: at most 2 at once
//   view       a view's reads: at most 1
//   analytics  analytics, tracks, SQL: at most 1
// A GET is a view read unless its path is an analytics one, anything else is an act. The store
// marks its reads core with scoped(). Each lane has a timeout; a caller's AbortSignal (scoped,
// or useReads in store/reads.ts) cancels a read queued or in flight.

export type Query = Record<string, string | number | boolean | undefined | null>;

// status 0: no answer (unreachable, or too slow). `body`: the error's JSON, which may say more
// than `error` (the safety gate's findings, for one).
export class ApiError extends Error {
  body?: Record<string, unknown>;
  constructor(public status: number, message: string, cause?: unknown) {
    super(message, cause === undefined ? undefined : { cause });
  }
}

// Off localhost the server wants a token; the app asks for it on the first 401.
let unauthorized: () => void = () => {};
export const onUnauthorized = (f: () => void) => (unauthorized = f);

const TOKEN_KEY = "openpasture.token";
export const getToken = () => localStorage.getItem(TOKEN_KEY) ?? "";
export const setToken = (t: string) => (t ? localStorage.setItem(TOKEN_KEY, t) : localStorage.removeItem(TOKEN_KEY));
export const authHeaders = (): Record<string, string> => {
  const t = getToken();
  return t ? { Authorization: `Bearer ${t}` } : {};
};

// "?a=1&b=x", skipping undefined, null and empty values; "" when nothing is left.
export function qs(q?: Query) {
  if (!q) return "";
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== null && v !== "") p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

export type Lane = "act" | "core" | "view" | "analytics";
export const LIMIT: Record<Lane, number> = { act: Infinity, core: 2, view: 1, analytics: 1 };
export const TIMEOUT_MS: Record<Lane, number> = { act: 30_000, core: 15_000, view: 30_000, analytics: 60_000 };
// A core read or write still unanswered after this marks the connection slow (see onSlow).
export const SLOW_MS = 5_000;

const ANALYTICS = ["/api/analytics/", "/api/tracks", "/api/sql", "/api/export"];
export function laneOf(method: string, path: string): Lane {
  if (method !== "GET" && method !== "HEAD") return path === "/api/sql" ? "analytics" : "act";
  return ANALYTICS.some((p) => path.startsWith(p)) ? "analytics" : "view";
}

// What scoped() set for the requests started inside it.
interface Scope { lane?: Lane; signal?: AbortSignal; timeoutMs?: number }
let ambient: Scope = {};

// Run f with every request it starts (synchronously) in this lane and/or cancelled by signal.
export function scoped<T>(scope: Scope, f: () => T): T {
  const prev = ambient;
  ambient = { ...prev, ...scope };
  try {
    return f();
  } finally {
    ambient = prev;
  }
}

// Thrown when a read was cancelled by its caller; callers drop it quietly.
export class Aborted extends Error {
  constructor() {
    super("Cancelled.");
  }
}
export const isAbort = (e: unknown) => e instanceof Aborted || (e instanceof DOMException && e.name === "AbortError");

const running: Record<Lane, number> = { act: 0, core: 0, view: 0, analytics: 0 };
const waiting: Record<Lane, (() => void)[]> = { act: [], core: [], view: [], analytics: [] };

// A slot in the lane, or Aborted if the signal fires while waiting.
function slot(lane: Lane, signal?: AbortSignal): Promise<void> {
  if (signal?.aborted) return Promise.reject(new Aborted());
  if (running[lane] < LIMIT[lane]) {
    running[lane]++;
    return Promise.resolve();
  }
  return new Promise((resolve, reject) => {
    const go = () => {
      signal?.removeEventListener("abort", drop);
      running[lane]++;
      resolve();
    };
    const drop = () => {
      const q = waiting[lane];
      const i = q.indexOf(go);
      if (i >= 0) q.splice(i, 1);
      reject(new Aborted());
    };
    signal?.addEventListener("abort", drop, { once: true });
    waiting[lane].push(go);
  });
}

function release(lane: Lane) {
  running[lane]--;
  waiting[lane].shift()?.();
}

// Requests a farmer waits on (writes, the store's reads) unanswered past SLOW_MS.
let slowCount = 0;
const slowSubs = new Set<(slow: boolean) => void>();
export function onSlow(f: (slow: boolean) => void) {
  slowSubs.add(f);
  return () => void slowSubs.delete(f);
}
function markSlow(delta: 1 | -1) {
  const was = slowCount > 0;
  slowCount += delta;
  if (was !== slowCount > 0) slowSubs.forEach((f) => f(slowCount > 0));
}

// One fetch in its lane, with the lane's timeout and the caller's signal.
async function send(lane: Lane, url: string, init: RequestInit, scope: Scope): Promise<Response> {
  await slot(lane, scope.signal);
  const ac = new AbortController();
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    ac.abort();
  }, scope.timeoutMs ?? TIMEOUT_MS[lane]);
  const watched = lane === "act" || lane === "core";
  let slow = false;
  const slowTimer = watched
    ? setTimeout(() => {
        slow = true;
        markSlow(1);
      }, SLOW_MS)
    : undefined;
  const cancel = () => ac.abort();
  scope.signal?.addEventListener("abort", cancel, { once: true });
  try {
    return await fetch(url, { ...init, signal: ac.signal });
  } catch (e) {
    if (timedOut) throw new ApiError(0, "The server took too long to answer.");
    if (scope.signal?.aborted) throw new Aborted();
    throw new ApiError(0, "Couldn't reach the server.", e);
  } finally {
    clearTimeout(timer);
    clearTimeout(slowTimer);
    if (slow) markSlow(-1);
    scope.signal?.removeEventListener("abort", cancel);
    release(lane);
  }
}

// Bodies: FormData and Blob go as they are (multipart, files), a string as text unless
// `headers` names a type (e.g. text/csv), anything else as JSON.
export async function req<T>(method: string, path: string, body?: unknown, q?: Query, headers: Record<string, string> = {}): Promise<T> {
  const scope = ambient;
  // Writes are always acts (or SQL's analytics); reads take the scope's lane when it sets one.
  const write = method !== "GET" && method !== "HEAD";
  const lane = write || !scope.lane ? laneOf(method, path) : scope.lane;
  const h: Record<string, string> = { ...authHeaders(), ...headers };
  let payload: BodyInit | undefined;
  if (body === undefined) payload = undefined;
  else if (body instanceof FormData) payload = body;
  else if (body instanceof Blob) {
    payload = body;
    if (body.type && !h["Content-Type"]) h["Content-Type"] = body.type;
  } else if (typeof body === "string") {
    payload = body;
    h["Content-Type"] ??= "text/plain";
  } else {
    payload = JSON.stringify(body);
    h["Content-Type"] ??= "application/json";
  }
  const res = await send(lane, path + qs(q), { method, headers: h, body: payload }, scope);
  if (res.status === 401) unauthorized();
  if (!res.ok) throw await failure(res);
  if (res.status === 204) return undefined as T;
  const type = res.headers.get("Content-Type") ?? "";
  return (type.includes("json") ? res.json() : res.text()) as Promise<T>;
}

async function failure(res: Response) {
  let body: Record<string, unknown> | undefined;
  try {
    body = (await res.json()) as Record<string, unknown>;
  } catch {
    body = undefined;
  }
  const e = new ApiError(res.status, typeof body?.error === "string" ? body.error : res.statusText);
  e.body = body;
  return e;
}

async function errorText(res: Response) {
  try {
    return ((await res.json()) as { error?: string }).error ?? res.statusText;
  } catch {
    return res.statusText;
  }
}

export const get = <T>(p: string, q?: Query) => req<T>("GET", p, undefined, q);
export const post = <T>(p: string, b?: unknown, q?: Query) => req<T>("POST", p, b ?? {}, q);
export const patch = <T>(p: string, b: unknown) => req<T>("PATCH", p, b);
export const put = <T>(p: string, b: unknown) => req<T>("PUT", p, b);
export const del = (p: string, q?: Query) => req<void>("DELETE", p, undefined, q);

// Fetch a file with the app's auth and hand it to the browser as a download.
// `init` makes it a POST (keys and codes go in bodies, never in URLs).
export async function downloadBlob(path: string, filename: string, q?: Query, init?: { method: "POST"; body: unknown }) {
  const headers = authHeaders();
  if (init) headers["Content-Type"] = "application/json";
  const res = await send(init ? "act" : "analytics", path + qs(q), init ? { method: init.method, headers, body: JSON.stringify(init.body) } : { headers }, { timeoutMs: 300_000 });
  if (res.status === 401) unauthorized();
  if (!res.ok) throw new ApiError(res.status, await errorText(res));
  const url = URL.createObjectURL(await res.blob());
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
