// Map features on the map. Zones (slot-zones): exclusions as a red pixel hatch, water, shade and
// hazard areas outlined, roads and neighbour lines dashed, the farm boundary a heavier line.
// Points (slot-points): a pixel icon per water, gate, shade and hazard. Only what is in effect
// now is drawn; a timer redraws when a temporary one starts or ends. Clicking one opens its sheet.

import { createElement } from "react";
import type { Map as MLMap, MapMouseEvent } from "maplibre-gl";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { features } from "../../store/d";
import { hatchBitmap, iconBitmap, ICONS, PR, type IconKind } from "./icons";
import { mapData, nextChange } from "./model";
import { FeatureSheet } from "./sheet";

// map/base.ts C, inlined so this chunk doesn't load MapLibre's module for four colours.
const C = { fg: "#F3F2EA", fg2: "#B7B8A6", fg3: "#838674", warn: "#F0936C", red: "#E5484D" } as const;

const SRC = { zones: "d-zones", lines: "d-lines", points: "d-points" } as const;
// Hit-testing order: icons, then lines, then areas (topmost first).
const CLICKABLE = ["d-icons", "d-hit", "d-zone-fill", "d-excl-fill"];
const LAYERS = ["d-excl-fill", "d-excl-line", "d-zone-fill", "d-zone-line", "d-road", "d-neighbour", "d-farm", "d-sel-zone", "d-sel-line", "d-hit", "d-icons"];

const empty = (): GeoJSON.FeatureCollection => ({ type: "FeatureCollection", features: [] });
const kindIs = (k: string) => ["==", ["get", "kind"], k] as ["==", ["get", string], string];
const byKind = (colors: Record<string, string>, fallback: string) =>
  ["match", ["get", "kind"], ...Object.entries(colors).flat(), fallback] as unknown as string;

// A tool (or a reshape) owns map clicks while its form shows in the tools bar.
function toolRunning(map: MLMap) {
  return !!map.getContainer().parentElement?.querySelector(".tools .toolform, .tools .toolhint");
}

