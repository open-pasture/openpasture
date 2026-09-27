// Typed client for docs/API.md. Same origin: the Rust server serves this UI.
// HTTP plumbing lives in ./api/http; each stream adds its own ./api/<id>.ts.

import { del, get, getToken, patch, post, put, qs } from "./api/http";
export { ApiError, authHeaders, downloadBlob, getToken, onUnauthorized, qs, setToken, type Query } from "./api/http";

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
  // Free-form facts, e.g. FSA numbers fsa_farm, fsa_tract, fsa_field. Absent when empty.
  props?: Record<string, unknown>;
}
export interface Herd {
  id: string; name: string; species: Species; count: number; paddock_id?: string;
  autonomy: Autonomy; timer_minutes: number; created_at: string;
}
export type Sex = "female" | "male" | "castrated";
export type RemovedReason = "sold" | "died" | "culled" | "moved_off";
export interface Animal {
  id: string; tag: string; name?: string; herd_id: string; collar_id?: string;
  // 15-digit EID; born is a date (YYYY-MM-DD). Removed animals keep their row.
  eid?: string; breed?: string; sex?: Sex; born?: string; notes?: string; removed_at?: string; removed_reason?: RemovedReason;
}
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
export type ParkReason = "charging" | "shelf" | "repair";
// battery is 0-1.
export interface Collar {
  id: string; name: string; herd_id: string; animal_id?: string; last_seen?: string;
  battery?: number; boundary_version?: number; state: CollarState; last_fix?: Fix;
  // What the collar reports about itself (fw, caps), and park state: a parked collar is on a shelf or charger.
  fw?: string; caps?: string[]; outside_since?: string; parked_at?: string; parked_reason?: ParkReason;
}
export interface Position { collar_id: string; animal_id?: string; fix: Fix; state: CollarState }
// The latest position of one collar, as the map keeps it (op_core::live::PositionItem).
export interface PositionItem {
  collar_id: string; animal_id?: string; fix: Fix; state: CollarState; battery?: number; last_seen?: string;
}
export interface Boundary {
  id: string; herd_id: string; version: number; geometry: Polygon; warn_m: number; hysteresis_m: number;
  effective_at?: string; decision_id: string; created_at: string;
  // Set on one collar's own boundary during an escape.
  collar_id?: string;
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
// An animal that stayed outside its herd's boundary. Its collar holds a boundary of its own,
// the herd's joined to a pen around it, which closes in behind it until it is back.
export type EscapeStatus = "returning" | "back" | "stopped";
export interface Escape {
  id: string; herd_id: string; collar_id: string; status: EscapeStatus; geometry?: Polygon; version?: number;
  step: number; remaining_m: number; started_at: string; updated_at: string; ended_at?: string;
}
export interface BoundaryStatus {
  active?: Boundary; pending?: Boundary; proposed?: { decision_id: string; geometry: Polygon }; acks: Ack[];
  // The running move, or the last one for 10 min after it ends.
  move?: Move;
  // Open escapes, and those ended in the last 10 min.
  escapes?: Escape[];
  // Every staged boundary still to come, in version order (pending is the last), and per version
  // how many of the herd's collars hold it.
  staged?: Boundary[];
  slots?: SlotCount[];
}
export interface SlotCount { version: number; effective_at?: string; applied: number; stored: number; rejected: number; collars: number }
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

// Live events on /api/live, keyed by `type`. Each stream adds its own events from its
// ui/src/api/<id>.ts:
//   declare module "../api" { interface LiveEvents { schedule: { schedule: Schedule } } }
// and mirrors the line in the events list of docs/API.md. LiveEvent is derived from it.
export interface LiveEvents {
  fix: { collar_id: string; animal_id?: string; herd_id: string; fix: Fix; state: CollarState };
  // kind and ring come from collars that report them; older rows have neither.
  cue: { collar_id: string; at: string; level: number; margin_m: number; kind?: CueKind; ring?: number };
  ack: { collar_id: string; herd_id: string; version: number; status: AckStatus; reason?: string };
  collar: { collar: Collar };
  boundary: { herd_id: string; boundary: Boundary };
  decision: { decision: Decision };
  move: { move: Move };
  escape: { escape: Escape };
  decision_log: { decision_id: string; line: string };
  // The server dropped events for this socket; refetch everything.
  resync: Record<never, never>;
  alert: { alert: Alert };
  // Manager and owner sockets only.
  message: { message: MessageLog };
  feature: { feature: MapFeature; deleted?: boolean };
  animals_changed: { herd_id?: string };
}
export type LiveEventType = keyof LiveEvents;
export type LiveEventOf<K extends LiveEventType> = { type: K } & LiveEvents[K];
export type LiveEvent = { [K in LiveEventType]: LiveEventOf<K> }[LiveEventType];

// ---- identity -----------------------------------------------------------------

// Ordered: viewer < hand < manager < owner.
export type Role = "viewer" | "hand" | "manager" | "owner";
export type Via = "local" | "app_token" | "user_token" | "brain" | "text" | "system" | "anonymous";
// Who did something, stored on records (acks, responses, schedules, imports).
export interface Actor { via: Via; user_id?: string; name?: string }
// GET /api/me
export interface Me {
  role: Role; via: Via;
  user?: { id: string; name: string; phone?: string; phone_verified?: boolean; email?: string };
}
// A person on the farm (op_core::users). People who only text have a phone and no sign-in.
export interface User {
  id: string; name: string; role: Role; phone?: string; phone_verified_at?: string; email?: string;
  created_at: string; disabled_at?: string;
}

// ---- shared op-core records ---------------------------------------------------

export type Severity = "info" | "warning" | "critical";
export type CueKind = "warn" | "outside";
// One thing a check found, e.g. "No water inside", with what to draw and what it is about.
export interface Finding {
  code: string; severity: Severity; text: string; geometry?: GeoJSON.Geometry; targets?: [string, string][];
}
export type AlertStatus = "open" | "acked" | "resolved";
export interface Alert {
  id: string; kind: string; key: string; severity: Severity; status: AlertStatus; herd_id?: string;
  title: string; body?: string; at?: LonLat; targets: [string, string][]; data: unknown;
  opened_at: string; updated_at: string; acked_at?: string; acked_by?: Actor;
  // resolved_at without resolved_by: it cleared by itself. rolled_into: it joined a herd rollup.
  resolved_at?: string; resolved_by?: Actor; rolled_into?: string;
}
export type MessageChannel = "sms" | "whatsapp" | "email" | "webhook" | "relay" | "push";
export type MessageKind = "alert" | "brief" | "reply" | "test" | "verify" | "inbound";
export type MessageStatus = "queued" | "sending" | "sent" | "delivered" | "failed" | "received" | "ignored";
// One text, email, webhook or push, out or in. address: phone, email, URL or push endpoint id.
export interface MessageLog {
  id: string; direction: "out" | "in"; channel: MessageChannel; address: string; user_id?: string; kind: MessageKind;
  text: string; subject?: string; status: MessageStatus; error?: string; alert_id?: string; decision_id?: string;
  provider_id?: string; attempts: number; created_at: string; updated_at: string;
}
export type FeatureKind = "exclusion" | "water" | "gate" | "shade" | "hazard" | "road" | "neighbour_line" | "farm_boundary";
export type FeatureGeometry =
  | { type: "Point"; coordinates: LonLat }
  | { type: "LineString"; coordinates: LonLat[] }
  | { type: "Polygon"; coordinates: LonLat[][] };
export interface MapFeature {
  id: string; kind: FeatureKind; name?: string; geometry: FeatureGeometry; paddock_id?: string; notes?: string;
  props: Record<string, unknown>; active_from?: string; active_until?: string; created_at: string; updated_at: string;
}

export interface HostedKey { id: string; label?: string; created_at: string; last_used?: string }
export interface NewHostedKey extends HostedKey { key: string }

function socket(path: string) {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const token = getToken();
  const q = token ? `?token=${encodeURIComponent(token)}` : "";
  return new WebSocket(`${proto}//${location.host}${path}${q}`);
}

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
  stopEscape: (collar_id: string) => post<Escape>(`/api/collars/${collar_id}/escape/stop`),
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
  me: () => get<Me>("/api/me"),
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
