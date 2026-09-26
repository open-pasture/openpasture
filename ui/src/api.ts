// Typed client for docs/API.md. Same origin: the Rust server serves this UI.

export type LonLat = [number, number];
export type Polygon = { type: "Polygon"; coordinates: LonLat[][] };

export type BrainId = "codex" | "claude" | "anthropic" | "openai" | "compatible" | "hosted" | "heuristic";
export type Species = "cattle" | "sheep" | "goats";
export type Autonomy = "propose" | "timer" | "auto";
export type CollarState = "inside" | "warning" | "outside" | "unknown";
export type SecretName =
  | "anthropic_api_key"
  | "openai_api_key"
  | "compatible_api_key"
  | "compatible_base_url"
  | "hosted_url"
  | "hosted_api_key"
  | "firecrawl_api_key";

export interface Farm { id: string; name: string; timezone: string; center: LonLat; created_at: string }
export interface Paddock {
  id: string; name: string; geometry: Polygon; area_ha: number;
  status: "resting" | "grazing" | "planned"; notes?: string; grazed_until?: string; created_at: string;
}
export interface Herd {
  id: string; name: string; species: Species; count: number; paddock_id?: string;
  autonomy: Autonomy; timer_minutes: number; created_at: string;
}
export interface Animal { id: string; tag: string; name?: string; herd_id: string; collar_id?: string }
export interface Settings {
  brain: { id: BrainId; model?: string };
  decision_time: string;
  server: { bind: string; port: number; public_url?: string; app_token: string };
  units: "metric" | "imperial";
}
export type SettingsPatch = {
  brain?: { id: BrainId; model?: string | null };
  decision_time?: string;
  server?: { bind?: string; port?: number; public_url?: string | null };
  units?: Settings["units"];
};
export interface AppState { farm: Farm | null; herds: Herd[]; paddocks: Paddock[]; settings: Settings }
export interface SecretStatus { name: SecretName; set: boolean }

export interface Fix { at: string; point: LonLat; accuracy_m: number; sats: number; cn0?: number; ttf_s?: number }
// battery is 0-1.
export interface Collar {
  id: string; name: string; herd_id: string; animal_id?: string; last_seen?: string;
  battery?: number; boundary_version?: number; state: CollarState; last_fix?: Fix;
}
export interface Position { collar_id: string; animal_id?: string; fix: Fix; state: CollarState }
export interface Boundary {
  id: string; herd_id: string; version: number; geometry: Polygon; warn_m: number; hysteresis_m: number;
  effective_at?: string; decision_id: string; created_at: string;
}
export type AckStatus = "received" | "applied" | "rejected";
export interface Ack { collar_id: string; version: number; status: AckStatus; reason?: string; at: string }
// A move toward a target. The collars hold the active boundary, which sweeps step by step
// until it is the target. Stragglers are collar ids dropped from the sweep.
export type MoveStatus = "sweeping" | "done" | "stopped";
export interface Move {
  id: string; herd_id: string; decision_id: string; target: Polygon; status: MoveStatus;
  step: number; remaining_m: number; stragglers: string[]; started_at: string; updated_at: string;
  // Unit east/north axis of the sweep, when the server sends it (absent for a gather).
  direction?: [number, number];
}
export interface BoundaryStatus {
  active?: Boundary; pending?: Boundary; proposed?: { decision_id: string; geometry: Polygon }; acks: Ack[];
  // The running move, or the last one for 10 min after it ends.
  move?: Move;
}
export interface NewCollar { collar: Collar; key: string; endpoint: string; public_key: string }

