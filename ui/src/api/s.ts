// Strip schedules (S): a herd walked across a paddock's strips on a cadence, each open and
// back-fence step staged on the collars ahead of time. Mirrors docs/API.md "Strip schedules".

import { get, post } from "./http";
import type { Actor, Polygon } from "../api";

export interface Cadence { every_days: number; at: string /* HH:MM farm time */ }
export interface BackFence { enabled: boolean; lag_strips: number; close_after_min: number; close_steps: number; close_every_min: number }
export type ScheduleStatus = "active" | "paused" | "done";
export interface Schedule {
  id: string; herd_id: string; paddock_id: string; layout_id?: string; strips: Polygon[];
  // The next strip to open, 0-based; strips.length once every strip opened.
  next_index: number; cadence: Cadence; starts_at: string; back_fence: BackFence; status: ScheduleStatus;
  created_by: Actor; created_at: string; updated_at: string; planned_end?: string; ended_at?: string;
}
export type MoveState = "planned" | "staged" | "done" | "skipped";
// One open (step 0) or back-fence step (1..) of a schedule.
export interface ScheduledMove {
  schedule_id: string; index: number; step: number; at: string; geometry: Polygon;
  boundary_version?: number; skipped?: "late" | "skipped" | "held"; state: MoveState; applied_at?: string;
}
export interface NewSchedule {
  herd_id: string; layout_id?: string; paddock_id?: string; strips?: Polygon[]; next_index?: number;
  cadence?: Cadence; starts_at?: string; back_fence?: Partial<BackFence>;
}

declare module "../api" {
  interface LiveEvents {
    // Made, changed (a move staged, opened, skipped, held or retimed), paused, resumed or ended.
    schedule: { schedule: Schedule };
  }
}

const at = (id: string, what: string) => `/api/schedules/${encodeURIComponent(id)}/${what}`;

export const sApi = {
  list: (herd_id?: string, status?: ScheduleStatus | "running") => get<Schedule[]>("/api/schedules", { herd_id, status }),
  moves: (id: string) => get<ScheduledMove[]>(at(id, "moves")),
  create: (b: NewSchedule) => post<Schedule>("/api/schedules", b),
  preview: (b: NewSchedule) => post<{ schedule: Schedule; moves: ScheduledMove[] }>("/api/schedules/preview", b),
  skip: (id: string, index: number) => post<Schedule>(at(id, "skip"), { index }),
  hold: (id: string) => post<Schedule>(at(id, "hold")),
  moveNow: (id: string) => post<Schedule>(at(id, "move-now")),
  setTime: (id: string, index: number, when: string) => post<Schedule>(at(id, "time"), { index, at: when }),
  pause: (id: string) => post<Schedule>(at(id, "pause")),
  resume: (id: string) => post<Schedule>(at(id, "resume")),
  end: (id: string) => post<Schedule>(at(id, "end")),
};
