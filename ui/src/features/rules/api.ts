// Farm rules, the safety gate's settings and log, and the app token (docs/API.md, "Farm
// rules", "Safety gate", "Owner sign-in").

import type { Actor, FeatureKind, Finding, MapFeature, Polygon } from "../../api";
import { del, get, patch, post, put } from "../../api/http";

// What a rule could become on the map: a no-go zone, or an exclusion for its dates.
export interface Suggestion {
  kind: Extract<FeatureKind, "no_go" | "exclusion">;
  name: string; geometry: Polygon; paddock_id?: string; active_from?: string; active_until?: string;
  text: string; // "Make Creek a no-go zone"
}

export interface FarmRule {
  id: string; text: string; feature_id?: string;
  created_by: Actor; created_at: string; updated_by?: Actor; updated_at: string;
  suggestion?: Suggestion;
}

export interface GateSettings { require_water: boolean }

export interface GateEntry {
  id: string; at: string; herd_id: string; collar_id?: string; sender: string; by?: Actor; decision_id?: string;
  allowed: boolean; codes: string[]; confirmed: string[]; findings: Finding[]; reason?: string;
}

export const rulesApi = {
  list: () => get<FarmRule[]>("/api/rules"),
  add: (text: string) => post<FarmRule>("/api/rules", { text }),
  edit: (id: string, text: string) => patch<FarmRule>(`/api/rules/${id}`, { text }),
  remove: (id: string) => del(`/api/rules/${id}`),
  convert: (id: string) => post<{ rule: FarmRule; feature: MapFeature }>(`/api/rules/${id}/convert`),
  gate: () => get<GateSettings>("/api/gate/settings"),
  setGate: (b: Partial<GateSettings>) => put<GateSettings>("/api/gate/settings", b),
  log: (q: { herd_id?: string; decision_id?: string; refused?: boolean; limit?: number } = {}) => get<GateEntry[]>("/api/gate/log", q),
  revealToken: () => post<{ app_token: string }>("/api/settings/app-token/reveal"),
  rotateToken: () => post<{ app_token: string }>("/api/settings/app-token/rotate"),
  signin: (code: string) => post<{ token: string }>("/api/session", { code }),
};
