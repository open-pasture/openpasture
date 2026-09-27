import { useEffect, useMemo, useRef, useState } from "react";
import type { Map as MLMap } from "maplibre-gl";
import { api, ApiError, downloadBlob, type HealthSeries, type PastureRow, type SqlResult, type Track } from "../api";
import { DATA, dataSections, guarded, interleave } from "../registry";
import { useStore } from "../store";
import { useCan } from "../store/me";
import { C, createMap, fc, fitPolys, onLoad, setData } from "../map/base";
import { addFarmLayers, Labels, paddockLabels, setBoundary, setPaddocks } from "../map/layers";
import { Animals } from "../map/animals";
import { positionsAt } from "./replay";
import { Button, Icon, Menu, Segmented } from "../ui";
import { useUnits, type Fmt } from "../units";

type RangeKey = "24h" | "7d" | "30d";
const SPAN: Record<RangeKey, number> = { "24h": 86400e3, "7d": 7 * 86400e3, "30d": 30 * 86400e3 };

export function DataView() {
  const herdId = useStore((s) => s.herdId);
  const [range, setRange] = useState<RangeKey>("24h");
  // SQL and export come from the analytics crate; show them once it answers.
  const [sqlOk, setSqlOk] = useState(false);
  // The console is a POST, which managers and owners make.
  const sqlRole = useCan("manager");
  useEffect(() => {
    api.sql("select count(*) from herds").then(() => setSqlOk(true), (e) => setSqlOk(!(e instanceof ApiError && e.status === 404)));
  }, []);
  const { from, to } = useMemo(() => {
    const t = Date.now();
    return { from: new Date(t - SPAN[range]).toISOString(), to: new Date(t).toISOString() };
  }, [range]);
  const added = dataSections.use().map((d) => ({
    key: `section:${d.id}`, order: d.order,
    // A section with nothing to show renders nothing, and its label goes with it (CSS).
    node: <section className="dsec" aria-label={d.label}><h2>{d.label}</h2><div className="dsec-body">{guarded(d.id, <d.Section herdId={herdId} from={from} to={to} />)}</div></section>,
  }));

  return (
    <div className="data">
      <div className="dbar">
        <Segmented label="Range" value={range} onChange={setRange}
          options={[{ value: "24h", label: "24 h" }, { value: "7d", label: "7 d" }, { value: "30d", label: "30 d" }]} />
        {sqlOk && <Menu align="right" trigger={<span className="btn quiet sm">Export</span>}
          items={EXPORTS.map(([table, format]) => ({ label: <span className="mono">{table}.{format}</span>, onSelect: () => download(table, format, from, to) }))} />}
      </div>
      {interleave([
        { key: "health", order: DATA.health, node: <Health herdId={herdId} from={from} to={to} /> },
        { key: "replay", order: DATA.replay, node: <Replay herdId={herdId} from={from} to={to} /> },
        { key: "pasture", order: DATA.pasture, node: <Pasture herdId={herdId} /> },
        { key: "sql", order: DATA.sql, node: sqlOk && sqlRole && <Sql /> },
      ], added)}
    </div>
  );
}

const EXPORTS: [string, "csv" | "geojson" | "parquet"][] = [
  ["fixes", "csv"], ["fixes", "geojson"], ["fixes", "parquet"], ["tracks", "geojson"], ["cues", "csv"], ["acks", "csv"],
  ["decisions", "csv"], ["paddocks", "geojson"], ["boundaries", "geojson"],
];

function download(table: string, format: "csv" | "geojson" | "parquet", from: string, to: string) {
  downloadBlob(api.exportUrl(table, format, { from, to }), `${table}.${format}`).catch(() => {});
}

// ---- health ----------------------------------------------------------------

type Metric = { key: "fix_rate" | "acc_p50" | "battery"; label: string; fmt: (v: number, u: Fmt) => string; unit?: [number, number] };
const METRICS: Metric[] = [
  { key: "fix_rate", label: "fix rate", fmt: (v) => `${Math.round(v * 100)}%`, unit: [0, 1] },
  { key: "acc_p50", label: "accuracy", fmt: (v, u) => u.len(v) },
  { key: "battery", label: "battery", fmt: (v) => `${Math.round(v * 100)}%`, unit: [0, 1] },
];

