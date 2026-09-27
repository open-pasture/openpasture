// GET /api/texting, loaded once and shared by the Texting rows, the brief beside Daily and
// every person's Morning brief check.

import { useEffect } from "react";
import { createSlice } from "../../store/slice";
import { textingApi, type Texting, type TextingPatch } from "./api";

export const texting = createSlice<Texting | null>(null);

let loading: Promise<void> | undefined;

export function loadTexting(force = false): Promise<void> {
  if (loading) return loading;
  if (texting.get() && !force) return Promise.resolve();
  loading = textingApi
    .get()
    .then((t) => texting.set(t))
    .catch(() => {})
    .finally(() => (loading = undefined));
  return loading;
}

export function useTexting(): Texting | null {
  useEffect(() => {
    void loadTexting();
  }, []);
  return texting.use((t) => t);
}

export async function saveTexting(p: TextingPatch): Promise<void> {
  texting.set(await textingApi.save(p));
}
