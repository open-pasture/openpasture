// Coverage and fleet care (stream G): the fleet rows by collar id, kept fresh while something
// shows them, and whether the coverage layer has anything to draw.

import { useEffect } from "react";
import { gApi, type FleetRow } from "../api/g";
import { store } from "../store";
import { createSlice } from "./slice";

export interface FleetState { rows: Record<string, FleetRow>; loaded: boolean }
export const fleet = createSlice<FleetState>({ rows: {}, loaded: false });

// The day tables behind these change every ten minutes at most.
const FLEET_EVERY = 5 * 60_000;
const COVERAGE_EVERY = 10 * 60_000;

let loading: Promise<void> | undefined;
export function loadFleet(): Promise<void> {
  loading ??= gApi
    .fleet()
    .then((rows) => fleet.set({ rows: Object.fromEntries(rows.map((r) => [r.collar_id, r])), loaded: true }))
    .catch(() => {})
    .finally(() => (loading = undefined));
  return loading;
}

let watchers = 0;
let timer: ReturnType<typeof setInterval> | undefined;
function watch() {
  if (watchers++ === 0) {
    void loadFleet();
    timer = setInterval(() => void loadFleet(), FLEET_EVERY);
  }
  return () => {
    if (--watchers === 0) clearInterval(timer);
  };
}

// One collar's fleet row; loads the rows while any is shown.
export function useFleetRow(collarId: string | undefined): FleetRow | undefined {
  useEffect(watch, []);
  return fleet.use((s) => (collarId ? s.rows[collarId] : undefined));
}

// Whether /api/coverage has cells for the last week (either metric), checked once the app has
// its farm and then every ten minutes. `changed` runs when that flips.
export const coverage = createSlice<{ available: boolean }>({ available: false });

export function watchCoverage(changed: () => void) {
  let started = false;
  const check = async () => {
    try {
      let has = (await gApi.coverage({ metric: "accuracy" })).cells.length > 0;
      if (!has) has = (await gApi.coverage({ metric: "fixes" })).cells.length > 0;
      if (has !== coverage.get().available) {
        coverage.set({ available: has });
        changed();
      }
    } catch {
      // Offline or signed out: keep what we knew.
    }
  };
  const begin = () => {
    if (started || !store.get().state) return;
    started = true;
    void check();
    setInterval(() => void check(), COVERAGE_EVERY);
  };
  store.subscribe(begin);
  begin();
}
