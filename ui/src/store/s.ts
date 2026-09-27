// Each herd's running strip schedule and its moves, kept by `schedule` live events, and which
// collars don't yet hold the next open (for the map's warn ring).

import { sApi, type Schedule, type ScheduledMove } from "../api/s";
import { createSlice } from "./slice";

export interface HerdSchedule { schedule?: Schedule; moves: ScheduledMove[]; missing: string[] }
export interface SState { byHerd: Record<string, HerdSchedule | undefined> }

export const sState = createSlice<SState>({ byHerd: {} });

const inflight = new Map<string, Promise<void>>();

// Load (or reload) the herd's running schedule, its moves and the collars missing the next open.
export function loadSchedule(herdId: string): Promise<void> {
  const busy = inflight.get(herdId);
  if (busy) return busy;
  const p = (async () => {
    try {
      const [schedule] = await sApi.list(herdId, "running");
      const moves = schedule ? await sApi.moves(schedule.id) : [];
      let missing: string[] = [];
      // The views' helpers load with them, not with the app.
      const { missingCollars, nextOpen } = await import("../features/s/model");
      const next = nextOpen(moves);
      if (schedule?.status === "active" && next?.boundary_version !== undefined) {
        const { esrv } = await import("../api/e-srv");
        const held = await esrv.herdSlots(herdId).catch(() => undefined);
        if (held) missing = missingCollars(held.collars, next.boundary_version);
      }
      sState.set((s) => ({ byHerd: { ...s.byHerd, [herdId]: { schedule, moves, missing } } }));
    } catch {
      /* the next event or reconnect loads again */
    } finally {
      inflight.delete(herdId);
    }
  })();
  inflight.set(herdId, p);
  return p;
}

// A burst of events for one herd is one reload.
const timers = new Map<string, ReturnType<typeof setTimeout>>();
export function reloadSoon(herdId: string, ms = 400) {
  clearTimeout(timers.get(herdId));
  timers.set(herdId, setTimeout(() => void loadSchedule(herdId), ms));
}
