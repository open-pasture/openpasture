// Map features (stream D): exclusions, water, gates, shade, hazards, roads, neighbour lines
// and the farm boundary. Records and the `feature` live event are typed in ../api.

import type { FeatureGeometry, FeatureKind, MapFeature } from "../api";
import { del, get, patch, post, type Query } from "./http";

export type { FeatureGeometry, FeatureKind, MapFeature } from "../api";

export interface NewFeature {
  kind: FeatureKind;
  name?: string;
  geometry: FeatureGeometry;
  // Absent: farm-wide.
  paddock_id?: string;
  notes?: string;
  // water: { source }; a hazard point: { radius_m }.
  props?: Record<string, unknown>;
  active_from?: string;
  active_until?: string;
}

// A JSON merge patch: null clears an optional field (paddock_id: null makes it farm-wide).
// kind, id and the timestamps don't change.
export type FeaturePatch = {
  name?: string | null;
  geometry?: FeatureGeometry;
  paddock_id?: string | null;
  notes?: string | null;
  props?: Record<string, unknown>;
  active_from?: string | null;
  active_until?: string | null;
};

// kind: one kind; paddock_id: one paddock's; active: "true" (now) or an RFC 3339 time.
export interface FeatureQuery extends Query { kind?: FeatureKind; paddock_id?: string; active?: string }

export const featuresApi = {
  list: (q?: FeatureQuery) => get<MapFeature[]>("/api/features", q),
  get: (id: string) => get<MapFeature>(`/api/features/${encodeURIComponent(id)}`),
  create: (b: NewFeature) => post<MapFeature>("/api/features", b),
  update: (id: string, b: FeaturePatch) => patch<MapFeature>(`/api/features/${encodeURIComponent(id)}`, b),
  remove: (id: string) => del(`/api/features/${encodeURIComponent(id)}`),
};
