// Paddock fills for the map layers: rest, NDVI, drought and flood. Stepped, like the pixel
// marks: each step is one tone at one opacity, so neighbouring paddocks read apart at a glance.

import type { PaddockLayer } from "../../api/b";

export type { LayerKind } from "./have";

// map/base.ts colours, inlined so the registries' chunk doesn't load MapLibre.
export const TONE = { fg: "#F3F2EA", grass: "#9FD760", warn: "#F0936C", red: "#E5484D" } as const;

export interface Fill { color: string; opacity: number }

// Days of rest → how much grass shows. A paddock grazed now shows none; recovered ground
// (45 days and more) the most.
export const REST_STEPS: readonly [number, number][] = [[0, 0.04], [7, 0.1], [14, 0.17], [28, 0.25], [45, 0.34]];

export function restFill(days: number | undefined, grazing = false): Fill | undefined {
  if (grazing) return undefined;
  if (days === undefined || !Number.isFinite(days) || days < 0) return undefined;
  let opacity = REST_STEPS[0][1];
  for (const [from, o] of REST_STEPS) if (days >= from) opacity = o;
  return { color: TONE.grass, opacity };
}

// NDVI: bare ground (under 0.4) warms, green cover (0.4 and up) greens with density.
export function ndviFill(v: number | undefined): Fill | undefined {
  if (v === undefined || !Number.isFinite(v)) return undefined;
  if (v < 0.3) return { color: TONE.warn, opacity: 0.22 };
  if (v < 0.4) return { color: TONE.warn, opacity: 0.12 };
  if (v < 0.5) return { color: TONE.grass, opacity: 0.08 };
  if (v < 0.6) return { color: TONE.grass, opacity: 0.16 };
  if (v < 0.7) return { color: TONE.grass, opacity: 0.24 };
  return { color: TONE.grass, opacity: 0.32 };
}

// US Drought Monitor: abnormally dry and moderate warm, severe and worse red.
const DROUGHT: Record<string, Fill> = {
  D0: { color: TONE.warn, opacity: 0.1 },
  D1: { color: TONE.warn, opacity: 0.2 },
  D2: { color: TONE.red, opacity: 0.16 },
  D3: { color: TONE.red, opacity: 0.24 },
  D4: { color: TONE.red, opacity: 0.32 },
};

export function droughtFill(d: PaddockLayer["drought"]): Fill | undefined {
  return d?.category ? DROUGHT[d.category.toUpperCase()] : undefined;
}

// Floodplain ground is hatched; brighter when the forecast carries a flood flag.
export function floodOpacity(f: PaddockLayer["flood"]): number | undefined {
  if (!f) return undefined;
  if (f.risk === "high") return 0.95;
  if (f.risk === "medium") return 0.75;
  return f.in_floodplain ? 0.5 : undefined;
}

// The number under a paddock's name while a layer is on.
export function restLabel(r: PaddockLayer): string | undefined {
  if (r.grazing) return "now";
  if (r.rest_days === undefined) return undefined;
  return r.rest_days < 1 ? "<1 d" : `${Math.floor(r.rest_days)} d`;
}

export const ndviLabel = (r: PaddockLayer) => (r.ndvi === undefined ? undefined : r.ndvi.toFixed(2));
export const droughtLabel = (r: PaddockLayer) => r.drought?.category ?? undefined;
export const floodLabel = (r: PaddockLayer) => (r.flood?.in_floodplain ? r.flood.zone ?? undefined : undefined);
