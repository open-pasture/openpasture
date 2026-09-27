// Reading what is drawn, and loading a shape to edit, without loading terra-draw: registries
// and tools import this.
//
// terra-draw edits polygons of one ring only. A shape with holes goes in as its outer ring
// with the holes in the feature's properties, and comes back out with those still inside it.

import type { GeoJSONStoreFeatures } from "terra-draw";
import type { LonLat, Polygon } from "../api";
import { ringAgainst } from "../geo";
import type { Draw, DrawKind } from "./draw";

export type DrawGeometry = GeoJSON.Point | GeoJSON.LineString | GeoJSON.Polygon;

// Handles terra-draw draws around a shape: closing, snapping, coordinate, selection and mid points.
// ("edited" marks the shape itself while it is dragged, so it is not one of them.)
const GUIDES = ["selectionPoint", "midPoint", "closingPoint", "snappingPoint", "coordinatePoint"];

// The shape being drawn or edited, without its handles.
const drawn = (draw: Draw) => draw.getSnapshot().filter((x) => x.properties.mode !== "edit" && !GUIDES.some((k) => x.properties[k]));

// The polygon a feature holds, with the holes it was loaded with that its outer ring still
// reaches (a hole the new edge leaves wholly outside goes; one it cuts stays, see shapeError).
export function polygonOf(f?: GeoJSONStoreFeatures): Polygon | undefined {
  if (!f || f.geometry.type !== "Polygon") return undefined;
  const [outer, ...rest] = f.geometry.coordinates as LonLat[][];
  const holes = (f.properties.holes as LonLat[][] | undefined) ?? [];
  return { type: "Polygon", coordinates: [outer, ...rest, ...holes.filter((h) => ringAgainst(h, outer) !== "out")] };
}

// Why a polygon edited here can't be saved: its outer edge runs through one of its holes.
export function shapeError(p: Polygon): string | undefined {
  const [outer, ...holes] = p.coordinates;
  return holes.some((h) => ringAgainst(h, outer) === "cut") ? "The edge crosses a hole." : undefined;
}

export function current(draw: Draw): Polygon | undefined {
  return polygonOf(drawn(draw).find((x) => x.geometry.type === "Polygon"));
}

export function currentGeometry(draw: Draw): DrawGeometry | undefined {
  const f = drawn(draw).find((x) => ["Point", "LineString", "Polygon"].includes(x.geometry.type));
  return f?.geometry.type === "Polygon" ? polygonOf(f) : (f?.geometry as DrawGeometry | undefined);
}

// Put an existing polygon into the draw store and select it for editing.
export function editPolygon(draw: Draw, g: Polygon, kind: DrawKind) {
  return editShape(draw, g, kind);
}

// The same for any shape a map feature has: a point drags whole, a line's vertices drag.
// Returns the shape's id, or undefined when terra-draw won't take it (nothing is left to edit).
export function editShape(draw: Draw, g: DrawGeometry, kind: DrawKind): string | number | undefined {
  draw.clear();
  draw.setMode("edit");
  const [outer, ...holes] = g.type === "Polygon" ? g.coordinates : [];
  const geometry = g.type === "Polygon" ? { type: "Polygon", coordinates: [outer] } : g;
  const properties = holes.length ? { mode: kind, holes } : { mode: kind };
  const [res] = draw.addFeatures([{ type: "Feature", geometry, properties } as GeoJSONStoreFeatures]);
  if (!res?.valid || res.id === undefined) {
    draw.clear();
    draw.setMode("static");
    return undefined;
  }
  draw.selectFeature(res.id);
  return res.id;
}
