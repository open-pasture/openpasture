import type { Map as MLMap } from "maplibre-gl";
import { TerraDraw, TerraDrawPolygonMode, TerraDrawSelectMode, type GeoJSONStoreFeatures } from "terra-draw";
import { TerraDrawMapLibreGLAdapter } from "terra-draw-maplibre-gl-adapter";
import type { LonLat, Polygon } from "../api";
import { C } from "./base";

// Two polygon modes that differ only in colour: "paddock" in fg, "boundary" in grass.
// "edit" is the select mode, with vertices draggable and midpoints to add more.

export type DrawKind = "paddock" | "boundary";

const polyStyles = (c: `#${string}`) => ({
  fillColor: c, fillOpacity: 0.08, outlineColor: c, outlineWidth: 1.5,
  closingPointColor: c, closingPointWidth: 4, closingPointOutlineColor: C.ink as `#${string}`, closingPointOutlineWidth: 1,
  coordinatePointColor: c, coordinatePointWidth: 3, coordinatePointOutlineColor: C.ink as `#${string}`, coordinatePointOutlineWidth: 1,
  snappingPointColor: c, snappingPointWidth: 4,
  editedPointColor: c, editedPointWidth: 4,
});

export function createDraw(map: MLMap) {
  const flags = {
    feature: { draggable: true, coordinates: { draggable: true, midpoints: true, deletable: true } },
  };
  const draw = new TerraDraw({
    adapter: new TerraDrawMapLibreGLAdapter({ map }),
    modes: [
      new TerraDrawPolygonMode({ modeName: "paddock", styles: polyStyles(C.fg) }),
      new TerraDrawPolygonMode({ modeName: "boundary", styles: polyStyles(C.grass) }),
      new TerraDrawSelectMode({
        modeName: "edit",
        allowManualDeselection: false,
        flags: { paddock: flags, boundary: flags },
        styles: {
          selectedPolygonColor: C.grass, selectedPolygonFillOpacity: 0.08, selectedPolygonOutlineColor: C.grass, selectedPolygonOutlineWidth: 1.5,
          selectionPointColor: C.grass, selectionPointWidth: 4, selectionPointOutlineColor: C.ink, selectionPointOutlineWidth: 1,
          midPointColor: C.fg, midPointWidth: 2.5, midPointOutlineColor: C.ink, midPointOutlineWidth: 1,
        },
      }),
    ],
  });
  draw.start();
  return draw;
}

export type Draw = ReturnType<typeof createDraw>;

export function polygonOf(f?: GeoJSONStoreFeatures): Polygon | undefined {
  if (!f || f.geometry.type !== "Polygon") return undefined;
  return { type: "Polygon", coordinates: f.geometry.coordinates as LonLat[][] };
}

// Put an existing polygon into the draw store and select it for editing.
export function editPolygon(draw: Draw, g: Polygon, kind: DrawKind) {
  draw.clear();
  draw.setMode("edit");
  const [res] = draw.addFeatures([{ type: "Feature", geometry: g, properties: { mode: kind } } as GeoJSONStoreFeatures]);
  if (res?.valid && res.id !== undefined) draw.selectFeature(res.id);
  return res?.id;
}

export function current(draw: Draw): Polygon | undefined {
  const f = draw.getSnapshot().find((x) => x.geometry.type === "Polygon" && x.properties.mode !== "edit");
  return polygonOf(f);
}