function Health({ herdId, from, to }: { herdId?: string; from: string; to: string }) {
  const u = useUnits();
  const [series, setSeries] = useState<HealthSeries[] | null>([]);
  useEffect(() => {
    if (herdId) void api.health({ herd_id: herdId, from, to }).then(setSeries).catch(() => setSeries(null));
  }, [herdId, from, to]);

  // Herd mean per bucket: one line per chart. The x axis is the span the
  // server bucketed, so every chart shares the same start and end.
  const { mean, span } = useMemo(() => {
    const s = series ?? [];
    const bucket = (s[0]?.bucket_s ?? 60) * 1000;
    const now = Date.now();
    // Whole buckets only: the one still filling reads low until its reports land.
    const idx = (s[0]?.points ?? []).map((p, i) => [Date.parse(p.t), i] as const).filter(([t]) => t + bucket <= now);
    const mean = METRICS.map((m) => ({
      m,
      // Buckets with no reading are skipped rather than drawn as zero.
      pts: idx.flatMap(([t, i]) => {
        const vals = s.map((x) => x.points[i]?.[m.key]).filter((v): v is number => typeof v === "number");
        return vals.length ? [{ t: t + bucket / 2, v: vals.reduce((a, b) => a + b, 0) / vals.length }] : [];
      }),
    }));
    // Every chart shares one axis: first reading to the last whole bucket.
    const ts = mean.flatMap((x) => x.pts.map((p) => p.t));
    const span: [number, number] = ts.length ? [Math.min(...ts), Math.max(...ts)] : [0, 0];
    return { mean, span };
  }, [series]);

  // One row per herd with collars, drawn from the first report on.
  if (!series?.length) return null;
  return (
    <div className="charts">
      {mean.map(({ m, pts }) => (
        <figure key={m.key}>
          <figcaption><span>{m.label}</span><b>{pts.length ? m.fmt(pts[pts.length - 1].v, u) : "–"}</b></figcaption>
          <Line pts={pts} span={span} unit={m.unit} />
        </figure>
      ))}
    </div>
  );
}

const hhmm = (t: number) => new Date(t).toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", hour12: false });
const day = (t: number) => new Date(t).toLocaleDateString(undefined, { month: "short", day: "numeric" });

function Line({ pts, span, unit }: { pts: { t: number; v: number }[]; span: [number, number]; unit?: [number, number] }) {
  const W = 300, H = 72;
  const [t0, t1] = span;
  const ok = pts.length >= 2 && t1 > t0;
  let lo = unit?.[0] ?? 0, hi = unit?.[1] ?? Math.max(...pts.map((p) => p.v), 0) * 1.25;
  if (hi - lo < 1e-9) hi = lo + 1;
  const x = (t: number) => ((t - t0) / (t1 - t0)) * W;
  const y = (v: number) => H - 1 - ((v - lo) / (hi - lo)) * (H - 2);
  const d = ok ? pts.map((p, i) => `${i ? "L" : "M"}${x(p.t).toFixed(1)} ${y(p.v).toFixed(1)}`).join("") : "";
  const long = t1 - t0 > 36 * 3600e3;
  return (
    <div className="linewrap">
      <svg className="chart" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none">
        <line x1={0} x2={W} y1={H - 0.5} y2={H - 0.5} className="axis" />
        {ok && <path d={d} className="stroke" vectorEffect="non-scaling-stroke" />}
      </svg>
      <div className="ticks mono">{t1 > t0 && <><span>{long ? day(t0) : hhmm(t0)}</span><span>{long ? day(t1) : hhmm(t1)}</span></>}</div>
    </div>
  );
}

// ---- replay ----------------------------------------------------------------

