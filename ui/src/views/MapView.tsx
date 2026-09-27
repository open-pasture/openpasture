import { Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode, type TextareaHTMLAttributes } from "react";
import type { Map as MLMap, MapMouseEvent } from "maplibre-gl";
import { api, type LonLat, type Paddock, type Polygon } from "../api";
import { behindOf, outOf, store, useStore } from "../store";
import { createMap, fitPolys, onLoad } from "../map/base";
import { addFarmLayers, addTopSlot, Labels, paddockLabels, setBoundary, setEscapes, setPaddocks } from "../map/layers";
import { roughAxis, snapAxis, SweepView } from "../map/sweep";
import { inside } from "../geo";
import { Animals } from "../map/animals";
import { createDraw, current, editPolygon, type Draw } from "../map/draw";
import { mountOverlay, overlayCtx, overlays, Rings, type MapHost, type OverlayHandle } from "../map/overlays";
import { DRAW_ORDER, tools, type ToolCtx, type ToolItem } from "../map/tools";
import { LayersMenu } from "../map/LayersMenu";
import { interleave, PADDOCK_SHEET, paddockSheet, sectionNodes, useSections, views } from "../registry";
import { Button, Input, Menu, Sheet } from "../ui";
import { useUnits } from "../units";
import { typing, useKey } from "../util";
import { HerdPanel } from "./HerdPanel";

type Mode =
  | { k: "idle" }
  | { k: "tool"; id: string }
  | { k: "change"; decisionId: string }
  | { k: "reshape"; paddockId: string };

