// K-animals: CSV import, remove and swap, park, bulk collar linking, new keys and cards.
// docs/API.md "Animals and collar linking (K-animals)".

import type { Animal, Collar, ParkReason, RemovedReason } from "../api";
import { get, post, req } from "./http";

// field → column name. Fields: tag, eid, name, breed, sex, born, collar, notes.
export type Mapping = Partial<Record<ImportField, string>>;
export type ImportField = "tag" | "eid" | "name" | "breed" | "sex" | "born" | "collar" | "notes";
// Spreadsheet row numbers: the header is row 1.
export interface RowError { row: number; error: string }
export interface ImportPreview {
  import_id: string; columns: string[]; mapping: Mapping; rows: string[][]; total: number; errors: RowError[];
}
export interface ImportResult { created: number; updated: number; unchanged: number; total: number; errors: RowError[] }
// A collar with what setting it up needs. The key is shown once.
export interface LinkedCollar { collar: Collar; key: string; endpoint: string; public_key: string; tag?: string }
export interface Batch { batch_id: string; collars: LinkedCollar[] }
export interface Card { collar_id: string; qr_svg: string }
// Imported position history (K-files), [lon, lat, unix seconds].
export interface ImportedTrack { animal_id: string; points: [number, number, number][] }

const csv = { "Content-Type": "text/csv" };

export const kAnimals = {
  preview: (file: Blob | string, herd_id?: string) => req<ImportPreview>("POST", "/api/animals/import/preview", file, { herd_id }, csv),
  commit: (import_id: string, mapping: Mapping, herd_id: string) =>
    post<ImportResult>(`/api/animals/import/${import_id}/commit`, { mapping, herd_id }),
  remove: (id: string, reason: RemovedReason, at?: string) => post<Animal>(`/api/animals/${id}/remove`, { reason, at }),
  swap: (id: string, collar_id: string) => post<Animal>(`/api/animals/${id}/swap`, { collar_id }),
  park: (collar_id: string, reason: ParkReason) => post<Collar>(`/api/collars/${collar_id}/park`, { reason }),
  unpark: (collar_id: string) => post<Collar>(`/api/collars/${collar_id}/unpark`),
  // CSV rows "tag,collar name"; errors come back as 400 with every row to fix.
  link: (file: Blob | string, herd_id: string) => req<Batch>("POST", "/api/collars/bulk", file, { herd_id }, csv),
  rekey: (collar_id: string) => post<LinkedCollar>(`/api/collars/${collar_id}/rekey`),
  // Keys go in the body, never the URL.
  cards: (items: { collar_id: string; key: string }[]) => post<Card[]>("/api/cards", { items }),
  importedTracks: (animal_id: string) => get<ImportedTrack[]>("/api/import/positions/tracks", { animal_id, max_points: 600 }),
};
