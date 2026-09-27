// The pre-send check (docs/API.md, "Pre-send check"). SI throughout; the UI formats through units.ts.

import type { Finding, LonLat, Polygon } from "../api";
import { post } from "./http";

export interface CheckRequest {
  geometry: Polygon;
  warn_m?: number;
  effective_at?: string;
  // Preview the sweep that walks the herd in.
  sweep?: boolean;
}

export interface CheckFacts {
  area_ha: number;
  head: number;
  m2_per_head: number;
  grazing_days?: number;
  forage_kg_dm?: number;
  forage_source?: "ndvi" | "measured";
  rest_days?: number;
  sweep_minutes?: number;
  vertices: number;
  holes: number;
}

export interface SweepPreview {
  // Where the back of the sweep will be, first to last, about every 10 m.
  back_lines: LonLat[][];
  minutes: number;
}

export interface CheckResult {
  // What sending it now stores: exclusion holes cut in, fitted to the collars.
  sent: Polygon;
  // The ring a collar without holes (firmware 0.1) enforces, when the herd has one.
  legacy?: Polygon;
  facts: CheckFacts;
  findings: Finding[];
  sweep?: SweepPreview;
}

export const fApi = {
  check: (herdId: string, b: CheckRequest) => post<CheckResult>(`/api/herds/${herdId}/check`, b),
};
