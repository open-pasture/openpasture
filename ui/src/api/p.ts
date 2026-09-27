// What /api/live sends instead of one message per fix, ack and cue: every 500 ms, per herd,
// one positions, one ack_batch and one cue_batch (op_core::live).

import type { AckStatus, CueKind } from "../api";

// A collar's latest boundary ack in the window. A collar whose reported version moved
// without an ack shows as applied.
export interface AckItem { collar_id: string; version: number; status: AckStatus; code?: string; reason?: string }
// One cue, as the single cue event has it; every cue in the window, in order.
export interface CueItem { collar_id: string; at: string; level: number; margin_m: number; kind?: CueKind; ring?: number }

declare module "../api" {
  interface LiveEvents {
    // The collars whose position or telemetry changed: newest fix, state, battery, last contact.
    positions: { herd_id: string; items: PositionItem[] };
    ack_batch: { herd_id: string; items: AckItem[] };
    cue_batch: { herd_id: string; items: CueItem[] };
  }
}
