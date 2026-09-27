// Coverage and fleet care (stream G): /api/coverage and /api/fleet*. Values are SI; the UI
// formats them through units.ts.

import type { Actor } from "../api";
import { get, post, put } from "./http";

// accuracy and fixes (G); H adds its own (fix rate, cell signal) through features/g/metrics.ts.
export type CoverageMetric = "accuracy" | "fixes" | (string & {});
// [lon, lat, value, n] at the cell's centre. accuracy: median metres, n fixes. fixes: share of
// the fixes the cadence called for that came (0-1), n fixes called for.
export type CoverageCell = [number, number, number, number];
export interface Coverage {
  metric: CoverageMetric;
  cell_m: number;
  unit: "m" | "ratio" | (string & {});
  size?: [number, number]; // degrees of longitude and latitude one cell spans
  cells: CoverageCell[];
}

export interface FleetRow {
  collar_id: string;
  name: string;
  herd_id: string;
  tag?: string;
  battery?: number; // 0-1
  trend_pct_day?: number; // percentage points a day, since the last charge
  days_left?: number; // only while falling, with three days of data
  fit_checked_at?: string;
  fit_due_at: string;
  last_seen?: string;
  parked: boolean;
  daily: (number | null)[]; // mean battery of each of the last 14 UTC days, today last
}

export interface FitCheck { id: string; collar_id: string; checked_at: string; by?: Actor; notes?: string }
export interface FleetSettings { fit_check_days: number }

const checks = (collarId: string) => `/api/fleet/${encodeURIComponent(collarId)}/fit-checks`;

export const gApi = {
  coverage: (q: { metric: CoverageMetric; from?: string; to?: string; cell_m?: number; herd_id?: string }) => get<Coverage>("/api/coverage", q),
  fleet: (q?: { herd_id?: string; collar_id?: string }) => get<FleetRow[]>("/api/fleet", q),
  fitChecks: (collarId: string) => get<FitCheck[]>(checks(collarId)),
  checkFit: (collarId: string, body: { checked_at?: string; notes?: string } = {}) => post<FitCheck>(checks(collarId), body),
  checkFits: (collarIds: string[], body: { checked_at?: string; notes?: string } = {}) => post<FitCheck[]>("/api/fleet/fit-checks", { collar_ids: collarIds, ...body }),
  settings: () => get<FleetSettings>("/api/fleet/settings"),
  saveSettings: (s: FleetSettings) => put<FleetSettings>("/api/fleet/settings", s),
};
