// Strips, layouts and paddock copies (docs/API.md, "Strips and layouts"). SI throughout.

import type { Actor, Paddock, Polygon } from "../api";
import { del, get, patch, post } from "./http";

// How to cut: an orientation (compass bearing the strips advance toward; 0 = strips run
// east-west, advancing north) and exactly one of width_m, count or days.
export interface StripParams {
  orientation_deg: number;
  width_m?: number;
  count?: number;
  days?: number;
  head?: number;
  warn_m?: number;
}

export interface StripFacts {
  geometry: Polygon;
  area_ha: number;
  // The strip less the exclusions in effect now.
  grazeable_ha: number;
  // Absent without a forage estimate or animals.
  days?: number;
}

export interface StripPreview {
  paddock_id: string;
  strips: StripFacts[];
  width_m: number;
  depth_m: number;
  head: number;
  animal_units: number;
  forage_kg_dm_per_ha?: number;
  forage_source?: string;
  warn_m: number;
}

export interface Layout {
  id: string;
  paddock_id: string;
  name: string;
  params: StripParams;
  strips: Polygon[];
  created_by: Actor;
  created_at: string;
  updated_at: string;
}

// A layout on its paddock now: its strips (cut again if the paddock changed shape) with today's days.
export interface AppliedLayout extends StripPreview {
  layout: Layout;
}

export const cApi = {
  preview: (b: { paddock_id: string; herd_id?: string } & StripParams) => post<StripPreview>("/api/strips/preview", b),
  layouts: (paddock_id?: string) => get<Layout[]>("/api/layouts", { paddock_id }),
  saveLayout: (b: { paddock_id: string; herd_id?: string; name?: string } & StripParams) => post<Layout>("/api/layouts", b),
  renameLayout: (id: string, name: string) => patch<Layout>(`/api/layouts/${id}`, { name }),
  deleteLayout: (id: string) => del(`/api/layouts/${id}`),
  applyLayout: (id: string, b: { herd_id?: string; head?: number } = {}) => post<AppliedLayout>(`/api/layouts/${id}/apply`, b),
  // offset_m: metres east and north to move the copy by.
  copyPaddock: (id: string, b: { name?: string; offset_m?: [number, number] } = {}) => post<Paddock>(`/api/paddocks/${id}/copy`, b),
};
