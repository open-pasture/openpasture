// Reading what is drawn, without loading terra-draw: registries and tools import this.

import type { GeoJSONStoreFeatures } from "terra-draw";
import type { LonLat, Polygon } from "../api";
import type { Draw } from "./draw";

export type DrawGeometry = GeoJSON.Point | GeoJSON.LineString | GeoJSON.Polygon;

// Handles terra-draw draws around a shape: closing, snapping, coordinate, selection and mid points.
// ("edited" marks the shape itself while it is dragged, so it is not one of them.)
const GUIDES = ["selectionPoint", "midPoint", "closingPoint", "snappingPoint", "coordinatePoint"];

// The shape being drawn or edited, without its handles.
const drawn = (draw: Draw) => draw.getSnapshot().filter((x) => x.properties.mode !== "edit" && !GUIDES.some((k) => x.properties[k]));

export function polygonOf(f?: GeoJSONStoreFeatures): Polygon | undefined {
  if (!f || f.geometry.type !== "Polygon") return undefined;
  return { type: "Polygon", coordinates: f.geometry.coordinates as LonLat[][] };
}

export function current(draw: Draw): Polygon | undefined {
  return polygonOf(drawn(draw).find((x) => x.geometry.type === "Polygon"));
}

export function currentGeometry(draw: Draw): DrawGeometry | undefined {
  const f = drawn(draw).find((x) => ["Point", "LineString", "Polygon"].includes(x.geometry.type));
  return f?.geometry as DrawGeometry | undefined;
}
