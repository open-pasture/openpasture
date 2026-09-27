// Welfare record and training mode (stream H): /api/welfare*. Values are SI (m, s, ms); the UI
// formats them through units.ts. The collars are audio only: cue kinds are warn and outside.

import type { Actor, CueKind } from "../api";
import { get, put } from "./http";

export type WelfareStatus = "trained" | "learning";
export type Ending = "turned_back" | "crossed" | "rest" | "boundary_changed";

export interface Outcomes { turned_back: number; crossed: number; rest: number; boundary_changed: number }

// Where an animal stands: trained after trained_after turned-back episodes in a row with no
// crossing, learning once it has any episode, no status without episodes. `since` is when
// that began; `derived` means some episodes were rebuilt from fixes (firmware 0.1).
export interface Learning {
  status?: WelfareStatus;
  since?: string;
  streak: number;
  outcomes: Outcomes;
  last_episode_at?: string;
  derived?: boolean;
}

export interface WelfareAnimal extends Learning { animal_id: string; tag: string; herd_id: string; collar_id?: string }

export interface Training { enabled: boolean; warn_m: number; trained_after: number }
export interface HerdTraining extends Training { herd_id: string }

export interface HerdWelfare {
  herd_id?: string;
  training?: Training;
  head: number; // animals not removed
  trained: number;
  learning: number;
  animals: WelfareAnimal[];
}

// One farm day: cues by kind, seconds of tone, episodes that began and the longest.
export interface WelfareDay { date: string; warn: number; outside: number; tone_s: number; episodes: number; longest_s?: number; max_level?: number }

export interface Episode {
  id: string; collar_id: string; start: string; end: string; ring: number; cues: number; max_level: number;
  min_margin_m: number; outcome: Ending; boundary_version?: number; derived?: boolean;
}

// A cue in the ledger. tone_ms is what counts (firmware 0.1 doesn't report it: its 0.3 s beep).
export interface LedgerRow {
  at: string; kind: CueKind; level: number; tone_ms: number; dur_ms?: number; margin_m: number;
  ring?: number; boundary_version?: number; collar_id: string; outcome?: Ending; episode_id?: string; derived?: boolean;
}

export interface FitCheckRow { collar_id: string; checked_at: string; by?: Actor; notes?: string }
export interface StillSpell { from: string; to?: string }

export interface AnimalWelfare {
  animal_id: string; tag: string; herd_id: string; collar_id?: string;
  from: string; to: string;
  trained_after: number;
  learning: Learning;
  days: WelfareDay[]; // oldest first
  episodes: Episode[]; // newest first
  cues: LedgerRow[]; // newest first
  truncated?: number;
  fit_checks: FitCheckRow[];
  drop_offs: StillSpell[];
}

// Where cues fired, in 2 m cells: [lon, lat, warn, outside] at each cell's centre.
export type Tick = [number, number, number, number];
export interface CuePoints { cell_m: number; size?: [number, number]; ticks: Tick[]; truncated?: number }

const enc = encodeURIComponent;

export const hApi = {
  herd: (herdId?: string) => get<HerdWelfare>("/api/welfare/animals", { herd_id: herdId }),
  animal: (animalId: string, q?: { from?: string; to?: string }) => get<AnimalWelfare>(`/api/welfare/animals/${enc(animalId)}/cues`, q),
  points: (q: { herd_id?: string; from?: string; to?: string }) => get<CuePoints>("/api/welfare/cues/points", q),
  training: (herdId: string) => get<HerdTraining>(`/api/welfare/training/${enc(herdId)}`),
  saveTraining: (herdId: string, p: Partial<Training>) => put<HerdTraining>(`/api/welfare/training/${enc(herdId)}`, p),
};
