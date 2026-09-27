// Reports, the feed log, leases and report settings (docs/API.md "Reports").
// Report values arrive in the farm's units; each column's unit says which.

import { del, downloadBlob, get, patch, post, put } from "./http";
import type { Actor } from "../api";

export type Cell = string | number | null;
// decimals: the places a number column prints with.
export interface Column { key: string; label: string; unit?: string; decimals?: number }
export interface ReportSection { title: string; columns: Column[]; rows: Cell[][]; totals?: Cell[] }
export interface ReportDoc {
  id: string; title: string; farm: string; from: string; to: string; herd_id?: string; generated_at: string;
  header: [string, string][]; sections: ReportSection[]; notes: string[]; signatures: string[];
}
export interface ReportInfo { id: string; title: string }
export interface ReportQuery { from: string; to: string; herd_id?: string }

export interface AuFactors { cow: number; bull: number; pair: number; weaned_calf: number }
export interface HerdMix { cows: number; bulls: number; calves: number; pairs: boolean }
export interface HerdReport { mean_weight_kg?: number; intake_pct: number; mix?: HerdMix }
export interface ReportInputs { operator?: string; fsa_farm?: string; au: AuFactors; herds: Record<string, HerdReport> }

// kg_dm is dry matter in kg; date is the farm-local day.
export interface FeedEntry { id: string; herd_id: string; date: string; kg_dm: number; kind: string; note?: string; created_by?: Actor; created_at: string }
export interface NewFeedEntry { herd_id: string; date: string; kg_dm: number; kind?: string; note?: string }

export type RatePer = "acre_season" | "head_day" | "au_day" | "aum" | "pair_month";
// For acre_season, rate_amount is per hectare (SI); the UI shows it per the farm's area unit.
export interface Lease {
  paddock_id: string; landowner: string; rate_per: RatePer; rate_amount: number; currency: string;
  season_from?: string; season_to?: string; notes?: string; updated_at: string;
}
export type LeaseBody = Omit<Lease, "paddock_id" | "updated_at">;

export const reportsApi = {
  list: () => get<ReportInfo[]>("/api/reports"),
  get: (id: string, q: ReportQuery) => get<ReportDoc>(`/api/reports/${id}`, { ...q }),
  csv: (id: string, q: ReportQuery) => downloadBlob(`/api/reports/${id}`, `${id}-${q.from}-${q.to}.csv`, { ...q, format: "csv" }),
  inputs: () => get<ReportInputs>("/api/reports/settings"),
  // A JSON merge patch: null clears a field or drops a herd's entry.
  updateInputs: (p: Record<string, unknown>) => put<ReportInputs>("/api/reports/settings", p),
  feed: (q: { herd_id?: string; from?: string; to?: string } = {}) => get<FeedEntry[]>("/api/feed-log", q),
  addFeed: (e: NewFeedEntry) => post<FeedEntry>("/api/feed-log", e),
  updateFeed: (id: string, p: Partial<NewFeedEntry> & { note?: string | null }) => patch<FeedEntry>(`/api/feed-log/${id}`, p),
  deleteFeed: (id: string) => del(`/api/feed-log/${id}`),
  leases: () => get<Lease[]>("/api/leases"),
  putLease: (paddockId: string, l: LeaseBody) => put<Lease>(`/api/leases/${paddockId}`, l),
  deleteLease: (paddockId: string) => del(`/api/leases/${paddockId}`),
};