export type DecisionStatus = "running" | "proposed" | "approved" | "applied" | "rejected" | "failed" | "superseded";
export interface Decision {
  id: string; herd_id: string; source: "brain" | "farmer" | "heuristic"; brain?: BrainId; model?: string;
  status: DecisionStatus; action?: "STAY" | "MOVE" | "NEEDS_INFO"; to_paddock_id?: string; geometry?: Polygon;
  reasoning?: string; confidence?: number; need?: string; inputs: unknown; apply_at?: string; boundary_id?: string;
  error?: string; created_at: string; responded_at?: string; outcome?: unknown;
}
export interface Brain {
  id: BrainId; name: string; available: boolean; signed_in: boolean; needs: SecretName[]; models: string[]; detail?: string;
}
export interface BrainTest { ok: boolean; detail: string; ms: number }
export interface LandSummary {
  paddock_id: string; report_id: string; source: string; as_of: string; cached: boolean; summary: string[]; sections: Record<string, unknown>;
}
export interface Signals {
  as_of: string; herd_id: string; current_paddock_id: string | null; position_source: string; herd_animal_units: number | null;
  feed_budget_days_current: number | null; behavior: unknown; risk_flags: unknown[]; assumptions: unknown;
  paddocks: { paddock_id: string; name: string; status: Paddock["status"]; area_ha: number; current: boolean; rest_days: number | null;
    grazing_pressure: unknown; forage: unknown; recovery: unknown; risk_flags: unknown[] }[];
}
export interface KnowledgeHit { id: string; title: string; kind: string; body: string; source: string }

export interface Track { collar_id: string; points: [number, number, number][] }
// Analytics values are null when there is no data behind them.
export interface HealthPoint {
  t: string; fixes: number; fix_rate: number | null; acc_p50: number | null; acc_p95: number | null; sats: number | null;
  cn0?: number | null; ttf_s?: number | null; battery: number | null; cues: number;
}
export interface HealthSeries {
  collar_id: string; name: string; herd_id: string; animal_id: string | null; bucket_s: number; cadence_s: number | null;
  summary: HealthPoint; ack: { version: number; status: AckStatus; reason: string | null; at: string } | null; points: HealthPoint[];
}
export interface Behaviour {
  animal_id: string | null; collar_id: string; tag: string | null; name: string | null; fixes: number; distance_km: number;
  paddock_hours: Record<string, number>; outside_hours: number; days: string[]; cues_per_day: number[]; cues: number;
  learning: { slope_per_day: number; trend: "falling" | "rising" | "flat" } | null;
}
export interface PastureRow {
  paddock_id: string; name: string; area_ha: number; status: Paddock["status"]; grazing_days: number | null; rest_days: number | null;
  pressure: number | null; au_days: number | null; last_grazed: string | null; ndvi: number | null; herds: string[];
}
export interface SqlResult { columns: string[]; rows: unknown[][]; ms: number; truncated?: boolean }
export interface ServerInfo { version: string; data_dir: string; bind: string; port: number; lan_url?: string; public_url?: string }

export type LiveEvent =
  | { type: "fix"; collar_id: string; animal_id?: string; herd_id: string; fix: Fix; state: CollarState }
  | { type: "cue"; collar_id: string; at: string; level: number; margin_m: number }
  | { type: "ack"; collar_id: string; herd_id: string; version: number; status: AckStatus; reason?: string }
  | { type: "collar"; collar: Collar }
  | { type: "boundary"; herd_id: string; boundary: Boundary }
  | { type: "decision"; decision: Decision }
  | { type: "move"; move: Move }
  | { type: "decision_log"; decision_id: string; line: string }
  // The server dropped events for this socket; refetch everything.
  | { type: "resync" };

export interface HostedKey { id: string; label?: string; created_at: string; last_used?: string }
export interface NewHostedKey extends HostedKey { key: string }

function socket(path: string) {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const token = getToken();
  const q = token ? `?token=${encodeURIComponent(token)}` : "";
  return new WebSocket(`${proto}//${location.host}${path}${q}`);
}

// Off localhost the server wants the app token; the app asks for it on the first 401.
let unauthorized: () => void = () => {};
export const onUnauthorized = (f: () => void) => (unauthorized = f);

