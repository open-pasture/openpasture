// Unresolved alerts on the farm, kept current by `alert` live events, and the alert list's
// open state. Loaded when the top bar first shows and again after every reconnect.

import type { Alert } from "../api";
import { alertsApi } from "../api/a-engine";
import { byUrgency, upsert } from "../features/a-engine/model";
import { createSlice } from "./slice";

export interface AlertsState {
  loaded: boolean;
  // Open and acked, every herd, most urgent first.
  list: Alert[];
}

export const alerts = createSlice<AlertsState>({ loaded: false, list: [] });

// The top bar's list of every unresolved alert.
export const alertList = createSlice<{ open: boolean }>({ open: false });

export async function loadAlerts() {
  try {
    const list = await alertsApi.list({ limit: 1000 });
    alerts.set({ loaded: true, list: list.sort(byUrgency) });
  } catch {
    /* the next reconnect loads again */
  }
}

export function applyAlert(a: Alert) {
  alerts.set((s) => ({ ...s, list: upsert(s.list, a).sort(byUrgency) }));
}
