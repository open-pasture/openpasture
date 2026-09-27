// Protocol v1 on the server (E-srv): what each collar is and holds, and the collars' cadence.
// Types mirror op-ingest's serde shapes (docs/API.md, "Protocol v1").

import { get, put } from "./http";
import type { AckStatus, SlotCount } from "../api";

declare module "../api" {
  // An ack's reject code (protocol v1), e.g. "hole_too_close".
  interface Ack { code?: string }
}

// What a collar can hold. A legacy collar (firmware 0.1) reports none and gets LEGACY.
export interface CollarLimits { outer: number; holes: number; hole_vertices: number; total: number; slots: number; slot_bytes: number }

// One boundary a collar holds ("applied" in effect, "received" staged) or refused.
export interface HeldSlot {
  version: number;
  // The herd version this copies (a collar handed back to its herd after an escape).
  copy_of?: number;
  status: AckStatus; effective_at?: string; reported_at: string; code?: string;
}

// The collar's signed config, as stored.
export interface CollarConfigView {
  version: number; herd_id?: string; endpoint?: string; report_s: number; poll_s: number;
  fast_report_s?: number; fast_poll_s?: number; fast_until?: string; refused?: true;
}

export interface CollarSlots {
  collar_id: string; fw?: string; caps?: string[]; limits: CollarLimits; slots: HeldSlot[];
  config?: CollarConfigView; parked?: true; escaped?: true;
}

export interface HerdSlots { counts: SlotCount[]; collars: CollarSlots[] }

// Report and poll cadence, seconds; the fast pair while the herd is moved or the collar escaped.
export interface CollarsConfig { report_s: number; poll_s: number; fast_report_s: number; fast_poll_s: number }

export const esrv = {
  collarSlots: (id: string) => get<CollarSlots>(`/api/collars/${encodeURIComponent(id)}/slots`),
  herdSlots: (id: string) => get<HerdSlots>(`/api/herds/${encodeURIComponent(id)}/slots`),
  collarsConfig: () => get<CollarsConfig>("/api/collars/config"),
  saveCollarsConfig: (c: CollarsConfig) => put<CollarsConfig>("/api/collars/config", c),
};