// The side sheet: a paddock's, or content an overlay opened.
type SheetState = { k: "paddock"; id: string } | { k: "node"; node: ReactNode };

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
  const host = useRef<MapHost>(null);
  const [mode, setMode] = useState<Mode>({ k: "idle" });
  const modeRef = useRef(mode);
  modeRef.current = mode;
  const [sheet, setSheet] = useState<SheetState>();
  const [busy, setBusy] = useState(false);

  const herd = state.herds.find((h) => h.id === herdId);
  const proposed = decisions.find((d) => d.status === "proposed");
  const proposedPad = proposed?.to_paddock_id;

  // Tools this reader may use for this herd, bar tools and the draw group.
  const allTools = tools.use();
  const herdCollars = useMemo(() => collars.filter((c) => c.herd_id === herdId), [collars, herdId]);
  const tctx: ToolCtx = { state, herdId, herd, collars: herdCollars };
  const shown = allTools.filter((t) => !t.when || t.when(tctx));
  const shownKey = shown.map((t) => t.id).join();

  const mounted = useRef<OverlayHandle[]>([]);
  // Overlays hear about a new herd or farm record.
  useEffect(() => {
    mounted.current.forEach((h) => h.update?.());
  }, [map, herdId, state]);

  // the map, once
  useEffect(() => {
    const m = createMap(el.current!, { center: state.farm!.center, zoom: 16 });
    onLoad(m, () => {
      addFarmLayers(m);
      sweep.current = new SweepView(m);
      animals.current = new Animals(m);
      addTopSlot(m);
      labels.current = new Labels(m);
      draw.current = createDraw(m);
      host.current = {
        map: m,
        rings: new Rings(m),
        herdId: () => store.get().herdId,
        openSheet: (node) => setSheet(node === null ? undefined : { k: "node", node }),
      };
      mounted.current = overlays.list().map((o) => mountOverlay(host.current!, o));
      fitPolys(m, store.get().state?.paddocks.map((p) => p.geometry) ?? [], 96);
      setMap(m);
    });
    // An animal opens its page once there is a Herd view to open it in.
    const animalAt = (e: MapMouseEvent) =>
      views.has("herd") && m.getLayer("animals") ? m.queryRenderedFeatures(e.point, { layers: ["animals"] })[0] : undefined;
    m.on("click", (e) => {
      if (modeRef.current.k !== "idle") return;
      const id = animalAt(e)?.properties?.id as string | undefined;
      const c = id ? store.get().collars.find((x) => x.id === id) : undefined;
      if (!c) return;
      const tag = store.get().animals.find((a) => a.id === c.animal_id || a.collar_id === c.id)?.tag;
      location.hash = `/herd/${encodeURIComponent(tag ?? c.id)}`;
    });
    let overAnimal = false;
    m.on("mousemove", (e) => {
      if (!views.has("herd") || modeRef.current.k !== "idle") return;
      const was = overAnimal;
      overAnimal = !!animalAt(e);
      if (overAnimal) m.getCanvas().style.cursor = "pointer";
      else if (was) m.getCanvas().style.cursor = ""; // a paddock under it sets its own, after this
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
      if (modeRef.current.k !== "idle" || animalAt(e)) return;
      const id = e.features?.[0]?.properties?.id as string | undefined;
      if (id) setSheet({ k: "paddock", id });
    });
    return () => {
      mounted.current.forEach((h) => h.destroy());
      mounted.current = [];
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

  const out = outOf(bstat);
  const penKey = out.map((e) => `${e.id}:${e.version}`).join();
  useEffect(() => {
    if (map) setEscapes(map, out.flatMap((e) => (e.geometry ? [e.geometry] : [])));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [map, penKey]);

  // Left behind by a move, or out on their own boundary: marked the same.
  const stragglers = [...new Set([...behindOf(move, collars), ...out.map((e) => e.collar_id)])];
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
    const cur = modeRef.current;
    if (cur.k === "tool") host.current?.rings.set(`tool:${cur.id}`, []);
    draw.current?.clear();
    draw.current?.setMode("static");
    setMode({ k: "idle" });
  }, []);

  const begin = useCallback((t: ToolItem) => {
    if (!draw.current) return;
    setSheet(undefined);
    setMode({ k: "tool", id: t.id });
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

  // Saving an edited shape: a changed proposal or a reshaped paddock. Tools save their own.
  const commit = async () => {
    const g: Polygon | undefined = draw.current ? current(draw.current) : undefined;
    if (!g) return;
    setBusy(true);
    try {
      if (mode.k === "change") await api.respond(mode.decisionId, { action: "modify", geometry: g });
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
    const t = shown.find((t) => t.key === e.key);
    if (t) begin(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shownKey, begin, cancel]);

  const sheetPad = sheet?.k === "paddock" ? state.paddocks.find((p) => p.id === sheet.id) : undefined;
  const active = mode.k === "tool" ? shown.find((t) => t.id === mode.id) : undefined;
  // One context per tool run, so a tool's effects don't re-run on every render.
  const activeCtx = useMemo(() => (active && host.current ? overlayCtx(host.current, `tool:${active.id}`) : undefined), [active?.id, map]); // eslint-disable-line react-hooks/exhaustive-deps
  const bar = shown.filter((t) => t.group === "bar");
  const drawGroup = shown.filter((t) => t.group === "draw");
  const barItems = [
    ...bar.map((t) => ({ order: t.order, node: <Button key={t.id} small onClick={() => begin(t)} title={t.key ? `${t.label} (${t.key.toUpperCase()})` : t.label}>{t.label}</Button> })),
    ...(drawGroup.length ? [{ order: DRAW_ORDER, node: <DrawGroup key="draw" items={drawGroup} onPick={begin} /> }] : []),
  ].sort((a, b) => a.order - b.order);

  return (
    <div className="mapview">
      <div className="mapwrap">
        <div ref={el} className="map" />
        <div className="tools">
          {mode.k === "idle" && barItems.map((b) => b.node)}
          {active && activeCtx && draw.current && map && (
            <Suspense fallback={null}>
              <active.Tool map={map} draw={draw.current} herdId={herdId} ctx={activeCtx} done={cancel} />
            </Suspense>
          )}
          {(mode.k === "change" || mode.k === "reshape") && (
            <form className="toolform" onSubmit={(e) => { e.preventDefault(); void commit(); }}>
              <Button small kind="plain" onClick={cancel}>Cancel</Button>
              <Button small kind="primary" type="submit" disabled={busy}>{mode.k === "reshape" ? "Save" : "Send"}</Button>
            </form>
          )}
        </div>
        {map && host.current && <LayersMenu host={host.current} herdId={herdId} />}
        <Sheet open={!!sheetPad || sheet?.k === "node"} onClose={() => setSheet(undefined)} label={sheetPad ? "Paddock" : "Details"}>
          {sheetPad && <PaddockSheet key={sheetPad.id} p={sheetPad} herdId={herdId} onReshape={() => reshape(sheetPad)} onClose={() => setSheet(undefined)} />}
          {sheet?.k === "node" && sheet.node}
        </Sheet>
      </div>
      <HerdPanel onChange={change} changing={mode.k === "change"} onFocusCollar={(id) => {
        const p = animals.current?.where(id);
        if (p && map) map.easeTo({ center: p, duration: 600 });
      }} onFocusCollars={flyTo} onHoverCollar={(id) => animals.current?.highlight(id)} />
    </div>
  );
}

// One draw tool is a plain button; two or more are the Draw menu.
function DrawGroup({ items, onPick }: { items: ToolItem[]; onPick: (t: ToolItem) => void }) {
  if (items.length === 1) {
    const t = items[0];
    return <Button small onClick={() => onPick(t)} title={t.key ? `${t.label} (${t.key.toUpperCase()})` : t.label}>{t.label}</Button>;
  }
  return (
    <Menu trigger={<span className="btn quiet sm">Draw</span>}
      items={items.map((t) => ({ label: <>{t.label}{t.key && <kbd className="mono dim">{t.key.toUpperCase()}</kbd>}</>, onSelect: () => onPick(t) }))} />
  );
}

function PaddockSheet({ p, herdId, onReshape, onClose }: { p: Paddock; herdId?: string; onReshape: () => void; onClose: () => void }) {
  const props = { paddock: p, herdId };
  const added = sectionNodes(useSections(paddockSheet, props), props);
  const u = useUnits();
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
  return interleave([
    { key: "name", order: PADDOCK_SHEET.name, node: (
      <form onSubmit={(e) => { e.preventDefault(); void save(); }}>
        <Input className="title" value={name} onChange={(e) => setName(e.target.value)} onBlur={save} aria-label="Name" />
      </form>
    ) },
    { key: "facts", order: PADDOCK_SHEET.facts, node: <p className="facts mono">{u.area(p.area_ha)}  {p.status}</p> },
    { key: "notes", order: PADDOCK_SHEET.notes, node: <AutoText value={notes} onChange={setNotes} onBlur={saveNotes} placeholder="Notes" aria-label="Notes" /> },
    { key: "actions", order: PADDOCK_SHEET.actions, node: (
      <div className="acts">
        <Button small onClick={onReshape}>Reshape</Button>
        <Button small kind="plain" className="danger" onClick={remove}>Delete</Button>
      </div>
    ) },
  ], added);
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
