import { useEffect, useMemo, useRef, useState } from "react";
import * as maplibregl from "maplibre-gl";
import type { Map as MLMap, MapMouseEvent } from "maplibre-gl";
import { api, type Paddock, type Polygon } from "../../api";
import { cApi, type Layout, type StripParams, type StripPreview } from "../../api/c";
import { done as taken, peek } from "../../store/c";
import { store, useStore } from "../../store";
import { Button, Segmented } from "../../ui";
import { NumberField } from "../../ui/NumberField";
import { useUnits } from "../../units";
import { centroid } from "../../geo";
import { C, fc, setData } from "../../map/base";
import { ToolFooter, type ToolProps } from "../../map/tools";
import { bearing, facts, handleAt, labelAt, lengthwise, snap, tetherFrom } from "./strips";

type By = "count" | "width" | "days";

// Cut the herd's paddock (or the paddock a sheet opened it on) into strips across a
// draggable orientation handle. Strip 1 is today's, in grass; the rest are faint with their
// number, and clicking one makes it the one to send. Send strip N sends it now as the herd's
// boundary; Save layout keeps the arrangement for the paddock sheet and for schedules.
export function StripTool({ map, herdId, ctx, done }: ToolProps) {
  const state = useStore((s) => s.state)!;
  const herd = state.herds.find((h) => h.id === herdId);
  const collarCount = useStore((s) => s.collars.filter((c) => c.herd_id === herdId).length);
  const u = useUnits();
  const [req] = useState(() => peek("open"));
  useEffect(() => taken("open"), []);
  const paddock = state.paddocks.find((p) => p.id === (req?.paddockId ?? herd?.paddock_id));

  const [deg, setDeg] = useState(() => (paddock ? lengthwise(paddock.geometry) : 0));
  const [by, setBy] = useState<By>("count");
  const [count, setCount] = useState(12);
  const [width, setWidth] = useState(30);
  const [days, setDays] = useState(1);
  const [head, setHead] = useState(herd?.count ?? 0);
  const [preview, setPreview] = useState<StripPreview>();
  const [error, setError] = useState<string>();
  const [sel, setSel] = useState(0);
  const [layout, setLayout] = useState<Layout>();
  const [busy, setBusy] = useState(false);

  const params: StripParams = useMemo(
    () => ({ orientation_deg: deg, head, ...(by === "count" ? { count } : by === "width" ? { width_m: width } : { days }) }),
    [deg, head, by, count, width, days],
  );
  const key = JSON.stringify(params);
  // The params a loaded layout was shown for: no new preview until they change.
  const shownFor = useRef<string>(undefined);

  // A saved layout opened from the paddock sheet: its strips, with today's days.
  useEffect(() => {
    if (!req?.layoutId || !paddock) return;
    let live = true;
    void cApi.applyLayout(req.layoutId, { herd_id: herdId }).then((a) => {
      if (!live) return;
      const p = a.layout.params;
      const nextBy: By = p.count !== undefined ? "count" : p.days !== undefined ? "days" : "width";
      setDeg(p.orientation_deg);
      setBy(nextBy);
      if (p.count !== undefined) setCount(p.count);
      if (p.width_m !== undefined) setWidth(p.width_m);
      if (p.days !== undefined) setDays(p.days);
      setHead(a.head);
      shownFor.current = JSON.stringify({ orientation_deg: p.orientation_deg, head: a.head, ...(nextBy === "count" ? { count: p.count } : nextBy === "width" ? { width_m: p.width_m } : { days: p.days }) });
      setPreview(a);
      setLayout(a.layout);
      setSel(0);
    }, (e: Error) => live && setError(e.message));
    return () => {
      live = false;
    };
  }, [req?.layoutId]); // eslint-disable-line react-hooks/exhaustive-deps

  // Every change asks the server again, a moment after the last one.
  useEffect(() => {
    if (!paddock || shownFor.current === key) return;
    if (req?.layoutId && !shownFor.current) return; // the layout is still loading
    let live = true;
    const t = setTimeout(() => {
      cApi.preview({ paddock_id: paddock.id, herd_id: herdId, ...params }).then(
        (p) => {
          if (!live) return;
          shownFor.current = key;
          setPreview(p);
          setError(undefined);
          setLayout(undefined);
          setSel((s) => Math.min(s, p.strips.length - 1));
        },
        (e: Error) => live && setError(e.message),
      );
    }, 150);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [key, paddock?.id, herdId]); // eslint-disable-line react-hooks/exhaustive-deps

  useStripLayers(map, ctx.beforeId("slot-plan"), paddock, preview, sel, setSel, deg, setDeg);

  if (!paddock)
    return (
      <>
        <span className="toolhint">Strip</span>
        <Button small kind="plain" onClick={done}>Cancel</Button>
      </>
    );

  const canDays = (preview?.forage_kg_dm_per_ha ?? 0) > 0 && (preview?.animal_units ?? 0) > 0;
  const switchBy = (b: By) => {
    if (b === "count" && preview) setCount(preview.strips.length);
    if (b === "width" && preview) setWidth(Math.max(1, Math.round(preview.width_m)));
    if (b === "days") setDays(Math.max(0.5, Math.round((preview?.strips[sel]?.days ?? 1) * 2) / 2));
    setBy(b);
  };
  const strip = preview?.strips[sel];

  const send = async () => {
    if (!strip || !herdId) return;
    setBusy(true);
    try {
      await api.sendBoundary(herdId, { geometry: strip.geometry });
      done();
      await store.refresh();
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const save = async () => {
    setBusy(true);
    try {
      setLayout(await cApi.saveLayout({ paddock_id: paddock.id, herd_id: herdId, ...params }));
    } catch (e) {
      setError((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const options = [
    { value: "count" as const, label: "Count" },
    { value: "width" as const, label: "Width" },
    ...(canDays || by === "days" ? [{ value: "days" as const, label: "Days" }] : []),
  ];
  return (
    <>
      <form className="toolform cstrip" onSubmit={(e) => { e.preventDefault(); void send(); }}>
        <Segmented label="Strips by" value={by} options={options} onChange={switchBy} />
        {by === "count" && <Plain label="Strips" value={count} unit="strips" step={1} min={1} max={200} onChange={setCount} />}
        {by === "width" && <NumberField label="Width" quantity="len" value={width} min={1} max={10_000} onChange={setWidth} width={4} />}
        {by === "days" && <Plain label="Days per strip" value={days} unit="d" step={0.1} min={0.1} max={365} onChange={setDays} />}
        <Plain label="Head" value={head} unit="hd" step={1} min={0} max={100_000} onChange={setHead} />
        <Button small kind="plain" onClick={done}>Cancel</Button>
        <Button small onClick={() => void save()} disabled={busy || !preview || !!layout}>{layout ? "Saved" : "Save layout"}</Button>
        {herdId && collarCount > 0 && (
          <Button small kind="primary" type="submit" disabled={busy || !strip}>Send strip {sel + 1}</Button>
        )}
      </form>
      <div className="cfoot">
        {(error || strip) && <p className={"mono " + (error ? "err" : "cfacts")}>{error ?? facts(u, strip!, preview!.head)}</p>}
        {herdId && preview && (
          <ToolFooter tool="strip" strips={preview.strips.map((s) => s.geometry)} layoutId={layout?.id} herdId={herdId} opts={{}} />
        )}
      </div>
    </>
  );
}

// A plain number with a word after it: "[12] strips", "[250] hd". Saves on Enter or blur.
function Plain({ value, unit, label, step, min, max, onChange }: {
  value: number; unit: string; label: string; step: number; min: number; max: number; onChange: (v: number) => void;
}) {
  const [text, setText] = useState(String(value));
  useEffect(() => setText(String(value)), [value]);
  const commit = () => {
    const v = Number(text.trim().replace(/,/g, ""));
    if (!Number.isFinite(v) || v < min || v > max) return setText(String(value));
    const r = step >= 1 ? Math.round(v) : Math.round(v / step) * step;
    const next = Number(r.toFixed(3));
    if (next !== value) onChange(next);
    else setText(String(value));
  };
  return (
    <span className="numf">
      <input className="input sm mono" inputMode="decimal" spellCheck={false} autoComplete="off" aria-label={label}
        style={{ width: `calc(${Math.max(3, String(max).length - 1)}ch + 26px)` }} value={text}
        onChange={(e) => setText(e.target.value)} onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") { e.preventDefault(); commit(); }
          if (e.key === "Escape") { e.stopPropagation(); setText(String(value)); }
        }} />
      <span className="mono dim">{unit}</span>
    </span>
  );
}

// The strips on the map (slot-plan), their numbers, and the orientation handle: a small
// square past the paddock's edge on a line from its middle, dragged around to turn the strips.
function useStripLayers(
  map: MLMap, beforeId: string, paddock: Paddock | undefined, preview: StripPreview | undefined,
  sel: number, setSel: (i: number) => void, deg: number, setDeg: (d: number) => void,
) {
  const marks = useRef(new Map<number, maplibregl.Marker>());
  const handle = useRef<maplibregl.Marker>(undefined);
  const dragging = useRef(false);
  const pick = useRef(setSel);
  pick.current = setSel;
  const turn = useRef(setDeg);
  turn.current = setDeg;

  useEffect(() => {
    if (!paddock) return;
    for (const id of ["c-strips", "c-axis"]) setData(map, id, fc([]));
    const before = map.getLayer(beforeId) ? beforeId : undefined;
    map.addLayer({
      id: "c-strips-fill", type: "fill", source: "c-strips",
      paint: { "fill-color": ["case", ["get", "sel"], C.grass, C.fg], "fill-opacity": ["case", ["get", "sel"], 0.16, 0.03] },
    }, before);
    map.addLayer({
      id: "c-strips-line", type: "line", source: "c-strips",
      paint: { "line-color": ["case", ["get", "sel"], C.grass, C.fg], "line-width": ["case", ["get", "sel"], 1.5, 1], "line-opacity": ["case", ["get", "sel"], 1, 0.35] },
    }, before);
    map.addLayer({ id: "c-axis", type: "line", source: "c-axis", paint: { "line-color": C.fg, "line-width": 1, "line-opacity": 0.45, "line-dasharray": [2, 3] } }, before);

    const el = document.createElement("div");
    el.className = "chandle";
    el.title = "Drag to turn the strips";
    const m = new maplibregl.Marker({ element: el, draggable: true }).setLngLat(handleAt(paddock.geometry, deg)).addTo(map);
    const mid = centroid(paddock.geometry);
    m.on("dragstart", () => (dragging.current = true));
    m.on("drag", () => {
      const at = m.getLngLat();
      turn.current(snap(bearing(mid, [at.lng, at.lat])));
    });
    m.on("dragend", () => {
      dragging.current = false;
      const at = m.getLngLat();
      const d = snap(bearing(mid, [at.lng, at.lat]));
      turn.current(d);
      m.setLngLat(handleAt(paddock.geometry, d));
    });
    handle.current = m;

    const click = (e: MapMouseEvent & { features?: GeoJSON.Feature[] }) => {
      const i = e.features?.[0]?.properties?.i;
      if (typeof i === "number") pick.current(i);
    };
    const enter = () => (map.getCanvas().style.cursor = "pointer");
    const leave = () => (map.getCanvas().style.cursor = "");
    map.on("click", "c-strips-fill", click);
    map.on("mouseenter", "c-strips-fill", enter);
    map.on("mouseleave", "c-strips-fill", leave);
    const shown = marks.current;
    return () => {
      map.off("click", "c-strips-fill", click);
      map.off("mouseenter", "c-strips-fill", enter);
      map.off("mouseleave", "c-strips-fill", leave);
      m.remove();
      shown.forEach((x) => x.remove());
      shown.clear();
      try {
        for (const id of ["c-axis", "c-strips-line", "c-strips-fill"]) if (map.getLayer(id)) map.removeLayer(id);
        for (const id of ["c-strips", "c-axis"]) if (map.getSource(id)) map.removeSource(id);
        map.getCanvas().style.cursor = "";
      } catch {
        /* the map went first */
      }
    };
  }, [map, paddock?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  // The strips and their numbers.
  useEffect(() => {
    if (!paddock || !map.getSource("c-strips")) return;
    const strips = preview?.strips ?? [];
    setData(map, "c-strips", fc(strips.map((s, i) => ({ type: "Feature", id: i + 1, properties: { i, sel: i === sel }, geometry: s.geometry as Polygon }))));
    const shown = marks.current;
    strips.forEach((s, i) => {
      let mk = shown.get(i);
      if (!mk) {
        const el = document.createElement("div");
        el.className = "plabel cstrip";
        mk = new maplibregl.Marker({ element: el, anchor: "center" }).setLngLat(labelAt(s.geometry, deg)).addTo(map);
        shown.set(i, mk);
      }
      mk.setLngLat(labelAt(s.geometry, deg));
      const el = mk.getElement();
      el.textContent = String(i + 1);
      el.dataset.tone = i === sel ? "grass" : "";
    });
    for (const [i, mk] of shown)
      if (i >= strips.length) {
        mk.remove();
        shown.delete(i);
      }
  }, [map, paddock?.id, preview, sel]); // eslint-disable-line react-hooks/exhaustive-deps

  // The short line from the paddock's edge out to the handle.
  useEffect(() => {
    if (!paddock || !map.getSource("c-axis")) return;
    const end = handleAt(paddock.geometry, deg);
    setData(map, "c-axis", fc([{ type: "Feature", properties: {}, geometry: { type: "LineString", coordinates: [tetherFrom(paddock.geometry, deg), end] } }]));
    if (!dragging.current) handle.current?.setLngLat(end);
  }, [map, paddock?.id, deg]); // eslint-disable-line react-hooks/exhaustive-deps
}
