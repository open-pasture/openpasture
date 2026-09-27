// What the map shows on request: a decision's shape drawn faint (slot-plan, over the boundaries)
// until Esc, and a found animal, paddock or knowledge entry (fly there, ring it, open its sheet).

import { bbox } from "../../geo";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { store } from "../../store";
import { focus, ghost } from "../../store/b";
import { TONE } from "./ramp";

const SRC = "b-ghost";
const LAYERS = ["b-ghost-fill", "b-ghost-line"];
// How long a found animal keeps its ring, and the zoom it is shown at (close enough to pick it
// out of the herd).
const RING_MS = 6000;
const ANIMAL_ZOOM = 18;

export function mountFocus(ctx: OverlayCtx): OverlayHandle {
  const { map } = ctx;
  map.addSource(SRC, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
  const plan = ctx.beforeId("slot-plan");
  map.addLayer({ id: LAYERS[0], type: "fill", source: SRC, paint: { "fill-color": TONE.fg, "fill-opacity": 0.06 } }, plan);
  map.addLayer({ id: LAYERS[1], type: "line", source: SRC, paint: { "line-color": TONE.fg, "line-width": 1.5, "line-opacity": 0.85, "line-dasharray": [1.5, 2] } }, plan);

  let flown: string | undefined;
  const drawGhost = () => {
    const g = ghost.get();
    (map.getSource(SRC) as { setData(d: GeoJSON.FeatureCollection): void } | undefined)?.setData({
      type: "FeatureCollection",
      features: g ? [{ type: "Feature", properties: {}, geometry: g.geometry }] : [],
    });
    if (g && g.id !== flown) {
      const b = bbox([g.geometry]);
      if (b) ctx.flyTo(b);
    }
    flown = g?.id;
  };
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape" && ghost.get()) ghost.set(null);
  };
  window.addEventListener("keydown", onKey);

  let done = 0;
  let unring: ReturnType<typeof setTimeout> | undefined;
  const run = () => {
    const f = focus.get();
    if (!f || f.seq === done) return;
    done = f.seq;
    focus.set(null);
    if (f.kind === "bbox") ctx.flyTo(f.bbox);
    else if (f.kind === "sheet") ctx.openSheet(f.node);
    else {
      const at = ctx.positions().get(f.id)?.fix.point ?? store.get().collars.find((c) => c.id === f.id)?.last_fix?.point;
      if (!at) return;
      map.easeTo({ center: at, zoom: Math.max(map.getZoom(), ANIMAL_ZOOM), duration: 700 });
      ctx.highlight([f.id], "fg");
      clearTimeout(unring);
      unring = setTimeout(() => ctx.highlight([]), RING_MS);
    }
  };

  const offGhost = ghost.subscribe(drawGhost);
  const offFocus = focus.subscribe(run);
  // After the map view has placed itself (it fits the paddocks right after overlays mount).
  const first = setTimeout(() => {
    drawGhost();
    run();
  }, 0);

  return {
    destroy() {
      clearTimeout(first);
      clearTimeout(unring);
      offGhost();
      offFocus();
      window.removeEventListener("keydown", onKey);
      // A faint shape belongs to this visit to the map.
      ghost.set(null);
      try {
        for (const id of [...LAYERS].reverse()) if (map.getLayer(id)) map.removeLayer(id);
        if (map.getSource(SRC)) map.removeSource(SRC);
      } catch {
        /* the map went first */
      }
    },
  };
}