const TOKEN_KEY = "openpasture.token";
export const getToken = () => localStorage.getItem(TOKEN_KEY) ?? "";
export const authHeaders = (): Record<string, string> => {
  const t = getToken();
  return t ? { Authorization: `Bearer ${t}` } : {};
};
export const setToken = (t: string) => (t ? localStorage.setItem(TOKEN_KEY, t) : localStorage.removeItem(TOKEN_KEY));

export class ApiError extends Error {
  constructor(public status: number, message: string) {
    super(message);
  }
}

type Query = Record<string, string | number | undefined | null>;

function qs(q?: Query) {
  if (!q) return "";
  const p = new URLSearchParams();
  for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== null && v !== "") p.set(k, String(v));
  const s = p.toString();
  return s ? `?${s}` : "";
}

async function req<T>(method: string, path: string, body?: unknown, q?: Query): Promise<T> {
  const headers = authHeaders();
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const res = await fetch(path + qs(q), {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.status === 401) unauthorized();
  if (!res.ok) {
    let msg = res.statusText;
    try {
      msg = ((await res.json()) as { error?: string }).error ?? msg;
    } catch {
      /* not json */
    }
    throw new ApiError(res.status, msg);
  }
  if (res.status === 204) return undefined as T;
  const type = res.headers.get("Content-Type") ?? "";
  return (type.includes("json") ? res.json() : res.text()) as Promise<T>;
}

const get = <T>(p: string, q?: Query) => req<T>("GET", p, undefined, q);
const post = <T>(p: string, b?: unknown) => req<T>("POST", p, b ?? {});
const patch = <T>(p: string, b: unknown) => req<T>("PATCH", p, b);
const put = <T>(p: string, b: unknown) => req<T>("PUT", p, b);
const del = (p: string, q?: Query) => req<void>("DELETE", p, undefined, q);

export type Range = { from?: string; to?: string };

export const api = {
  // farm
  state: () => get<AppState>("/api/state"),
  createFarm: (b: { name: string; timezone: string; center: LonLat }) => post<Farm>("/api/farm", b),
  updateFarm: (b: Partial<Farm>) => patch<Farm>("/api/farm", b),
  paddocks: () => get<Paddock[]>("/api/paddocks"),
  createPaddock: (b: { name: string; geometry: Polygon }) => post<Paddock>("/api/paddocks", b),
  updatePaddock: (id: string, b: Partial<Paddock>) => patch<Paddock>(`/api/paddocks/${id}`, b),
  deletePaddock: (id: string) => del(`/api/paddocks/${id}`),
  herds: () => get<Herd[]>("/api/herds"),
  createHerd: (b: { name: string; species: Species; count: number; paddock_id?: string }) => post<Herd>("/api/herds", b),
  updateHerd: (id: string, b: Partial<Herd>) => patch<Herd>(`/api/herds/${id}`, b),
  deleteHerd: (id: string) => del(`/api/herds/${id}`),
  animals: () => get<Animal[]>("/api/animals"),
  createAnimal: (b: { tag: string; name?: string; herd_id: string; collar_id?: string }) => post<Animal>("/api/animals", b),
  updateAnimal: (id: string, b: Partial<Animal>) => patch<Animal>(`/api/animals/${id}`, b),
  deleteAnimal: (id: string) => del(`/api/animals/${id}`),
  settings: () => get<Settings>("/api/settings"),
  // JSON merge patch: null clears an optional field.
  updateSettings: (b: SettingsPatch) => put<Settings>("/api/settings", b),
  secrets: () => get<SecretStatus[]>("/api/secrets"),
  setSecret: (name: SecretName, value: string) => put<void>(`/api/secrets/${name}`, { value }),
  deleteSecret: (name: SecretName) => del(`/api/secrets/${name}`),

  // collars and boundaries
  collars: (herd_id?: string) => get<Collar[]>("/api/collars", { herd_id }),
  createCollar: (b: { name?: string; herd_id: string }) => post<NewCollar>("/api/collars", b),
  updateCollar: (id: string, b: Partial<Collar>) => patch<Collar>(`/api/collars/${id}`, b),
  deleteCollar: (id: string) => del(`/api/collars/${id}`),
  boundary: (herd_id: string) => get<BoundaryStatus>(`/api/herds/${herd_id}/boundary`),
  // The geometry is the move's target; the server sweeps the active boundary toward it.
  sendBoundary: (herd_id: string, b: { geometry: Polygon; warn_m?: number; hysteresis_m?: number; effective_at?: string }) =>
    post<Move>(`/api/herds/${herd_id}/boundary`, b),
  stopMove: (herd_id: string) => post<Move>(`/api/herds/${herd_id}/move/stop`),
  positions: (herd_id?: string) => get<Position[]>("/api/positions", { herd_id }),

  // decisions and brains
  decisions: (herd_id?: string, limit?: number) => get<Decision[]>("/api/decisions", { herd_id, limit }),
  decision: (id: string) => get<Decision>(`/api/decisions/${id}`),
  decide: (herd_id: string) => post<Decision>(`/api/herds/${herd_id}/decide`),
  respond: (id: string, b: { action: "approve" | "reject" | "modify"; geometry?: Polygon; note?: string }) =>
    post<Decision>(`/api/decisions/${id}/respond`, b),
  brains: () => get<Brain[]>("/api/brains"),
  testBrain: (id: BrainId) => post<BrainTest>(`/api/brains/${id}/test`),
  hostedKeys: () => get<HostedKey[]>("/api/brains/hosted/keys"),
  createHostedKey: (label?: string) => post<NewHostedKey>("/api/brains/hosted/keys", { label }),
  deleteHostedKey: (id: string) => del(`/api/brains/hosted/keys/${id}`),
  knowledge: (q: string, limit?: number) => get<KnowledgeHit[]>("/api/knowledge", { q, limit }),
  land: (paddock_id: string, refresh?: boolean) =>
    get<LandSummary>(`/api/land/${paddock_id}`, { refresh: refresh ? "true" : undefined }),
  signals: (herd_id?: string) => get<Signals>("/api/signals", { herd_id }),

  // analytics
  tracks: (q: { collar_id?: string; herd_id?: string; max_points?: number } & Range) => get<Track[]>("/api/tracks", q),
  health: (q: { herd_id?: string; collar_id?: string; bucket?: string } & Range) => get<HealthSeries[]>("/api/analytics/health", q),
  behaviour: (q: { herd_id: string } & Range) => get<Behaviour[]>("/api/analytics/behaviour", q),
  heatmap: (q: { herd_id: string } & Range) => get<[number, number, number][]>("/api/analytics/heatmap", q),
  pasture: (herd_id?: string) => get<PastureRow[]>("/api/analytics/pasture", { herd_id }),
  sql: (query: string) => post<SqlResult>("/api/sql", { query }),
  exportUrl: (table: string, format: "csv" | "geojson" | "parquet", r: Range = {}) =>
    "/api/export" + qs({ table, format, ...r }),

  // server
  server: () => get<ServerInfo>("/api/server"),
};

// Live feed with reconnect. Returns an unsubscribe function.
export function live(onEvent: (e: LiveEvent) => void, onStatus?: (up: boolean) => void): () => void {
  let ws: WebSocket | null = null;
  let closed = false;
  let delay = 500;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const connect = () => {
    ws = socket("/api/live");
    ws.onopen = () => {
      delay = 500;
      onStatus?.(true);
    };
    ws.onmessage = (m) => {
      try {
        onEvent(JSON.parse(typeof m.data === "string" ? m.data : "") as LiveEvent);
      } catch {
        /* ignore malformed */
      }
    };
    ws.onclose = () => {
      onStatus?.(false);
      if (closed) return;
      timer = setTimeout(connect, delay);
      delay = Math.min(delay * 2, 8000);
    };
    ws.onerror = () => ws?.close();
  };
  connect();
  return () => {
    closed = true;
    clearTimeout(timer);
    ws?.close();
  };
}
