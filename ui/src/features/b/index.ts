// Stream B: what the backend already knows, on the map and in the views. Settings > Farm units;
// the paddock sheet's facts, measured height and weather; the Rest, NDVI, Drought and Flood
// layers; Data > Decisions and Behaviour; behaviour on the animal page; "/" search.

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { api } from "../../api";
import { layers, overlays, type Overlay, type OverlayCtx, type OverlayHandle } from "../../map/overlays";
import { animalPage, dataSections, paddockSheet, search, settingsSections, shortcuts, topbar, views } from "../../registry";
import { store } from "../../store";
import { bboxOf, layerData, searchOpen, show, startB } from "../../store/b";
import "../../styles/b.css";
import { pageHash, pageKeys } from "../k-animals/herd";
import { findAnimals, findPaddocks } from "./find";
import { hasData, type LayerKind } from "./have";
import { KnowledgeSheet, SearchBox } from "./Search";

startB();

// A map overlay whose drawing code loads with the map, not with the app.
function lazyOverlay(id: string, slot: Overlay["slot"], load: () => Promise<(ctx: OverlayCtx) => OverlayHandle>): Overlay {
  return {
    id, slot,
    mount(ctx) {
      let h: OverlayHandle | undefined;
      let gone = false;
      void load().then((mount) => {
        if (!gone) h = mount(ctx);
      });
      return {
        update: () => h?.update?.(),
        destroy() {
          gone = true;
          h?.destroy();
        },
      };
    },
  };
}

// A section that loads on first show, suspending only itself (not the view around it).
function lazySection<P extends object>(load: () => Promise<ComponentType<P>>): ComponentType<P> {
  const L = lazy(() => load().then((c) => ({ default: c })));
  return (p: P) => createElement(Suspense, { fallback: null }, createElement(L as ComponentType<P>, p));
}

// ---- settings ---------------------------------------------------------------------------------

settingsSections.register({
  id: "b-units", group: "Farm", label: "Units", order: 5, minRole: "owner",
  Section: lazySection(() => import("./Farm").then((m) => m.UnitsSetting)),
});

// ---- the paddock sheet --------------------------------------------------------------------------

paddockSheet.register({ id: "b-facts", order: 20, Section: lazySection(() => import("./Paddock").then((m) => m.PaddockFacts)) });
paddockSheet.register({ id: "b-height", order: 25, Section: lazySection(() => import("./Paddock").then((m) => m.PaddockHeight)) });
paddockSheet.register({ id: "b-weather", order: 30, Section: lazySection(() => import("./Paddock").then((m) => m.PaddockWeather)) });

// ---- layers ---------------------------------------------------------------------------------------

const LAYERS: { kind: LayerKind; label: string; order: number }[] = [
  { kind: "rest", label: "Rest", order: 10 },
  { kind: "ndvi", label: "NDVI", order: 20 },
  { kind: "drought", label: "Drought", order: 30 },
  { kind: "flood", label: "Flood", order: 40 },
];
for (const l of LAYERS)
  layers.register({
    id: `b-${l.kind}`, label: l.label, order: l.order,
    available: () => hasData(l.kind, layerData.get()?.paddocks ?? []),
    overlay: lazyOverlay(`b-${l.kind}`, "slot-fill", () => import("./fills").then((m) => (ctx: OverlayCtx) => m.mountFill(ctx, l.kind))),
  });
// Availability follows the values.
layerData.subscribe(() => layers.changed());

// A decision's shape from Data > Decisions, and whatever "/" found.
overlays.register(lazyOverlay("b-focus", "slot-plan", () => import("./focus").then((m) => m.mountFocus)));

// ---- data and the animal page -------------------------------------------------------------------

dataSections.register({ id: "b-decisions", label: "Decisions", order: 40, Section: lazySection(() => import("./Decisions").then((m) => m.Decisions)) });
dataSections.register({ id: "b-behaviour", label: "Behaviour", order: 50, Section: lazySection(() => import("./Behaviour").then((m) => m.Behaviour)) });
animalPage.register({ id: "b-behaviour", order: 60, Section: lazySection(() => import("./Behaviour").then((m) => m.AnimalBehaviour)) });

// ---- search -----------------------------------------------------------------------------------------

shortcuts.register({ id: "b-search", key: "/", run: () => searchOpen.set(true) });
topbar.register({ id: "b-search", order: 5, Item: SearchBox });

// An animal flies the map to where its collar last was; one without a position opens its page.
search.register({
  id: "b-animals", order: 10,
  async find(q) {
    const s = store.get();
    const key = pageKeys(s.animals);
    return findAnimals(q, s.animals, s.collars).flatMap((h) => {
      const c = h.collarId ? s.collars.find((x) => x.id === h.collarId) : undefined;
      if (c?.last_fix && !c.parked_at) return [{ label: h.label, kind: "animal", run: () => show({ kind: "collar", id: c.id }) }];
      const a = h.animalId ? s.animals.find((x) => x.id === h.animalId) : undefined;
      if (a && views.has("herd")) return [{ label: h.label, kind: "animal", run: () => void (location.hash = pageHash(key(a))) }];
      return [];
    });
  },
});

search.register({
  id: "b-paddocks", order: 20,
  async find(q) {
    return findPaddocks(q, store.get().state?.paddocks ?? []).flatMap((h) => {
      const b = bboxOf(h.paddock.geometry);
      return b ? [{ label: h.label, kind: "paddock", run: () => show({ kind: "bbox", bbox: b }) }] : [];
    });
  },
});

search.register({
  id: "b-knowledge", order: 30,
  async find(q) {
    if (q.trim().length < 3) return [];
    const hits = await api.knowledge(q.trim(), 5);
    return hits.map((k) => ({ label: k.title, kind: k.kind, run: () => show({ kind: "sheet", node: createElement(KnowledgeSheet, { key: k.id, hit: k }) }) }));
  },
});
