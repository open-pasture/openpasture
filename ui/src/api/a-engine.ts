// Alerts: /api/alerts* (docs/API.md "Alerts"). The Alert record and the `alert` live event
// are op-core's (../api).

import type { Alert, Role, Severity } from "../api";
import { get, post, put } from "./http";

export type AlertFilter = "open" | "acked" | "resolved" | "all";

export interface RuleConfig { enabled: boolean; severity: Severity; after_min?: number; threshold?: number; notify: boolean }
// A rule as Settings shows it: `sentence` holds "{n}", the rule's number in `unit`
// ("min" → after_min, "%" or "m" → threshold, "" → no number).
export interface RuleView extends RuleConfig {
  kind: string; sentence: string; unit: "min" | "%" | "m" | ""; cadence_s: number; default: RuleConfig;
}
export interface Policy {
  renotify_every_min: number; renotify_max: number; escalate_after_min: number; group_window_s: number; rollup_min: number;
  herd_silent_share: number; clear_after_min: number; start_grace_min: number; critical_window_s: number;
  // The farm's quiet hours, farm time HH:MM.
  quiet_start?: string; quiet_end?: string;
}
export type PersonChannel = "sms" | "whatsapp" | "email" | "push";
export interface RulesView {
  rules: RuleView[];
  policy: Policy;
  // Channels that can send now, and those a person can choose from them.
  configured: string[];
  person_channels: PersonChannel[];
}
// A rules change: only the fields named; null clears a number back to its default.
export interface RulesChange {
  rules?: Record<string, Partial<Record<keyof RuleConfig, unknown>>>;
  policy?: Partial<Record<keyof Policy, unknown>>;
}
export interface AlertPrefs {
  channels: PersonChannel[]; min_severity: Severity; herds?: string[]; muted_kinds: string[];
  // Absent: the farm's quiet hours.
  quiet_start?: string; quiet_end?: string; critical_in_quiet: boolean; on_duty: boolean;
}
export interface PersonPrefs extends AlertPrefs { user_id: string; name: string; role: Role; sms_opt_out: boolean; updated_at?: string }
export type PrefsChange = Partial<Record<keyof AlertPrefs, unknown>>;

export const alertsApi = {
  list: (q: { status?: AlertFilter; herd_id?: string; limit?: number; from?: string; to?: string } = {}) => get<Alert[]>("/api/alerts", q),
  get: (id: string) => get<Alert>(`/api/alerts/${id}`),
  ack: (id: string) => post<Alert>(`/api/alerts/${id}/ack`),
  resolve: (id: string) => post<Alert>(`/api/alerts/${id}/resolve`),
  rules: () => get<RulesView>("/api/alerts/rules"),
  setRules: (b: RulesChange) => put<RulesView>("/api/alerts/rules", b),
  prefs: () => get<PersonPrefs[]>("/api/alerts/prefs"),
  myPrefs: () => get<PersonPrefs>("/api/alerts/prefs/me"),
  setMyPrefs: (b: PrefsChange) => put<PersonPrefs>("/api/alerts/prefs/me", b),
  setPrefs: (userId: string, b: PrefsChange) => put<PersonPrefs>(`/api/alerts/prefs/${userId}`, b),
};