export function mountFeatures(ctx: OverlayCtx): OverlayHandle {
  const { map } = ctx;
  if (!map.hasImage("d-hatch")) map.addImage("d-hatch", hatchBitmap(C.red), { pixelRatio: PR });
  for (const k of Object.keys(ICONS) as IconKind[])
    if (!map.hasImage(`d-${k}`)) map.addImage(`d-${k}`, iconBitmap(ICONS[k], k === "hazard" ? C.warn : C.fg), { pixelRatio: PR });
  for (const id of Object.values(SRC)) map.addSource(id, { type: "geojson", data: empty() });

  const zones = ctx.beforeId("slot-zones");
  const notExclusion = ["!=", ["get", "kind"], "exclusion"] as ["!=", ["get", string], string];
  map.addLayer({ id: "d-excl-fill", type: "fill", source: SRC.zones, filter: kindIs("exclusion"), paint: { "fill-pattern": "d-hatch", "fill-opacity": 0.85 } }, zones);
  map.addLayer({ id: "d-excl-line", type: "line", source: SRC.zones, filter: kindIs("exclusion"), paint: { "line-color": C.red, "line-width": 1, "line-opacity": 0.8 } }, zones);
  map.addLayer({
    id: "d-zone-fill", type: "fill", source: SRC.zones, filter: notExclusion,
    paint: { "fill-color": byKind({ hazard: C.warn }, C.fg), "fill-opacity": ["match", ["get", "kind"], "hazard", 0.08, 0.05] },
  }, zones);
  map.addLayer({
    id: "d-zone-line", type: "line", source: SRC.zones, filter: notExclusion,
    paint: { "line-color": byKind({ water: C.fg2, shade: C.fg3, hazard: C.warn }, C.fg2), "line-width": 1, "line-opacity": 0.8 },
  }, zones);
  map.addLayer({ id: "d-road", type: "line", source: SRC.lines, filter: kindIs("road"), paint: { "line-color": C.fg2, "line-width": 2, "line-dasharray": [3, 2] } }, zones);
  map.addLayer({ id: "d-neighbour", type: "line", source: SRC.lines, filter: kindIs("neighbour_line"), paint: { "line-color": C.fg, "line-width": 1.5, "line-opacity": 0.8, "line-dasharray": [1, 2] } }, zones);
  map.addLayer({ id: "d-farm", type: "line", source: SRC.lines, filter: kindIs("farm_boundary"), paint: { "line-color": C.fg, "line-width": 2.5, "line-opacity": 0.9 } }, zones);
  // The feature whose sheet is open.
  const none = ["==", ["get", "id"], ""] as ["==", ["get", string], string];
  map.addLayer({ id: "d-sel-zone", type: "line", source: SRC.zones, filter: none, paint: { "line-color": C.fg, "line-width": 2 } }, zones);
  map.addLayer({ id: "d-sel-line", type: "line", source: SRC.lines, filter: none, paint: { "line-color": C.fg, "line-width": 3.5 } }, zones);
  // Lines are thin; this wide, unseen copy is what a click finds.
  map.addLayer({ id: "d-hit", type: "line", source: SRC.lines, paint: { "line-color": C.fg, "line-width": 14, "line-opacity": 0 } }, zones);
  map.addLayer({
    id: "d-icons", type: "symbol", source: SRC.points,
    layout: { "icon-image": ["concat", "d-", ["get", "kind"]], "icon-allow-overlap": true, "icon-ignore-placement": true },
  }, ctx.beforeId("slot-points"));

  // Draw what is in effect; come back when the next temporary feature starts or ends.
  let timer: ReturnType<typeof setTimeout> | undefined;
  const draw = () => {
    clearTimeout(timer);
    const list = features.get(), now = Date.now();
    const data = mapData(list, now);
    for (const k of Object.keys(SRC) as (keyof typeof SRC)[]) (map.getSource(SRC[k]) as { setData(d: GeoJSON.FeatureCollection): void } | undefined)?.setData(data[k]);
    const next = nextChange(list, now);
    // setTimeout holds at most ~24.8 days; check again hourly until then.
    if (next !== undefined) timer = setTimeout(draw, Math.min(next - now + 50, 3_600_000));
  };
  const off = features.subscribe(draw);
  draw();

  const select = (id: string | null) => {
    const f = ["==", ["get", "id"], id ?? ""] as ["==", ["get", string], string];
    if (map.getLayer("d-sel-zone")) map.setFilter("d-sel-zone", f);
    if (map.getLayer("d-sel-line")) map.setFilter("d-sel-line", f);
  };

  const hitAt = (e: MapMouseEvent): string | undefined => {
    if (toolRunning(map)) return undefined;
    // An animal on top wins (it opens its own page).
    if (map.getLayer("animals") && map.queryRenderedFeatures(e.point, { layers: ["animals"] }).length) return undefined;
    const layers = CLICKABLE.filter((l) => map.getLayer(l));
    const hits = map.queryRenderedFeatures(e.point, { layers });
    hits.sort((a, b) => CLICKABLE.indexOf(a.layer.id) - CLICKABLE.indexOf(b.layer.id));
    return hits[0]?.properties?.id as string | undefined;
  };
  const onClick = (e: MapMouseEvent) => {
    const id = hitAt(e);
    if (!id) return;
    ctx.openSheet(createElement(FeatureSheet, { key: id, id, select, close: () => ctx.openSheet(null) }));
  };
  let pointing = false;
  const onMove = (e: MapMouseEvent) => {
    const over = !!hitAt(e);
    if (over) map.getCanvas().style.cursor = "pointer";
    else if (pointing) map.getCanvas().style.cursor = "";
    pointing = over;
  };
  map.on("click", onClick);
  map.on("mousemove", onMove);

  return {
    update: draw,
    destroy() {
      off();
      clearTimeout(timer);
      map.off("click", onClick);
      map.off("mousemove", onMove);
      for (const id of [...LAYERS].reverse()) if (map.getLayer(id)) map.removeLayer(id);
      for (const id of Object.values(SRC)) if (map.getSource(id)) map.removeSource(id);
    },
  };
}
