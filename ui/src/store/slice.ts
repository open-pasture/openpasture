// A small store per stream: createSlice(init) → { get, set, use, subscribe }.
// set() updates at once; subscribers hear about it once per animation frame however
// many sets happened, so a burst of live events costs one render.

import { useSyncExternalStore } from "react";

export interface Slice<T> {
  get(): T;
  // A value, or a function of the current one. Objects are replaced, not merged; use patch() for that.
  set(next: T | ((prev: T) => T)): void;
  // Shallow merge into an object slice.
  patch(p: Partial<T>): void;
  // React: re-render when sel(value) changes. Select primitives or stable references.
  use<R>(sel: (v: T) => R): R;
  subscribe(fn: () => void): () => void;
}

// One frame, or 250 ms when the tab is hidden and frames stop.
const schedule = (f: () => void) => {
  let done = false;
  const run = () => {
    if (done) return;
    done = true;
    f();
  };
  if (typeof requestAnimationFrame === "function") requestAnimationFrame(run);
  setTimeout(run, 250);
};

export function createSlice<T>(init: T): Slice<T> {
  let value = init;
  let queued = false;
  const subs = new Set<() => void>();
  const notify = () => {
    queued = false;
    subs.forEach((f) => f());
  };
  const slice: Slice<T> = {
    get: () => value,
    set(next) {
      const v = typeof next === "function" ? (next as (prev: T) => T)(value) : next;
      if (Object.is(v, value)) return;
      value = v;
      if (!queued) {
        queued = true;
        schedule(notify);
      }
    },
    patch(p) {
      slice.set({ ...value, ...p });
    },
    use: <R>(sel: (v: T) => R) => useSyncExternalStore(slice.subscribe, () => sel(value), () => sel(value)),
    subscribe(fn) {
      subs.add(fn);
      return () => void subs.delete(fn);
    },
  };
  return slice;
}
