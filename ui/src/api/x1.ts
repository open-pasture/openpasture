// One message of /api/live as the events it carries. The server gathers the whole farm's
// positions, ack_batch and cue_batch every 500 ms; a window holding more than one goes out
// as { type: "batch", events: [...] }, in herd order. Everything else is one event.

import type { LiveEvent } from "../api";

export interface LiveBatch { type: "batch"; events: LiveEvent[] }

export function liveEvents(msg: unknown): LiveEvent[] {
  if (!msg || typeof msg !== "object" || typeof (msg as { type?: unknown }).type !== "string") return [];
  const m = msg as LiveEvent | LiveBatch;
  if (m.type !== "batch") return [m];
  return Array.isArray(m.events) ? m.events.filter((e) => e && typeof e === "object" && typeof e.type === "string") : [];
}
