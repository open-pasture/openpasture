// Between the desktop shell and the map. The herd panel and the paddock list sit outside the map
// view, so they ask it for things here; the map answers when it is showing, or when it next
// loads, and says here what it is doing.

import { createSlice } from "../store/slice";

export type MapAsk =
  | { k: "paddock"; id: string }
  | { k: "collar"; id: string }
  | { k: "collars"; ids: string[] }
  | { k: "hover"; id?: string }
  | { k: "hoverPaddock"; id?: string }
  | { k: "change" };

// The newest ask, numbered so the same one twice is two asks. The map clears it once done.
export const mapAsk = createSlice<{ n: number; ask: MapAsk } | null>(null);
let n = 0;
export const ask = (a: MapAsk) => mapAsk.set({ n: ++n, ask: a });

// What the map tells the shell: whether it is mounted, whether a proposal is being changed, the
// paddock whose sheet is open and the one under the pointer.
export const mapState = createSlice<{ on: boolean; changing: boolean; paddock?: string; hoverPaddock?: string }>({ on: false, changing: false });
