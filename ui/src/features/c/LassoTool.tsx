import { useEffect, useState } from "react";
import type { MapMouseEvent, MapTouchEvent } from "maplibre-gl";
import { api, type Collar, type LonLat } from "../../api";
import { done as taken, peek } from "../../store/c";
import { collarLabel, store, useStore } from "../../store";
import { Button, Menu } from "../../ui";
import { Table, type Column } from "../../ui/Table";
import { age, useNow } from "../../util";
import { views } from "../../registry";
import { selectUrl } from "../k-animals/herd";
import type { ToolProps } from "../../map/tools";
import type { OverlayCtx } from "../../map/overlays";
import { caughtIn, drag, dragTouch, lassoLayers, removeLasso, setLasso } from "./freehand";

interface Caught { ids: string[]; ring: LonLat[] }

// Drag around animals to select them, then move them to another herd or list them (in the Herd
// view, selected there, when it exists).
// Opened with l (drag to draw) or by shift-dragging on the map (it opens with that selection).
export function LassoTool({ map, ctx, done }: ToolProps) {
  const [sel, setSel] = useState<Caught | undefined>(() => peek("lasso"));
  useEffect(() => taken("lasso"), []);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  const herds = useStore((s) => s.state?.herds) ?? [];
  const collars = useStore((s) => s.collars);

  useEffect(() => {
    lassoLayers(map, ctx.beforeId("slot-top"));
    return () => {
      ctx.highlight([]);
      removeLasso(map);
    };
  }, [map, ctx]);

  // With nothing selected a drag draws the lasso; with a selection it stays on the map, ringed.
  useEffect(() => {
    if (sel) {
      setLasso(map, { ring: sel.ring });
      ctx.highlight(sel.ids, "fg");
      return;
    }
    setLasso(map);
    ctx.highlight([]);
    const el = map.getCanvasContainer();
    el.classList.add("classo-draw");
    let stop: (() => void) | undefined;
    const down = (e: MapMouseEvent) => {
      if (e.originalEvent.button !== 0) return;
      stop = drag(map, e, (ring) => ring && setSel({ ring, ids: caughtIn(ring, ctx.positions()) }));
    };
    // By touch too (M): one finger draws the lasso.
    const touch = (e: MapTouchEvent) => {
      if (e.originalEvent.touches.length !== 1) return;
      stop = dragTouch(map, e, (ring) => ring && setSel({ ring, ids: caughtIn(ring, ctx.positions()) }));
    };
    map.on("mousedown", down);
    map.on("touchstart", touch);
    return () => {
      map.off("mousedown", down);
      map.off("touchstart", touch);
      stop?.();
      el.classList.remove("classo-draw");
    };
  }, [map, ctx, sel]);

  if (!sel)
    return (
      <>
        <span className="toolhint">Lasso</span>
        <Button small kind="plain" onClick={done}>Cancel</Button>
      </>
    );

  const picked = sel.ids.map((id) => collars.find((c) => c.id === id)).filter((c): c is Collar => !!c);
  const others = herds.filter((h) => picked.some((c) => c.herd_id !== h.id));

  const moveTo = async (herdId: string) => {
    setBusy(true);
    setErr(undefined);
    const animals = store.get().animals;
    try {
      await each(picked.filter((c) => c.herd_id !== herdId), 6, async (c) => {
        await api.updateCollar(c.id, { herd_id: herdId });
        const a = animals.find((x) => x.id === c.animal_id || x.collar_id === c.id);
        if (a && a.herd_id !== herdId) await api.updateAnimal(a.id, { herd_id: herdId });
      });
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
      await store.refresh();
    }
  };

  return (
    <div className="toolform">
      <span className="mono cnum">{picked.length} selected</span>
      {picked.length > 0 && others.length > 0 && (
        <Menu trigger={<span className={"btn quiet sm" + (busy ? " busy" : "")}>Move to herd</span>}
          items={others.map((h) => ({ label: h.name, onSelect: () => void moveTo(h.id) }))} />
      )}
      {picked.length > 0 && <Button small onClick={() => (views.has("herd")
        ? void (location.hash = selectUrl(picked.map((c) => c.id)))
        : ctx.openSheet(<Picked ids={sel.ids} ctx={ctx} />))}>Show in table</Button>}
      <Button small kind="plain" onClick={() => { ctx.openSheet(null); setErr(undefined); setSel(undefined); }}>Clear</Button>
      {err && <span className="mono err cerr">{err}</span>}
    </div>
  );
}

// Run f over items, at most n at once.
async function each<T>(items: T[], n: number, f: (t: T) => Promise<void>) {
  let i = 0;
  const worker = async () => {
    while (i < items.length) await f(items[i++]);
  };
  await Promise.all(Array.from({ length: Math.min(n, items.length) }, worker));
}

interface Row { c: Collar; label: string; herd: string }

// The selection as a table; a row flies to its animal.
function Picked({ ids, ctx }: { ids: string[]; ctx: OverlayCtx }) {
  const collars = useStore((s) => s.collars);
  const animals = useStore((s) => s.animals);
  const herds = useStore((s) => s.state?.herds) ?? [];
  const now = useNow(5000);
  const rows: Row[] = ids
    .map((id) => collars.find((c) => c.id === id))
    .filter((c): c is Collar => !!c)
    .map((c) => ({ c, label: collarLabel(c, animals), herd: herds.find((h) => h.id === c.herd_id)?.name ?? "" }));
  const cols: Column<Row>[] = [
    { id: "tag", label: "Tag", width: 80, sort: (a, b) => a.label.localeCompare(b.label, undefined, { numeric: true }), cell: (r) => r.label },
    { id: "herd", label: "Herd", width: 110, sort: (a, b) => a.herd.localeCompare(b.herd), cell: (r) => r.herd },
    { id: "battery", label: "Battery", width: 76, sort: (a, b) => (a.c.battery ?? -1) - (b.c.battery ?? -1), cell: (r) => (r.c.battery === undefined ? "–" : `${Math.round(r.c.battery * 100)}%`) },
    { id: "seen", label: "Seen", cell: (r) => age(r.c.last_seen, now) },
  ];
  return (
    <div className="cpicked">
      <Table rows={rows} columns={cols} rowKey={(r) => r.c.id} height={Math.min(420, 37 * rows.length + 40)}
        initialSort={{ col: "tag", dir: "asc" }}
        onRowClick={(r) => { const p = ctx.positions().get(r.c.id)?.fix.point; if (p) ctx.flyTo(p); }} />
    </div>
  );
}
