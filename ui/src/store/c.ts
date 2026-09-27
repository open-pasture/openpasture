// What the strip and lasso tools pick up when they open.

import type { LonLat } from "../api";
import { createSlice } from "./slice";

export interface CState {
  // A paddock sheet asked the strip tool to open on this paddock, with a saved layout.
  open?: { paddockId: string; layoutId?: string; at: number };
  // Animals a shift-drag lasso caught before the lasso tool opened.
  lasso?: { ids: string[]; ring: LonLat[]; at: number };
}

export const cState = createSlice<CState>({});

// Left for a tool that opens now; older requests went unanswered and don't count.
const FRESH_MS = 5000;

// What was left for a tool, if it was left just now. Safe to call while rendering (it
// doesn't clear anything); the tool calls done(k) once it has mounted.
export function peek<K extends keyof CState>(k: K): CState[K] {
  const v = cState.get()[k];
  return v && Date.now() - v.at < FRESH_MS ? v : undefined;
}

export function done<K extends keyof CState>(k: K) {
  if (cState.get()[k] !== undefined) cState.set((s) => ({ ...s, [k]: undefined }));
}
