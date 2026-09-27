// Weather lines for the paddock sheet, in the farm's units: the air now and a three-day forecast.
// units.ts (and op_core::units) have no temperature or rainfall yet, so those two live here,
// driven by the same settings.units; snow depth is a height and goes through units.ts.

import type { Weather, WeatherDay } from "../../api/b";
import { fmt, num, type Units } from "../../units";

// Rust's f64::round, as units.ts: halves away from zero.
const round = (x: number) => (x < 0 ? -Math.round(-x) : Math.round(x));

// Whole degrees: "64°F" | "18°C". bare: "64°" (the forecast, under a line that names the unit).
export function temp(c: number, units: Units, bare = false): string {
  const v = units === "imperial" ? (c * 9) / 5 + 32 : c;
  const n = round(v);
  return `${n === 0 ? 0 : n}°${bare ? "" : units === "imperial" ? "F" : "C"}`;
}

// Rainfall: "0.14 in", "1.2 in" | "3.5 mm", "12 mm". Under a hundredth of an inch (0.25 mm) is "0 in".
export function rain(mm: number, units: Units): string {
  if (units === "imperial") {
    const inches = mm / 25.4;
    return `${num(inches, inches >= 1 ? 1 : 2).replace(/^0\.00$/, "0")} in`;
  }
  return `${num(mm, mm >= 10 ? 0 : 1).replace(/^0\.0$/, "0")} mm`;
}

// A day in the forecast worth naming its rain: at least a millimetre.
const WET_MM = 1;

const weekday = (date: string) => new Date(`${date}T12:00:00Z`).toLocaleDateString("en-US", { weekday: "short", timeZone: "UTC" });

// "18°C now, 3.5 mm last 24 h, 8 cm snow" then one line per forecast day from `today`
// (YYYY-MM-DD): "Sat 22°/11° 4.0 mm". Nothing when the section has no numbers.
export function weatherLines(w: Weather | undefined, units: Units, today: string): string[] {
  if (!w || w.status !== "ok") return [];
  const out: string[] = [];
  const c = w.current ?? {};
  const now: string[] = [];
  if (typeof c.air_temp_c === "number") now.push(`${temp(c.air_temp_c, units)} now`);
  if (typeof c.precip_mm_24h === "number" && c.precip_mm_24h >= WET_MM) now.push(`${rain(c.precip_mm_24h, units)} last 24 h`);
  if (typeof c.snow_depth_cm === "number" && c.snow_depth_cm > 0) now.push(`${fmt(units).height(c.snow_depth_cm)} snow`);
  if (now.length) out.push(now.join(", "));
  const days = (w.forecast ?? []).filter((d) => d.date >= today).slice(0, 3);
  for (const d of days) {
    const line = dayLine(d, units);
    if (line) out.push(line);
  }
  return out;
}

function dayLine(d: WeatherDay, units: Units): string | undefined {
  const parts = [weekday(d.date)];
  const hi = d.temp_max_c, lo = d.temp_min_c;
  if (typeof hi === "number" && typeof lo === "number") parts.push(`${temp(hi, units, true)}/${temp(lo, units, true)}`);
  else if (typeof hi === "number") parts.push(temp(hi, units, true));
  if (typeof d.precip_mm === "number" && d.precip_mm >= WET_MM) parts.push(rain(d.precip_mm, units));
  return parts.length > 1 ? parts.join(" ") : undefined;
}

// Today's date (YYYY-MM-DD) in `tz`, as the forecast dates are days.
export function todayIn(tz: string | undefined, now = Date.now()): string {
  return new Date(now).toLocaleDateString("en-CA", { timeZone: tz });
}
