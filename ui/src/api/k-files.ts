// Paddock files and position history (K-files): /api/import/*.

import type { Actor, Paddock, Polygon } from "../api";
import { del, get, post } from "./http";

// A polygon from a file, ready to become a paddock. props: fsa_farm, fsa_tract, fsa_field.
export interface Draft { name: string; layer?: string; geometry: Polygon; area_ha: number; props?: Record<string, string> }
export interface PaddockPreview { import_id: string; file: string; drafts: Draft[]; errors: string[] }

// Which column (CSV) or property (GeoJSON) holds each field; no tag = one animal per file.
export type Mapping = { tag?: string; time?: string; lat?: string; lon?: string; accuracy?: string };
export type PositionSource = "csv" | "gpx" | "geojson";
export interface ImportLabel { label: string; points: number; from: string; to: string; animal_id?: string; tag?: string }
export type TrackPoint = [number, number, number]; // lon, lat, unix seconds
export interface PositionPreview {
  import_id: string; file: string; source: PositionSource; columns?: string[]; mapping: Mapping; rows?: string[][];
  total: number; points: number; labels: ImportLabel[]; tracks: { label: string; points: TrackPoint[] }[];
  needs_zone: boolean; zone: string; errors: string[];
}
export interface PositionImport {
  id: string; file_name: string; source: PositionSource; zone?: string; fixes: number; animals: number;
  from?: string; to?: string; created_by: Actor; created_at: string;
}
export interface PositionCommit { import: PositionImport; duplicates: number; skipped: string[]; errors: string[] }
export interface ImportTrack { animal_id: string; points: TrackPoint[] }

const form = (file: File) => {
  const f = new FormData();
  f.append("file", file, file.name);
  return f;
};

export const files = {
  previewPaddocks: (file: File) => post<PaddockPreview>("/api/import/paddocks/preview", form(file)),
  commitPaddocks: (b: { import_id: string; keep: number[]; names?: (string | null)[] }) => post<{ paddocks: Paddock[] }>("/api/import/paddocks/commit", b),
  previewPositions: (file: File) => post<PositionPreview>("/api/import/positions/preview", form(file)),
  rereadPositions: (id: string, b: { mapping?: Mapping; zone?: string }) => post<PositionPreview>(`/api/import/positions/${id}/preview`, b),
  commitPositions: (id: string, b: { mapping?: Mapping; zone?: string; animals?: Record<string, string | null> }) =>
    post<PositionCommit>(`/api/import/positions/${id}/commit`, b),
  imports: () => get<PositionImport[]>("/api/import/positions"),
  removeImport: (id: string) => del(`/api/import/positions/${id}`),
  tracks: (q: { animal_id?: string; import_id?: string; from?: string; to?: string; max_points?: number }) =>
    get<ImportTrack[]>("/api/import/positions/tracks", q),
};
