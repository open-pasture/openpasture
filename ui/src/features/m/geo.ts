// How far and which way: from where you stand to an animal, for the one mono line
// "214  340 m NE". Pure, so it is tested without a map.

import type { LonLat } from "../../api";

const R = 6371008.8;
const rad = (d: number) => (d * Math.PI) / 180;

// Great-circle metres between two points.
export function metres(a: LonLat, b: LonLat): number {
  const dLat = rad(b[1] - a[1]);
  const dLon = rad(b[0] - a[0]);
  const h = Math.sin(dLat / 2) ** 2 + Math.cos(rad(a[1])) * Math.cos(rad(b[1])) * Math.sin(dLon / 2) ** 2;
  return 2 * R * Math.asin(Math.min(1, Math.sqrt(h)));
}

// Initial bearing from a to b, degrees clockwise from north, 0..360.
export function bearing(a: LonLat, b: LonLat): number {
  const y = Math.sin(rad(b[0] - a[0])) * Math.cos(rad(b[1]));
  const x = Math.cos(rad(a[1])) * Math.sin(rad(b[1])) - Math.sin(rad(a[1])) * Math.cos(rad(b[1])) * Math.cos(rad(b[0] - a[0]));
  return ((Math.atan2(y, x) * 180) / Math.PI + 360) % 360;
}

const POINTS = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"] as const;
export type Compass = (typeof POINTS)[number];

// The nearest of eight compass points.
export function compass(deg: number): Compass {
  return POINTS[Math.round((((deg % 360) + 360) % 360) / 45) % 8];
}

// "214  340 m NE": the label, then (when you are located) the distance in the farm's units and
// the way to walk. Closer than a few metres there is no way to point: "here".
export function walkLine(label: string, you: LonLat | undefined, animal: LonLat | undefined, len: (m: number) => string): string {
  if (!you || !animal) return label;
  const m = metres(you, animal);
  if (m < 5) return `${label}  here`;
  return `${label}  ${len(m)} ${compass(bearing(you, animal))}`;
}

// Where "you" stands after the browser reports a location error (GeolocationPositionError.code):
// only a refusal (1) ends the watch; a timeout (3, a phone standing still) or no fix for now (2, under
// trees, in a truck cab) keeps the watch and the last fix, with the error shown for a moment.
export type YouState = { on: boolean; at?: LonLat; accuracy_m?: number; error?: string };
export function afterLocateError(prev: YouState, code: number): { next: YouState; stop: boolean } {
  if (code === 1) return { next: { on: false, error: "Location is off for this site." }, stop: true };
  return { next: { ...prev, error: "Can't find where you are." }, stop: false };
}
