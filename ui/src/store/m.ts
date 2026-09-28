// M: the phone's state. The herd panel as a bottom sheet (peek, half, full), the animal a tap
// picked, and where you are when you asked.

import type { LonLat } from "../api";
import { createSlice } from "./slice";

export type SheetState = "peek" | "half" | "full";

// The herd panel's sheet: its state and the height it shows at (px, 0 off the phone layout).
export const sheet = createSlice<{ state: SheetState; h: number }>({ state: "peek", h: 0 });

// The collar a tap (or search, or an alert) picked on a touch screen.
export const picked = createSlice<string | null>(null);

// Your position while "you" is on: point and accuracy (m), or the error that stopped it.
export const you = createSlice<{ on: boolean; at?: LonLat; accuracy_m?: number; error?: string }>({ on: false });
