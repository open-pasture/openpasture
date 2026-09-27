// Welfare record (stream H): each herd's learning status by animal, kept fresh while something
// shows it; whether the Cues layer and the fix rate / cell maps have anything to draw.

import { useEffect } from "react";
import { gApi } from "../api/g";
import { hApi, type HerdWelfare, type Training, type WelfareAnimal } from "../api/h";
import { store } from "../store";
import { createSlice } from "./slice";

export interface WelfareState { herds: Record<string, HerdWelfare>; animals: Record<string, WelfareAnimal> }
export const welfare = createSlice<WelfareState>({ herds: {}, animals: {} });

// Episodes follow cues within a report or two; the status changes slowly.
const EVERY = 2 * 60_000;
const AFTER_CUES = 15_000;

const loading = new Map<string, Promise<void>>();
export function loadHerd(herdId: string): Promise<void> {
  let p = loading.get(herdId);
  if (!p) {
    p = hApi
      .herd(herdId)
      .then((h) =>
        welfare.set((s) => ({
          herds: { ...s.herds, [herdId]: h },
          animals: { ...s.animals, ...Object.fromEntries(h.animals.map((a) => [a.animal_id, a])) },
        })),
      )
      .catch(() => {})
      .finally(() => loading.delete(herdId));
    loading.set(herdId, p);
  }
  return p;
}

// Each herd's training mode, as last read or saved.
export const trainings = createSlice<Record<string, Training>>({});

// A herd's record after its training mode changed.
export function setTraining(herdId: string, training: Training) {
  trainings.set((t) => ({ ...t, [herdId]: training }));
  welfare.set((s) => {
    const h = s.herds[herdId];
    return h ? { ...s, herds: { ...s.herds, [herdId]: { ...h, training } } } : s;
  });
  void loadHerd(herdId);
}

const asked = new Set<string>();
// The warning zone a boundary sent now without one gets: the herd's training warn while
// training is on, else nothing (the collars' own default).
export function useDefaultWarn(herdId: string | undefined): number | undefined {
  useEffect(() => {
    if (!herdId || asked.has(herdId)) return;
    asked.add(herdId);
    hApi.training(herdId).then((t) => trainings.set((x) => ({ ...x, [herdId]: t })), () => asked.delete(herdId));
  }, [herdId]);
  return trainings.use((t) => {
    const x = herdId ? t[herdId] : undefined;
    return x?.enabled ? x.warn_m : undefined;
  });
}

const watched = new Map<string, { n: number; timer: ReturnType<typeof setInterval> }>();
const soon = new Map<string, ReturnType<typeof setTimeout>>();
let listening = false;

function watch(herdId: string) {
  if (!listening) {
    listening = true;
    // New cues mean new episodes shortly after.
    store.on("cue_batch", (e) => {
      if (!watched.has(e.herd_id) || soon.has(e.herd_id)) return;
      soon.set(e.herd_id, setTimeout(() => {
        soon.delete(e.herd_id);
        void loadHerd(e.herd_id);
      }, AFTER_CUES));
    });
  }
  const w = watched.get(herdId);
  if (w) w.n++;
  else {
    void loadHerd(herdId);
    watched.set(herdId, { n: 1, timer: setInterval(() => void loadHerd(herdId), EVERY) });
  }
  return () => {
    const x = watched.get(herdId);
    if (x && --x.n === 0) {
      clearInterval(x.timer);
      watched.delete(herdId);
    }
  };
}

export function useHerdWelfare(herdId: string | undefined): HerdWelfare | undefined {
  useEffect(() => (herdId ? watch(herdId) : undefined), [herdId]);
  return welfare.use((s) => (herdId ? s.herds[herdId] : undefined));
}

export function useWelfareAnimal(animalId: string | undefined, herdId: string | undefined): WelfareAnimal | undefined {
  useEffect(() => (herdId ? watch(herdId) : undefined), [herdId]);
  return welfare.use((s) => (animalId ? s.animals[animalId] : undefined));
}

// ---- what the map can offer ------------------------------------------------------------

const CHECK_EVERY = 10 * 60_000;

// The Cues layer: the selected herd has cues with a position in the last 7 days.
export const cueLayer = createSlice<{ available: boolean; herdId?: string }>({ available: false });

export function watchCues(changed: () => void) {
  let started = false;
  let checking = false;
  const check = async () => {
    const herdId = store.get().herdId;
    if (!herdId || checking) return;
    checking = true;
    try {
      const has = (await hApi.points({ herd_id: herdId, from: "-7d" })).ticks.length > 0;
      if (has !== cueLayer.get().available || herdId !== cueLayer.get().herdId) {
        cueLayer.set({ available: has, herdId });
        changed();
      }
    } catch {
      // Offline or signed out: keep what we knew.
    } finally {
      checking = false;
    }
  };
  const begin = () => {
    if (!store.get().state) return;
    if (!started) {
      started = true;
      setInterval(() => void check(), CHECK_EVERY);
      // The first cues of a herd show the layer.
      store.on("cue_batch", (e) => {
        if (!cueLayer.get().available && e.herd_id === store.get().herdId) void check();
      });
    }
    if (store.get().herdId !== cueLayer.get().herdId) void check();
  };
  store.subscribe(begin);
  begin();
}

// Fix rate and cell signal on the coverage map: offered once the last week has cells.
export const coverageExtra = createSlice<{ fix_rate: boolean; cell: boolean }>({ fix_rate: false, cell: false });

export function watchCoverageExtra(changed: () => void) {
  let started = false;
  const check = async () => {
    try {
      const [rate, cell] = await Promise.all([gApi.coverage({ metric: "fix_rate" }), gApi.coverage({ metric: "cell" })]);
      const next = { fix_rate: rate.cells.length > 0, cell: cell.cells.length > 0 };
      const now = coverageExtra.get();
      if (next.fix_rate !== now.fix_rate || next.cell !== now.cell) {
        coverageExtra.set(next);
        changed();
      }
    } catch {
      // Keep what we knew.
    }
  };
  const begin = () => {
    if (started || !store.get().state) return;
    started = true;
    void check();
    setInterval(() => void check(), CHECK_EVERY);
  };
  store.subscribe(begin);
  begin();
}
