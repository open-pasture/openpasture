// How far an animal walks a day, kept pure for tests.

import type { Behaviour } from "../../api";

const DAY_H = 24;

// Hours the collar has data for in the range: its time in paddocks and outside them (the
// server credits each fix up to its next one, at most 30 min).
export const trackedHours = (r: Behaviour) => Object.values(r.paddock_hours ?? {}).reduce((a, h) => a + h, r.outside_hours ?? 0);

// Metres walked on an average day with data: the distance over the days the collar was
// tracked, and all of it when that is under a day. Not over `days`, which is every UTC date
// the range touches (two for the last 24 h) whether the collar reported or not.
export function walkedPerDay(r: Behaviour): number {
  const m = (r.distance_km ?? 0) * 1000;
  return m / Math.max(1, trackedHours(r) / DAY_H);
}
