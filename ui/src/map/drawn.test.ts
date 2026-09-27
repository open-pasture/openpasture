import { describe, expect, test } from "bun:test";
import { TerraDraw, TerraDrawPolygonMode, TerraDrawSelectMode } from "terra-draw";
import type { LonLat, Polygon } from "../api";
import type { Draw } from "./draw";
import { current, currentGeometry, editShape, shapeError } from "./drawn";

// terra-draw on a map that is just the plane: lon/lat are the pixels.
function makeDraw(): Draw {
  const el = { addEventListener() {}, removeEventListener() {}, style: {} };
  const adapter = {
    project: (lng: number, lat: number) => ({ x: lng, y: lat }),
    unproject: (x: number, y: number) => ({ lng: x, lat: y }),
    setCursor() {}, getLngLatFromEvent: () => null, setDoubleClickToZoom() {},
    getMapEventElement: () => el, register(cb: { onReady?: () => void }) { cb.onReady?.(); }, unregister() {},
    render() {}, clear() {}, getCoordinatePrecision: () => 9,
  };
  const flags = { feature: { draggable: true, coordinates: { draggable: true, midpoints: true, deletable: true } } };
  const draw = new TerraDraw({
    adapter: adapter as never,
    modes: [
      new TerraDrawPolygonMode({ modeName: "paddock" }),
      new TerraDrawPolygonMode({ modeName: "boundary" }),
      new TerraDrawSelectMode({ modeName: "edit", allowManualDeselection: false, flags: { paddock: flags, boundary: flags } }),
    ],
  });
  draw.start();
  return draw as unknown as Draw;
}

const outer: LonLat[] = [[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]];
// A pond in the middle, and one in the east end.
const pond: LonLat[] = [[-93.6235, 42.0315], [-93.6225, 42.0315], [-93.6225, 42.0321], [-93.6235, 42.0321], [-93.6235, 42.0315]];
const eastPond: LonLat[] = [[-93.6208, 42.0315], [-93.6203, 42.0315], [-93.6203, 42.0321], [-93.6208, 42.0321], [-93.6208, 42.0315]];
const holed: Polygon = { type: "Polygon", coordinates: [outer, pond, eastPond] };

// The outer ring as a farmer drags it: the shape terra-draw now holds.
const reshapeTo = (draw: Draw, id: string | number, ring: LonLat[]) =>
  (draw as unknown as { updateFeatureGeometry(id: string | number, g: GeoJSON.Polygon): void }).updateFeatureGeometry(id, { type: "Polygon", coordinates: [ring] });

describe("editing a shape with holes", () => {
  test("a paddock with holes goes into the editor and comes back with them", () => {
    for (const kind of ["paddock", "boundary"] as const) {
      const draw = makeDraw();
      const id = editShape(draw, holed, kind);
      expect(id).toBeDefined();
      expect(current(draw)?.coordinates).toEqual([outer, pond, eastPond]);
      expect(currentGeometry(draw)).toEqual(holed);
      expect(shapeError(current(draw)!)).toBeUndefined();
    }
  });

  test("moving the outer edge keeps the holes still inside it and drops those it leaves out", () => {
    const draw = makeDraw();
    const id = editShape(draw, holed, "paddock")!;
    // Pull the east edge in west of the east pond.
    const smaller: LonLat[] = [[-93.625, 42.03], [-93.621, 42.03], [-93.621, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]];
    reshapeTo(draw, id, smaller);
    expect(current(draw)?.coordinates).toEqual([smaller, pond]);
    expect(shapeError(current(draw)!)).toBeUndefined();
  });

  test("an edge through a hole is refused with a sentence", () => {
    const draw = makeDraw();
    const id = editShape(draw, holed, "paddock")!;
    // The east edge now runs through the east pond.
    const cut: LonLat[] = [[-93.625, 42.03], [-93.6205, 42.03], [-93.6205, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]];
    reshapeTo(draw, id, cut);
    expect(shapeError(current(draw)!)).toBe("The edge crosses a hole.");
  });

  test("a shape without holes reads back as one ring", () => {
    const draw = makeDraw();
    editShape(draw, { type: "Polygon", coordinates: [outer] }, "paddock");
    expect(current(draw)?.coordinates).toEqual([outer]);
  });

  test("a shape terra-draw can't take is reported, not half-loaded", () => {
    const draw = makeDraw();
    expect(editShape(draw, { type: "Polygon", coordinates: [[outer[0], outer[1], outer[0]]] }, "paddock")).toBeUndefined();
    expect(current(draw)).toBeUndefined();
  });
});
