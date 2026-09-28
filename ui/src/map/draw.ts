import type { Map as MLMap } from "maplibre-gl";
import {
  TerraDraw,
  TerraDrawFreehandMode,
  TerraDrawLineStringMode,
  TerraDrawPointMode,
  TerraDrawPolygonMode,
  TerraDrawRectangleMode,
  TerraDrawSelectMode,
  type GeoJSONStoreFeatures,
} from "terra-draw";
import { TerraDrawMapLibreGLAdapter } from "terra-draw-maplibre-gl-adapter";
import { C } from "./base";
import { finishOnLongPress } from "../features/m/longpress";
import { touchUI } from "../features/m/phone";

export { current, currentGeometry, editPolygon, editShape, polygonOf, shapeError, type DrawGeometry } from "./drawn";

// Drawing modes. Polygons: "paddock" in fg, "boundary" in grass, "exclusion" in red,
// "rect" a dragged rectangle. "point" and "line" for map features, "lasso" a freehand
// shape dragged around animals. "edit" is the select mode, with vertices draggable and
// midpoints to add more. A mode only shows when a tool uses it.

export type DrawKind = "paddock" | "boundary" | "exclusion" | "rect" | "point" | "line" | "lasso";

type Hex = `#${string}`;

// On a touch screen corners are drawn about twice as big, for a fingertip (M).
const T = () => (touchUI() ? 2 : 1);

const polyStyles = (c: Hex) => ({
  fillColor: c, fillOpacity: 0.08, outlineColor: c, outlineWidth: 1.5,
  closingPointColor: c, closingPointWidth: 4 * T(), closingPointOutlineColor: C.ink as Hex, closingPointOutlineWidth: 1,
  coordinatePointColor: c, coordinatePointWidth: 3 * T(), coordinatePointOutlineColor: C.ink as Hex, coordinatePointOutlineWidth: 1,
  snappingPointColor: c, snappingPointWidth: 4 * T(),
  editedPointColor: c, editedPointWidth: 4 * T(),
});

// The colour a finished shape keeps while it is edited.
const EDIT_COLOR: Partial<Record<DrawKind, Hex>> = { exclusion: C.red, point: C.fg, line: C.fg };
const editColor = (f: GeoJSONStoreFeatures): Hex => EDIT_COLOR[f.properties.mode as DrawKind] ?? C.grass;

export function createDraw(map: MLMap) {
  // How near a finger or pointer must come to a corner to take it.
  const pointerDistance = touchUI() ? 56 : 40;
  const flags = {
    feature: { draggable: true, coordinates: { draggable: true, midpoints: true, deletable: true } },
  };
  const draw = new TerraDraw({
    adapter: new TerraDrawMapLibreGLAdapter({ map }),
    modes: [
      new TerraDrawPolygonMode({ modeName: "paddock", pointerDistance, styles: polyStyles(C.fg) }),
      new TerraDrawPolygonMode({ modeName: "boundary", pointerDistance, styles: polyStyles(C.grass) }),
      new TerraDrawPolygonMode({ modeName: "exclusion", pointerDistance, styles: polyStyles(C.red) }),
      new TerraDrawRectangleMode({
        modeName: "rect",
        styles: { fillColor: C.fg, fillOpacity: 0.08, outlineColor: C.fg, outlineWidth: 1.5 },
      }),
      new TerraDrawPointMode({
        modeName: "point",
        styles: { pointColor: C.fg, pointWidth: 5, pointOutlineColor: C.ink, pointOutlineWidth: 1 },
      }),
      new TerraDrawLineStringMode({
        modeName: "line",
        styles: {
          lineStringColor: C.fg, lineStringWidth: 1.5,
          closingPointColor: C.fg, closingPointWidth: 4 * T(), closingPointOutlineColor: C.ink, closingPointOutlineWidth: 1,
          snappingPointColor: C.fg, snappingPointWidth: 4 * T(),
        },
        pointerDistance,
      }),
      new TerraDrawFreehandMode({
        modeName: "lasso",
        drawInteraction: "click-drag",
        autoClose: true,
        styles: {
          fillColor: C.fg, fillOpacity: 0.06, outlineColor: C.fg, outlineWidth: 1,
          closingPointColor: C.fg, closingPointWidth: 0, closingPointOutlineWidth: 0,
        },
      }),
      new TerraDrawSelectMode({
        modeName: "edit",
        pointerDistance,
        allowManualDeselection: false,
        flags: { paddock: flags, boundary: flags, exclusion: flags, rect: flags, line: flags, point: { feature: { draggable: true } } },
        styles: {
          selectedPolygonColor: editColor, selectedPolygonFillOpacity: 0.08, selectedPolygonOutlineColor: editColor, selectedPolygonOutlineWidth: 1.5,
          selectedLineStringColor: editColor, selectedLineStringWidth: 1.5,
          selectedPointColor: editColor, selectedPointWidth: 5, selectedPointOutlineColor: C.ink, selectedPointOutlineWidth: 1,
          selectionPointColor: C.grass, selectionPointWidth: 4 * T(), selectionPointOutlineColor: C.ink, selectionPointOutlineWidth: 1,
          midPointColor: C.fg, midPointWidth: 2.5 * T(), midPointOutlineColor: C.ink, midPointOutlineWidth: 1,
        },
      }),
    ],
  });
  draw.start();
  // By touch, a long press finishes the shape (M).
  finishOnLongPress(map, () => draw.getMode());
  return draw;
}

export type Draw = ReturnType<typeof createDraw>;