function Replay({ herdId, from, to }: { herdId?: string; from: string; to: string }) {
  const state = useStore((s) => s.state)!;
  const bstat = useStore((s) => (s.herdId ? s.boundary[s.herdId] : undefined));
  const el = useRef<HTMLDivElement>(null);
  const [map, setMap] = useState<MLMap>();
  const animals = useRef<Animals>(null);
  const [tracks, setTracks] = useState<Track[]>([]);
  const [heat, setHeat] = useState<[number, number, number][]>([]);
  const [have, setHave] = useState({ tracks: true, heat: true });
  const [layer, setLayer] = useState<"tracks" | "heat">("tracks");
  const [pos, setPos] = useState(1000);
  const [playing, setPlaying] = useState(false);
  // Dragging the scrubber jumps the animals; playing eases them.
  const scrubbing = useRef(false);

  useEffect(() => {
    const m = createMap(el.current!, { center: state.farm!.center, zoom: 16, dim: 0.6 });
    onLoad(m, () => {
      addFarmLayers(m);
      setPaddocks(m, state.paddocks);
      new Labels(m).set(paddockLabels(state.paddocks));
      setData(m, "tracks", fc([]));
      setData(m, "heat", fc([]));
      m.addLayer({ id: "tracks", type: "line", source: "tracks", paint: { "line-color": C.grass, "line-opacity": 0.2, "line-width": 1 } });
      m.addLayer({
        id: "heat", type: "heatmap", source: "heat", layout: { visibility: "none" },
        paint: {
          "heatmap-radius": 18, "heatmap-intensity": 0.6, "heatmap-opacity": 0.85,
          "heatmap-color": ["interpolate", ["linear"], ["heatmap-density"], 0, "rgba(0,0,0,0)", 0.2, "rgba(29,48,22,.6)", 0.45, "#4F8A2A", 0.75, "#9FD760", 1, "#F3F2EA"],
        },
      });
      animals.current = new Animals(m);
      fitPolys(m, state.paddocks.map((p) => p.geometry), 40);
      setMap(m);
    });
    return () => m.remove();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    map?.resize();
  }, [map, have]);

  useEffect(() => {
    if (map) setBoundary(map, "active", bstat?.active?.geometry);
  }, [map, bstat]);

  useEffect(() => {
    if (!herdId) return;
    void api.tracks({ herd_id: herdId, from, to, max_points: 300 })
      .then((t) => { setTracks(t); setHave((h) => ({ ...h, tracks: true })); })
      .catch(() => { setTracks([]); setHave((h) => ({ ...h, tracks: false })); });
    void api.heatmap({ herd_id: herdId, from, to })
      .then((x) => { setHeat(x); setHave((h) => ({ ...h, heat: true })); })
      .catch(() => { setHeat([]); setHave((h) => ({ ...h, heat: false })); });
  }, [herdId, from, to]);

  useEffect(() => {
    if (!map) return;
    setData(map, "tracks", fc(tracks.map((t) => ({ type: "Feature", properties: {}, geometry: { type: "LineString", coordinates: t.points.map((p) => [p[0], p[1]]) } }))));
    setData(map, "heat", fc(heat.map(([x, y, w]) => ({ type: "Feature", properties: { w }, geometry: { type: "Point", coordinates: [x, y] } }))));
  }, [map, tracks, heat]);

  useEffect(() => {
    if (!map) return;
    map.setLayoutProperty("heat", "visibility", layer === "heat" ? "visible" : "none");
    map.setLayoutProperty("tracks", "visibility", layer === "tracks" ? "visible" : "none");
  }, [map, layer]);

  // Scrub over the time the tracks actually cover, not the whole range.
  const span = useMemo(() => {
    const ts = tracks.flatMap((t) => (t.points.length ? [t.points[0][2], t.points[t.points.length - 1][2]] : []));
    return ts.length ? [Math.min(...ts) * 1000, Math.max(...ts) * 1000] : [Date.parse(from), Date.parse(to)];
  }, [tracks, from, to]);
  const [t0, t1] = span;
  const at = t0 + ((t1 - t0) * pos) / 1000;

  // Who is in the replay: once per set of tracks. Where they are: at each step.
  useEffect(() => {
    animals.current?.set(positionsAt(tracks, at / 1000));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tracks, map]);
  useEffect(() => {
    animals.current?.moveMany(positionsAt(tracks, at / 1000), scrubbing.current);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [at]);

  useEffect(() => {
    if (!playing) return;
    scrubbing.current = false;
    const t = setInterval(() => setPos((p) => (p >= 1000 ? (setPlaying(false), 1000) : p + 4)), 40);
    return () => clearInterval(t);
  }, [playing]);

  const when = new Date(at);
  return (
    <div className="replay" hidden={!have.tracks && !have.heat}>
      <div ref={el} className="map" />
      <div className="over" hidden={!have.tracks || !have.heat}><Segmented label="Layer" value={layer} onChange={setLayer} options={[{ value: "tracks", label: "Tracks" }, { value: "heat", label: "Heat" }]} /></div>
      <div className="scrub">
        <button type="button" className="play" aria-label={playing ? "Pause" : "Play"} onClick={() => { if (pos >= 1000) setPos(0); setPlaying(!playing); }}>
          {playing ? <span className="pause" /> : <Icon name="chev" size={7} />}
        </button>
        <input type="range" min={0} max={1000} value={pos} onChange={(e) => { scrubbing.current = true; setPlaying(false); setPos(Number(e.target.value)); }} aria-label="Time" />
        <span className="mono dim">{when.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false })}</span>
      </div>
    </div>
  );
}

