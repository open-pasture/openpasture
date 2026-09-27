// K-animals: collar keys from a link or a new key, kept in this tab only until the cards
// print (keys are shown once), and a Herd table action waiting on a choice.

import type { HerdRow } from "../registry";
import type { LinkedCollar } from "../api/k-animals";
import { createSlice } from "./slice";

export type Pending = { kind: "move" | "park"; rows: HerdRow[] };

export interface KAnimals {
  // print id → collars with their keys
  batches: Record<string, LinkedCollar[]>;
  pending?: Pending;
}

export const kAnimalsSlice = createSlice<KAnimals>({ batches: {} });

export function keepBatch(id: string, collars: LinkedCollar[]) {
  kAnimalsSlice.patch({ batches: { ...kAnimalsSlice.get().batches, [id]: collars } });
}
