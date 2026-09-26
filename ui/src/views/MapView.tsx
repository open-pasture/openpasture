import { useCallback, useEffect, useLayoutEffect, useRef, useState, type TextareaHTMLAttributes } from "react";
import type { Map as MLMap, MapMouseEvent } from "maplibre-gl";
import { api, type LonLat, type Paddock, type Polygon } from "../api";
import { behindOf, store, useStore } from "../store";
import { createMap, fitPolys, onLoad } from "../map/base";
import { addFarmLayers, Labels, paddockLabels, setBoundary, setPaddocks } from "../map/layers";
import { roughAxis, snapAxis, SweepView } from "../map/sweep";
import { inside } from "../geo";
import { Animals } from "../map/animals";
import { createDraw, current, editPolygon, type Draw, type DrawKind } from "../map/draw";
import { Button, Input, Sheet } from "../ui";
import { typing, useKey } from "../util";
import { HerdPanel } from "./HerdPanel";

type Mode =
  | { k: "idle" }
  | { k: "draw"; what: DrawKind }
  | { k: "drawn"; what: DrawKind }
  | { k: "change"; decisionId: string }
  | { k: "reshape"; paddockId: string };

export function MapView() {
  const state = useStore((s) => s.state)!;
  const herdId = useStore((s) => s.herdId);
  const collars = useStore((s) => s.collars);
  const bstat = useStore((s) => (s.herdId ? s.boundary[s.herdId] : undefined));
  const decisions = useStore((s) => s.decisions);

  const el = useRef<HTMLDivElement>(null);
  const [map, setMap] = useState<MLMap>();
  const animals = useRef<Animals>(null);
  const labels = useRef<Labels>(null);
  const sweep = useRef<SweepView>(null);
  // Per move: the rough axis and the ground held, fixed when the move is first seen.
  const moveSeen = useRef<{ id: string; rough?: [number, number]; ground: Polygon[] }>(undefined);
  const draw = useRef<Draw>(null);
  const [mode, setMode] = useState<Mode>({ k: "idle" });
  const modeRef = useRef(mode);
  modeRef.current = mode;
  const [sheet, setSheet] = useState<string>();
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);

  const herd = state.herds.find((h) => h.id === herdId);
  // Boundaries go to collars, so a herd without any has nothing to send one to.
  const hasCollars = collars.some((c) => c.herd_id === herdId);
  const proposed = decisions.find((d) => d.status === "proposed");
  const proposedPad = proposed?.to_paddock_id;

  // the map, once
  useEffect(() => {
    const m = createMap(el.current!, { center: state.farm!.center, zoom: 16 });
    onLoad(m, () => {
      addFarmLayers(m);
      sweep.current = new SweepView(m);
      animals.current = new Animals(m);
      labels.current = new Labels(m);
      draw.current = createDraw(m);
      draw.current.on("finish", (id, ctx) => {
        const cur = modeRef.current;
        if (ctx.action === "draw" && cur.k === "draw") {
          draw.current!.setMode("edit");
          draw.current!.selectFeature(id);
          setMode({ k: "drawn", what: cur.what });
        }
      });
      fitPolys(m, store.get().state?.paddocks.map((p) => p.geometry) ?? [], 96);
      setMap(m);
    });
    let hover: number | undefined;
    m.on("mousemove", "paddocks-fill", (e: MapMouseEvent & { features?: GeoJSON.Feature[] }) => {
      if (modeRef.current.k !== "idle") return;
      const f = e.features?.[0];
      if (hover !== undefined) m.setFeatureState({ source: "paddocks", id: hover }, { hover: false });
      hover = f?.id as number | undefined;
      if (hover !== undefined) m.setFeatureState({ source: "paddocks", id: hover }, { hover: true });
      m.getCanvas().style.cursor = "pointer";
    });
    m.on("mouseleave", "paddocks-fill", () => {
      if (hover !== undefined) m.setFeatureState({ source: "paddocks", id: hover }, { hover: false });
      hover = undefined;
      m.getCanvas().style.cursor = "";
    });
    m.on("click", "paddocks-fill", (e: MapMouseEvent & { features?: GeoJSON.Feature[] }) => {
      if (modeRef.current.k !== "idle") return;
      const id = e.features?.[0]?.properties?.id as string | undefined;
      if (id) setSheet(id);
    });
    return () => {
      animals.current?.destroy();
      sweep.current?.destroy();
      m.remove();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // paddocks and labels
  useEffect(() => {
    if (!map) return;
    setPaddocks(map, state.paddocks);
    labels.current?.set(paddockLabels(state.paddocks, herd?.paddock_id, proposedPad));
  }, [map, state.paddocks, herd?.paddock_id, proposedPad]);

  // boundaries. While a move sweeps, the target shows dashed and the active line glides
  // toward it one step at a time; when they meet the dashed line goes.
  const move = bstat?.move;
  const sweeping = move?.status === "sweeping";
  const shownHerd = useRef<string>(undefined);
  const restingActive = useRef<Polygon>(undefined);
  useEffect(() => {
    if (!map) return;
    const sameHerd = shownHerd.current === herdId;
    shownHerd.current = herdId;
    const act = bstat?.active?.geometry;
    let axis: [number, number] | undefined;
    let ground: Polygon[] = [];
    if (sweeping && move) {
      if (moveSeen.current?.id !== move.id) {
        const herdPts = store.get().collars
          .filter((c) => c.herd_id === herdId && c.last_fix && !move.stragglers.includes(c.id))
          .map((c) => animals.current?.where(c.id) ?? c.last_fix!.point);
        // What the herd held when the move began: the boundary before it, else its paddocks.
        const before = restingActive.current;
        const ground = before && herdPts.some((q) => inside(q, before))
          ? [before]
          : state.paddocks.filter((p) => herdPts.some((q) => inside(q, p.geometry))).map((p) => p.geometry);
        moveSeen.current = { id: move.id, rough: roughAxis(herdPts, move.target), ground };
      }
      const seen = moveSeen.current!;
      axis = move.direction ?? (seen.rough && act ? snapAxis(act, seen.rough) : seen.rough);
      ground = seen.ground;
    }
    if (!sweeping) restingActive.current = sameHerd ? act : undefined;
    sweep.current?.set({ active: act, target: sweeping ? move?.target : undefined, axis, ground }, sameHerd);
    animals.current?.showTrails(sweeping);
    setBoundary(map, "pending", bstat?.pending?.geometry);
    setBoundary(map, "proposed", mode.k === "change" ? undefined : bstat?.proposed?.geometry);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [map, bstat, mode.k, herdId, sweeping, move?.id]);

  const stragglers = behindOf(move, collars);
  const stragglerKey = stragglers.join();
  useEffect(() => {
    if (map) animals.current?.stragglers(stragglers);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [map, stragglerKey]);

  const flyTo = (ids: string[]) => {
    const pts = ids.map((id) => animals.current?.where(id)).filter((p): p is LonLat => !!p);
    if (!map || !pts.length) return;
    if (pts.length === 1) return void map.easeTo({ center: pts[0], duration: 700 });
    const lon = pts.map((p) => p[0]), lat = pts.map((p) => p[1]);
    map.fitBounds([[Math.min(...lon), Math.min(...lat)], [Math.max(...lon), Math.max(...lat)]], { padding: 120, maxZoom: map.getZoom(), duration: 700 });
  };

  // animals: membership from the collar list, motion from live fixes
  const collarKey = collars.map((c) => c.id).join();
  useEffect(() => {
    if (!map || !animals.current) return;
    animals.current.set(
      store.get().collars.filter((c) => c.last_fix).map((c) => ({ id: c.id, point: c.last_fix!.point, state: c.state })),
    );
  }, [map, collarKey]);
  useEffect(() => store.onFix((e) => animals.current?.move(e.collar_id, e.fix.point, e.state)), []);

  const cancel = useCallback(() => {
    draw.current?.clear();
    draw.current?.setMode("static");
    setMode({ k: "idle" });
  }, []);

  const begin = useCallback((what: DrawKind) => {
    if (!draw.current) return;
    setSheet(undefined);
    draw.current.clear();
    draw.current.setMode(what);
    if (what === "paddock") setName(nextName(store.get().state?.paddocks ?? []));
    setMode({ k: "draw", what });
  }, []);

  const change = useCallback(() => {
    const d = store.get().decisions.find((x) => x.status === "proposed");
    if (!d?.geometry || !draw.current) return;
    editPolygon(draw.current, d.geometry, "boundary");
    setMode({ k: "change", decisionId: d.id });
  }, []);

  const reshape = (p: Paddock) => {
    if (!draw.current) return;
    setSheet(undefined);
    editPolygon(draw.current, p.geometry, "paddock");
    setMode({ k: "reshape", paddockId: p.id });
  };

  const commit = async () => {
    const g: Polygon | undefined = draw.current ? current(draw.current) : undefined;
    if (!g) return;
    setBusy(true);
    try {
      if (mode.k === "drawn" && mode.what === "paddock") await api.createPaddock({ name: name.trim() || nextName(state.paddocks), geometry: g });
      else if (mode.k === "drawn" && mode.what === "boundary" && herdId) await api.sendBoundary(herdId, { geometry: g });
      else if (mode.k === "change") await api.respond(mode.decisionId, { action: "modify", geometry: g });
      else if (mode.k === "reshape") await api.updatePaddock(mode.paddockId, { geometry: g });
      cancel();
      await store.refresh();
    } finally {
      setBusy(false);
    }
  };

  useKey((e) => {
    if (e.key === "Escape" && modeRef.current.k !== "idle") return cancel();
    if (typing(e) || modeRef.current.k !== "idle") return;
    if (e.key === "p") begin("paddock");
    if (e.key === "b" && hasCollars) begin("boundary");
  }, [hasCollars, begin, cancel]);

  const sheetPad = state.paddocks.find((p) => p.id === sheet);
  const sendLabel = mode.k === "reshape" || (mode.k === "drawn" && mode.what === "paddock") ? "Save" : "Send";
  const hint = mode.k === "draw" ? (mode.what === "paddock" ? "Paddock" : "Boundary") : undefined;

  return (
    <div className="mapview">
      <div className="mapwrap">
        <div ref={el} className="map" />
        <div className="tools">
          {mode.k === "idle" && (
            <>
              <Button small onClick={() => begin("paddock")} title="Draw a paddock (P)">Paddock</Button>
              {hasCollars && <Button small onClick={() => begin("boundary")} title="Draw a boundary (B)">Boundary</Button>}
            </>
          )}
          {hint && (
            <>
              <span className="toolhint">{hint}</span>
              <Button small kind="plain" onClick={cancel}>Cancel</Button>
            </>
          )}
          {(mode.k === "drawn" || mode.k === "change" || mode.k === "reshape") && (
            <form className="toolform" onSubmit={(e) => { e.preventDefault(); void commit(); }}>
              {mode.k === "drawn" && mode.what === "paddock" && (
                <Input autoFocus value={name} onChange={(e) => setName(e.target.value)} aria-label="Name" className="sm" />
              )}
              <Button small kind="plain" onClick={cancel}>Cancel</Button>
              <Button small kind="primary" type="submit" disabled={busy}>{sendLabel}</Button>
            </form>
          )}
        </div>
        <Sheet open={!!sheetPad} onClose={() => setSheet(undefined)} label="Paddock">
          {sheetPad && <PaddockSheet key={sheetPad.id} p={sheetPad} onReshape={() => reshape(sheetPad)} onClose={() => setSheet(undefined)} />}
        </Sheet>
      </div>
      <HerdPanel onChange={change} changing={mode.k === "change"} onFocusCollar={(id) => {
        const p = animals.current?.where(id);
        if (p && map) map.easeTo({ center: p, duration: 600 });
      }} onFocusCollars={flyTo} onHoverCollar={(id) => animals.current?.highlight(id)} />
    </div>
  );
}

function nextName(ps: Paddock[]) {
  let n = ps.length + 1;
  while (ps.some((p) => p.name === `P${n}`)) n++;
  return `P${n}`;
}

function PaddockSheet({ p, onReshape, onClose }: { p: Paddock; onReshape: () => void; onClose: () => void }) {
  const [name, setName] = useState(p.name);
  const [notes, setNotes] = useState(p.notes ?? "");
  const save = async () => {
    if (name.trim() && name !== p.name) {
      await api.updatePaddock(p.id, { name: name.trim() });
      await store.refresh();
    }
  };
  // Standing notes go into every decision's context.
  const saveNotes = async () => {
    const v = notes.trim();
    if (v === (p.notes ?? "")) return;
    await api.updatePaddock(p.id, { notes: (v || null) as string | undefined });
    await store.refresh();
  };
  const remove = async () => {
    await api.deletePaddock(p.id);
    onClose();
    await store.refresh();
  };
  return (
    <>
      <form onSubmit={(e) => { e.preventDefault(); void save(); }}>
        <Input className="title" value={name} onChange={(e) => setName(e.target.value)} onBlur={save} aria-label="Name" />
      </form>
      <p className="facts mono">{p.area_ha.toFixed(1)} ha  {p.status}</p>
      <AutoText value={notes} onChange={setNotes} onBlur={saveNotes} placeholder="Notes" aria-label="Notes" />
      <div className="acts">
        <Button small onClick={onReshape}>Reshape</Button>
        <Button small kind="plain" className="danger" onClick={remove}>Delete</Button>
      </div>
    </>
  );
}

// A textarea that grows with its text.
function AutoText({ value, onChange, ...rest }: { value: string; onChange: (v: string) => void } & Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, "value" | "onChange">) {
  const ref = useRef<HTMLTextAreaElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight + 2}px`;
  }, [value]);
  return <textarea ref={ref} className="input notes" rows={1} spellCheck={false} value={value} onChange={(e) => onChange(e.target.value)} {...rest} />;
}