// ---- pasture ---------------------------------------------------------------

function Pasture({ herdId }: { herdId?: string }) {
  const u = useUnits();
  const herdPad = useStore((s) => s.state?.herds.find((h) => h.id === s.herdId)?.paddock_id);
  const [rows, setRows] = useState<PastureRow[] | null>([]);
  useEffect(() => {
    void api.pasture(herdId).then(setRows).catch(() => setRows(null));
  }, [herdId, herdPad]);
  if (!rows?.length) return null;
  const now = Date.now();
  const ndvi = rows.some((r) => r.ndvi !== null);
  // Rest: "now" where the herd is, else the time since the herd last spent a real share of a day there.
  const rest = (r: PastureRow) => {
    if (r.paddock_id === herdPad) return "now";
    if (!r.last_grazed) return "–";
    const h = (now - Date.parse(r.last_grazed)) / 3600e3;
    return h < 1 ? "<1 h" : h < 48 ? `${Math.floor(h)} h` : `${Math.floor(h / 24)} d`;
  };
  return (
    <div className="pasture">
      <table className="tbl">
        <thead><tr><th>paddock</th><th>rest</th><th>grazed</th><th>AU·d/{u.unitLabel("area")}</th>{ndvi && <th>ndvi</th>}</tr></thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.paddock_id}>
              <td>{r.name}</td>
              <td className={r.paddock_id === herdPad ? "ok" : undefined}>{rest(r)}</td>
              <td>{r.grazing_days ? `${r.grazing_days} d` : "–"}</td>
              <td>{r.pressure ? auDays(u.toDisplay(r.pressure, "density")) : "–"}</td>
              {ndvi && <td>{r.ndvi === null ? "–" : r.ndvi.toFixed(2)}</td>}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

// AU-days per area, converted like a density: one decimal under 10.
const auDays = (v: number) => v.toFixed(v < 10 ? 1 : 0);

// ---- sql -------------------------------------------------------------------

function Sql() {
  const [q, setQ] = useState("select name, battery, state, last_seen from collars order by name");
  const [res, setRes] = useState<SqlResult>();
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  const run = async () => {
    setBusy(true);
    setErr(undefined);
    try {
      setRes(await api.sql(q));
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  useEffect(() => {
    void run();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return (
    <div className="sql">
      <div className="editor">
        <textarea className="mono" value={q} spellCheck={false} aria-label="SQL" rows={Math.max(2, q.split("\n").length)}
          onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); void run(); } }} />
        <div className="run">
          {err ? <span className="mono err">{err}</span> : res && <span className="mono dim">{res.rows.length} rows  {res.ms} ms</span>}
          <Button small kind="primary" disabled={busy} onClick={run} title="⌘↵">Run</Button>
        </div>
      </div>
      {res && (
        <div className="result">
          <table className="tbl">
            <thead><tr>{res.columns.map((c) => <th key={c}>{c}</th>)}</tr></thead>
            <tbody>
              {res.rows.map((r, i) => (
                <tr key={i}>{r.map((v, j) => {
                  const t = v === null || v === undefined ? "" : typeof v === "object" ? JSON.stringify(v) : String(v);
                  return <td key={j} title={t.length > 24 ? t : undefined}>{t}</td>;
                })}</tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
