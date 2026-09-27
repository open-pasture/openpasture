// HTTP plumbing every API module builds on. Same origin: the Rust server serves this UI.
// Streams write their calls in ui/src/api/<id>.ts with these helpers.

export type Query = Record<string, string | number | boolean | undefined | null>;

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
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

// Bodies: FormData and Blob go as they are (multipart, files), a string as text unless
// `headers` names a type (e.g. text/csv), anything else as JSON.
export async function req<T>(method: string, path: string, body?: unknown, q?: Query, headers: Record<string, string> = {}): Promise<T> {
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
  const res = await fetch(path + qs(q), { method, headers: h, body: payload });
  if (res.status === 401) unauthorized();
  if (!res.ok) throw new ApiError(res.status, await errorText(res));
  if (res.status === 204) return undefined as T;
  const type = res.headers.get("Content-Type") ?? "";
  return (type.includes("json") ? res.json() : res.text()) as Promise<T>;
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
  const res = await fetch(path + qs(q), init ? { method: init.method, headers, body: JSON.stringify(init.body) } : { headers });
  if (res.status === 401) unauthorized();
  if (!res.ok) throw new ApiError(res.status, await errorText(res));
  const url = URL.createObjectURL(await res.blob());
  const a = document.createElement("a");
  a.href = url;
  a.download = filename;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
