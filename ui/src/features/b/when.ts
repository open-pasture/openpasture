// Farm-local clock times for a staged boundary: "07:00" means 07:00 where the farm is, today
// or, once that has passed, tomorrow. Works in any IANA zone, across daylight-saving changes.

// Minutes the zone is ahead of UTC at instant `t` (Chicago in summer: -300).
export function offsetMin(t: number, tz: string): number {
  const parts = new Intl.DateTimeFormat("en-US", {
    timeZone: tz, hourCycle: "h23", year: "numeric", month: "2-digit", day: "2-digit", hour: "2-digit", minute: "2-digit", second: "2-digit",
  }).formatToParts(new Date(t));
  const n = (type: string) => Number(parts.find((p) => p.type === type)?.value);
  const local = Date.UTC(n("year"), n("month") - 1, n("day"), n("hour"), n("minute"), n("second"));
  return Math.round((local - Math.floor(t / 1000) * 1000) / 60_000);
}

// The instant a wall-clock time in `tz` names. In the repeated autumn hour it is the first one;
// in the spring-forward gap the time doesn't exist and the clock reading an hour on is used.
export function zonedToUtc(date: string, hhmm: string, tz: string): number {
  const [y, m, d] = date.split("-").map(Number);
  const [hh, mm] = hhmm.split(":").map(Number);
  const wall = Date.UTC(y, m - 1, d, hh, mm);
  // The zone's offsets well before and after (a change, if any, falls between).
  const before = offsetMin(wall - 36 * 3_600_000, tz), after = offsetMin(wall + 36 * 3_600_000, tz);
  const want = `${date} ${String(hh).padStart(2, "0")}:${String(mm).padStart(2, "0")}`;
  const hits = [before, after].map((o) => wall - o * 60_000).filter((t) => wallOf(t, tz) === want);
  return hits.length ? Math.min(...hits) : wall - before * 60_000;
}

// "YYYY-MM-DD HH:MM" at instant `t` in `tz`.
function wallOf(t: number, tz: string): string {
  return new Date(t).toLocaleString("sv-SE", { timeZone: tz, hour12: false }).slice(0, 16);
}

// Today's date in `tz`.
export function dateIn(t: number, tz: string): string {
  return new Date(t).toLocaleDateString("en-CA", { timeZone: tz });
}

// The next time the clock in `tz` reads `hhmm` after `now`: today, else tomorrow.
export function nextAt(hhmm: string, tz: string, now = Date.now()): number {
  const today = dateIn(now, tz);
  const t = zonedToUtc(today, hhmm, tz);
  if (t > now) return t;
  const noon = zonedToUtc(today, "12:00", tz) + 86_400_000;
  return zonedToUtc(dateIn(noon, tz), hhmm, tz);
}

// "HH:MM" in `tz` at `t`.
export function clockIn(t: number, tz: string): string {
  return new Date(t).toLocaleTimeString("en-GB", { timeZone: tz, hour: "2-digit", minute: "2-digit", hour12: false });
}

// "07:00" today, "Wed 07:00" another day.
export function atLabel(t: number, tz: string, now = Date.now()): string {
  const clock = clockIn(t, tz);
  if (dateIn(t, tz) === dateIn(now, tz)) return clock;
  return `${new Date(t).toLocaleDateString("en-US", { weekday: "short", timeZone: tz })} ${clock}`;
}

// Five minutes from now, rounded up to the minute, as the clock in `tz` reads it.
export function soon(tz: string, now = Date.now()): string {
  return clockIn(Math.ceil((now + 5 * 60_000) / 60_000) * 60_000, tz);
}
