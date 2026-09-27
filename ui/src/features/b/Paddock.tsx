import { useEffect, useState, type ReactNode } from "react";
import type { Paddock } from "../../api";
import { b, type Height, type Weather } from "../../api/b";
import { useStore } from "../../store";
import { herdSignals, layerData, loadSignals } from "../../store/b";
import { useCan } from "../../store/me";
import { NumberField } from "../../ui/NumberField";
import { num, useUnits } from "../../units";
import { day } from "./timeline";
import { todayIn, weatherLines } from "./weather";

type Props = { paddock: Paddock; herdId?: string };

const useTz = () => useStore((s) => s.state?.farm?.timezone);

// "Sep 21" for a calendar date (YYYY-MM-DD), whatever the zone.
const ymd = (d: string) => day(`${d.slice(0, 10)}T12:00:00Z`, "UTC");

const days = (d: number) => `${num(d, d < 10 ? 1 : 0)} d`;
const rest = (d: number) => (d < 1 ? "<1 d" : `${Math.floor(d)} d`);

// Rest, forage, grazing days for the selected herd, NDVI and when it was last grazed.
export function PaddockFacts({ paddock, herdId }: Props) {
  const u = useUnits();
  const tz = useTz();
  const row = layerData.use((l) => l?.paddocks.find((r) => r.paddock_id === paddock.id));
  const sig = herdSignals.use((m) => (herdId ? m[herdId] : undefined));
  useEffect(() => {
    if (herdId) void loadSignals(herdId);
  }, [herdId, paddock.id]);
  const sp = sig?.paddocks.find((p) => p.paddock_id === paddock.id);
  const f = sp?.forage;

  const lines: [string, ReactNode][] = [];
  if (row?.grazing) lines.push(["rest", "grazing now"]);
  else if (row?.rest_days !== undefined) lines.push(["rest", rest(row.rest_days)]);
  if (f?.reason) lines.push(["forage", f.reason]);
  else if (f && f.height_inches !== null && f.available_kg_dm_per_ha !== null) {
    // Standing forage above the residual, over the whole paddock.
    const shown = `${u.height(f.height_inches * 2.54)}  ${u.mass(f.available_kg_dm_per_ha * paddock.area_ha)}`;
    lines.push(["forage", <>{shown}{f.source === "imagery" && <span className="dim">  ndvi</span>}</>]);
  }
  if (typeof sp?.grazing_days === "number") lines.push(["grazing", days(sp.grazing_days)]);
  if (row?.ndvi !== undefined) lines.push(["ndvi", <>{row.ndvi.toFixed(2)}{row.ndvi_at && <span className="dim">  {ymd(row.ndvi_at)}</span>}</>]);
  if (!row?.grazing && row?.last_grazed) lines.push(["grazed", day(row.last_grazed, tz)]);
  if (!lines.length) return null;
  return (
    <ul className="kv bfacts">
      {lines.map(([k, v]) => <li key={k}><span>{k}</span><b>{v}</b></li>)}
    </ul>
  );
}

// "Height [4] in  Sep 24": the last height measured here. Hands and up record a new one.
export function PaddockHeight({ paddock, herdId }: Props) {
  const u = useUnits();
  const tz = useTz();
  const edit = useCan("hand");
  const [latest, setLatest] = useState<Height | null>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    let live = true;
    setLatest(undefined);
    b.heights(paddock.id, 1).then((l) => live && setLatest(l[0] ?? null), () => live && setLatest(null));
    return () => {
      live = false;
    };
  }, [paddock.id]);
  if (latest === undefined || (!edit && !latest)) return null;

  const save = async (cm: number) => {
    setErr(undefined);
    try {
      setLatest(await b.addHeight(paddock.id, { height_cm: cm }));
      if (herdId) void loadSignals(herdId, true);
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <div className="bheight">
      <span className="dim">Height</span>
      {edit
        ? <NumberField value={latest?.height_cm} quantity="height" min={0.5} max={300} onChange={(cm) => void save(cm)} label="Height" width={3} />
        : <span className="mono">{u.height(latest!.height_cm)}</span>}
      {latest && <span className="mono dim">{day(latest.at, tz)}</span>}
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}

// The weather here now and the next three days, from the paddock's land report.
export function PaddockWeather({ paddock }: Props) {
  const u = useUnits();
  const tz = useTz();
  const [w, setW] = useState<Weather>();
  useEffect(() => {
    let live = true;
    setW(undefined);
    b.land(paddock.id).then(
      (l) => {
        const s = l.sections.weather;
        if (live && s?.status === "ok") setW(s as Weather);
      },
      () => {},
    );
    return () => {
      live = false;
    };
  }, [paddock.id]);
  const lines = weatherLines(w, u.units, todayIn(tz));
  if (!lines.length) return null;
  return (
    <ul className="bweather mono">
      {lines.map((l, i) => <li key={i}>{l}</li>)}
    </ul>
  );
}
