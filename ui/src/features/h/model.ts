// Pure pieces of the welfare record (stream H): the words the animal page, the herd panel,
// the Herd table and the Cues layer show, and the map ticks.

import type { Ending, HerdWelfare, Learning, Outcomes, Tick, WelfareDay, WelfareStatus } from "../../api/h";

const ENDINGS: Record<Ending, string> = { turned_back: "turned back", crossed: "crossed", rest: "rest", boundary_changed: "boundary changed" };

export const endingWord = (o: Ending | undefined) => (o ? ENDINGS[o] ?? o.replace(/_/g, " ") : "");

// "Sep 24", in the reader's calendar.
export const shortDay = (iso: string) => new Date(iso).toLocaleDateString(undefined, { month: "short", day: "numeric" });

// "trained since Sep 24", "learning since Sep 20"; nothing without episodes.
export function statusText(l: Learning | undefined, day: (iso: string) => string = shortDay): string | undefined {
  if (!l?.status) return undefined;
  return l.since ? `${l.status} since ${day(l.since)}` : l.status;
}

// "8 turned back  1 crossed  2 rest", leaving out what didn't happen.
export function outcomesText(o: Outcomes): string {
  return (Object.keys(ENDINGS) as Ending[]).filter((k) => o[k] > 0).map((k) => `${o[k]} ${ENDINGS[k]}`).join("  ");
}

// The ring a cue was about: the boundary's edge, or a hole in it.
export const ringWord = (ring: number | undefined) => (ring === undefined ? "" : ring === 0 ? "edge" : `hole ${ring}`);

// Seconds of tone, one decimal: "0.3 s", "12.6 s".
export const secs = (s: number) => `${(Math.round(s * 10) / 10).toFixed(1)} s`;
export const toneText = (ms: number) => secs(ms / 1000);

// "training  31/250 trained" while the herd's training mode is on; nothing otherwise.
export function trainingLine(h: HerdWelfare | undefined): string | undefined {
  if (!h?.training?.enabled) return undefined;
  return `training  ${h.trained}/${h.head} trained`;
}

// The two sparklines of the animal page: cues a day and seconds of tone a day, oldest first.
export function sparks(days: WelfareDay[]): { cues: number[]; tone: number[] } {
  return { cues: days.map((d) => d.warn + d.outside), tone: days.map((d) => d.tone_s) };
}

// Herd table: trained first, then learning, then animals with no episodes.
export const statusRank = (s: WelfareStatus | undefined) => (s === "trained" ? 0 : s === "learning" ? 1 : 2);

export interface TickFeature {
  type: "Feature";
  properties: { w: number; o: number; kind: "warn" | "outside"; weight: number };
  geometry: { type: "Point"; coordinates: [number, number] };
}

// One point per 2 m cell. A cell with any outside tone draws as outside (red); weight grows
// with the cues there, from faint for one to full at 20.
export function tickFeatures(ticks: Tick[]): { type: "FeatureCollection"; features: TickFeature[] } {
  return {
    type: "FeatureCollection",
    features: ticks.map(([lon, lat, w, o]) => ({
      type: "Feature",
      properties: { w, o, kind: o > 0 ? "outside" : "warn", weight: Math.min(1, 0.35 + 0.65 * Math.log10(1 + w + o) / Math.log10(21)) },
      geometry: { type: "Point", coordinates: [lon, lat] },
    })),
  };
}

// "12 warn  1 outside" under the pointer.
export function tickText(w: number, o: number): string {
  return [w > 0 && `${w} warn`, o > 0 && `${o} outside`].filter(Boolean).join("  ");
}
