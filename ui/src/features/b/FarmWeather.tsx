// The foot of the desktop farm sidebar: the weather where the herd is, now and the next three
// days, each day's low-to-high drawn against the three days' range. From the paddock's land
// report, as the paddock sheet's weather lines; nothing when the report has no weather.

import { useEffect, useState } from "react";
import { b, type Weather, type WeatherDay } from "../../api/b";
import { useStore } from "../../store";
import { useUnits } from "../../units";
import { rain, temp, todayIn } from "./weather";

// A day worth naming its rain: at least a millimetre (as the weather lines).
const WET_MM = 1;

const weekday = (date: string) => new Date(`${date}T12:00:00Z`).toLocaleDateString("en-US", { weekday: "short", timeZone: "UTC" });

export function FarmWeather({ herdId }: { herdId?: string }) {
  const u = useUnits();
  const tz = useStore((s) => s.state?.farm?.timezone);
  // The herd's paddock, else the farm's first.
  const padId = useStore((s) => s.state?.herds.find((h) => h.id === herdId)?.paddock_id ?? s.state?.paddocks[0]?.id);
  const [w, setW] = useState<Weather>();
  useEffect(() => {
    if (!padId) return;
    let live = true;
    b.land(padId).then(
      (l) => {
        const s = l.sections.weather;
        if (live) setW(s?.status === "ok" ? (s as Weather) : undefined);
      },
      () => live && setW(undefined),
    );
    return () => {
      live = false;
    };
  }, [padId]);

  if (!w) return null;
  const now = w.current?.air_temp_c;
  const today = todayIn(tz);
  const days = (w.forecast ?? []).filter((d) => d.date >= today && typeof d.temp_max_c === "number" && typeof d.temp_min_c === "number").slice(0, 3);
  if (typeof now !== "number" && !days.length) return null;
  const lo = Math.min(...days.map((d) => d.temp_min_c!), now ?? Infinity);
  const hi = Math.max(...days.map((d) => d.temp_max_c!), now ?? -Infinity);
  const at = (c: number) => (hi > lo ? ((c - lo) / (hi - lo)) * 100 : 50);
  const wet = typeof w.current?.precip_mm_24h === "number" && w.current.precip_mm_24h >= WET_MM ? w.current.precip_mm_24h : undefined;

  return (
    <section className="fweather" aria-label="Weather">
      {typeof now === "number" && (
        <p className="fwnow">
          <b>{temp(now, u.units)}</b>
          <span className="mono">{wet !== undefined ? `${rain(wet, u.units)} last 24 h` : "now"}</span>
        </p>
      )}
      <ul className="mono">
        {days.map((d: WeatherDay) => (
          <li key={d.date}>
            <span className="fwd">{d.date === today ? "Today" : weekday(d.date)}</span>
            <span className="fwlo">{temp(d.temp_min_c!, u.units, true)}</span>
            <span className="fwr" aria-hidden="true">
              <i style={{ left: `${at(d.temp_min_c!)}%`, right: `${100 - at(d.temp_max_c!)}%` }} />
              {typeof now === "number" && d.date === today && <b style={{ left: `${at(now)}%` }} />}
            </span>
            <span className="fwhi">{temp(d.temp_max_c!, u.units, true)}</span>
            <span className="fwp">{typeof d.precip_mm === "number" && d.precip_mm >= WET_MM ? rain(d.precip_mm, u.units) : ""}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}
