// Stream B: map layers, measured heights, and typed reads of the existing signals and land
// reports (docs/API.md, "Map layers, measured heights" and "Decisions and brains").

import type { Actor, Paddock } from "../api";
import { get, post } from "./http";

// One paddock's layer values. Absent means nothing is known.
export interface PaddockLayer {
  paddock_id: string;
  grazing?: boolean; // a herd is in it now
  rest_days?: number;
  last_grazed?: string;
  ndvi?: number;
  ndvi_at?: string; // YYYY-MM-DD of the imagery
  drought?: { category: string | null }; // D0-D4, null: not in drought
  flood?: { in_floodplain: boolean; zone?: string; risk?: "medium" | "high" };
}
export interface Layers { as_of: string; paddocks: PaddockLayer[] }

export interface Height {
  id: string; paddock_id: string; at: string; height_cm: number; residual_cm?: number; by: Actor; created_at: string;
}
export interface NewHeight { height_cm: number; residual_cm?: number; at?: string }

// Standing forage above the residual (the grazing signals' `forage`). A height measured in the
// last 21 days is source "measured"; snow or dormant grass withhold the imagery estimate (reason).
export interface Forage {
  height_inches: number | null;
  available_kg_dm_per_ha: number | null;
  residual_target_inches?: number;
  source: "imagery" | "measured" | "farmer" | null;
  confidence: string | null;
  height_cm?: number;
  measured_at?: string;
  reason?: "snow" | "dormant";
}
export interface SignalPaddock {
  paddock_id: string; name: string; status: Paddock["status"]; area_ha: number; current: boolean;
  rest_days: number | null; last_grazed?: string | null; grazing_days?: number | null; forage: Forage | null;
}
export interface HerdSignals {
  as_of: string; herd_id: string | null; current_paddock_id: string | null; herd_animal_units: number | null;
  feed_budget_days_current: number | null; paddocks: SignalPaddock[];
}

// The weather section of a land report (open data or the land provider). Temperatures °C, rain mm.
export interface WeatherDay {
  date: string; precip_mm?: number | null; precip_probability?: number | null;
  temp_max_c?: number | null; temp_min_c?: number | null; temp_mean_c?: number | null;
}
export interface Weather {
  status: "ok";
  current?: { air_temp_c?: number | null; precip_mm_24h?: number | null; wind_kph?: number | null; snow_depth_cm?: number | null };
  history?: WeatherDay[];
  forecast?: WeatherDay[];
}
export interface Land {
  paddock_id: string; source: string; as_of: string; cached: boolean;
  sections: { weather?: Weather | { status: "unavailable"; reason?: string } } & Record<string, unknown>;
}

export const b = {
  layers: () => get<Layers>("/api/layers/paddocks"),
  heights: (paddockId: string, limit?: number) => get<Height[]>(`/api/paddocks/${paddockId}/heights`, { limit }),
  addHeight: (paddockId: string, body: NewHeight) => post<Height>(`/api/paddocks/${paddockId}/heights`, body),
  signals: (herdId?: string) => get<HerdSignals>("/api/signals", { herd_id: herdId }),
  land: (paddockId: string) => get<Land>(`/api/land/${paddockId}`),
};
