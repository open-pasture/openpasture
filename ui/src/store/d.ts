// The farm's map features, loaded once the app is up and kept current by `feature` events.
// Reloaded when the live socket comes back or the server says this socket missed events.

import { featuresApi, type MapFeature } from "../api/d";
import { store } from "../store";
import { upsert, withPaddocks } from "../features/d/kinds";
import { createSlice } from "./slice";

// Every feature, in stored order, whatever its active window: the map shows those in effect.
export const features = createSlice<MapFeature[]>([]);

let loading: Promise<void> | undefined;

export function loadFeatures() {
  loading ??= featuresApi
    .list()
    .then((list) => {
      const paddocks = store.get().state?.paddocks;
      features.set(paddocks ? withPaddocks(list, paddocks) : list);
    })
    .catch(() => {
      /* offline or no token yet: the next reconnect loads them */
    })
    .finally(() => {
      loading = undefined;
    });
  return loading;
}

// A feature this browser just saved, before its event arrives.
export const putFeature = (f: MapFeature) => features.set((list) => upsert(list, f));
export const dropFeature = (id: string) => features.set((list) => list.filter((x) => x.id !== id));

let started = false;

export function startFeatures() {
  if (started) return;
  started = true;
  store.on("feature", (e) => (e.deleted ? dropFeature(e.feature.id) : putFeature(e.feature)));
  store.on("resync", () => void loadFeatures());
  let up = false, ready = false;
  let paddocks = store.get().state?.paddocks;
  const check = () => {
    const s = store.get();
    // First load once the app has its state, again whenever the socket reconnects.
    if ((s.ready && s.state && !ready) || (s.up && !up)) void loadFeatures();
    ready = s.ready && !!s.state;
    up = s.up;
    // A deleted paddock took its features with it.
    if (s.state && s.state.paddocks !== paddocks) {
      paddocks = s.state.paddocks;
      features.set((list) => withPaddocks(list, paddocks!));
    }
  };
  store.subscribe(check);
  check();
}
