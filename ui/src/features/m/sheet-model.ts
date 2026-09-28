// The herd panel as a bottom sheet: where it can rest and where a drag lets it settle. Pure.

import type { SheetState } from "../../store/m";

export interface Stops { peek: number; half: number; full: number }

// Least height of the peek, px: the grab and one line.
export const PEEK_MIN = 96;
// A fling faster than this (px/ms) goes on to the next stop in its direction.
export const FLING = 0.5;

// The rests of a sheet in a view `view` px tall whose peek content (the alerts, the decision
// sentence and its buttons, the next move) is `content` px tall. The peek never covers more
// than half the map; full leaves a sliver of map above it.
export function stops(view: number, content: number): Stops {
  const full = Math.max(PEEK_MIN, view - 8);
  const half = Math.max(PEEK_MIN, Math.round(view * 0.5));
  const peek = Math.round(Math.min(Math.max(content, PEEK_MIN), half));
  return { peek, half: Math.max(half, peek), full: Math.max(full, half) };
}

const ORDER: SheetState[] = ["peek", "half", "full"];

// Where a drag let go at `h` px with velocity `v` px/ms (positive = growing) comes to rest.
export function settle(s: Stops, h: number, v: number): SheetState {
  if (Math.abs(v) >= FLING) {
    const up = v > 0;
    // The next stop past where it is now, in the direction of the fling.
    const next = up ? ORDER.find((k) => s[k] > h + 1) : [...ORDER].reverse().find((k) => s[k] < h - 1);
    return next ?? (up ? "full" : "peek");
  }
  return ORDER.reduce((best, k) => (Math.abs(s[k] - h) < Math.abs(s[best] - h) ? k : best), "peek" as SheetState);
}

// A tap on the grab: peek opens to half, half to full, full back to peek.
export function nextState(s: SheetState): SheetState {
  return s === "peek" ? "half" : s === "half" ? "full" : "peek";
}
