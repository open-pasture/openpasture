// The tabs over the desktop view: one per view opened, in the order they were opened, each
// keeping where it was inside its view (the hash after the view id), so going back to a tab goes
// back there. Kept across reloads.

import { createSlice } from "../store/slice";

export interface Tab { view: string; rest: string }

const KEY = "op.tabs";
const store = typeof localStorage === "undefined" ? undefined : localStorage;

function load(): Tab[] {
  try {
    const v = JSON.parse(store?.getItem(KEY) ?? "[]");
    return Array.isArray(v) ? v.filter((t) => typeof t?.view === "string" && typeof t?.rest === "string") : [];
  } catch {
    return [];
  }
}

export const tabs = createSlice<Tab[]>(load());
tabs.subscribe(() => store?.setItem(KEY, JSON.stringify(tabs.get())));

export const hashOf = (t: Tab) => `#/${t.view}${t.rest ? (t.rest.startsWith("?") ? t.rest : `/${t.rest}`) : ""}`;

// ---- the rules, on a list ----------------------------------------------------------------

// The view showing now: its tab takes this place inside it, or opens at the end. The same list
// when nothing changed.
export function visited(list: Tab[], view: string, rest: string): Tab[] {
  const at = list.findIndex((t) => t.view === view);
  if (at < 0) return [...list, { view, rest }];
  if (list[at].rest === rest) return list;
  const next = list.slice();
  next[at] = { view, rest };
  return next;
}

// Without `view`'s tab, and the tab to show if it was the one showing: the next, else the
// previous. The last tab doesn't close.
export function closed(list: Tab[], view: string, showing: string): { list: Tab[]; show?: Tab } {
  const at = list.findIndex((t) => t.view === view);
  if (at < 0 || list.length < 2) return { list };
  const next = list.filter((t) => t.view !== view);
  return { list: next, show: view === showing ? next[Math.min(at, next.length - 1)] : undefined };
}

// Tabs whose view went away (a role change, a feature gone) drop out.
export const pruned = (list: Tab[], known: (view: string) => boolean): Tab[] =>
  list.every((t) => known(t.view)) ? list : list.filter((t) => known(t.view));

// ---- on the app's tabs -------------------------------------------------------------------

export const visit = (view: string, rest: string) => tabs.set((list) => visited(list, view, rest));

export function close(view: string, showing: string) {
  const { list, show } = closed(tabs.get(), view, showing);
  tabs.set(list);
  if (show) location.hash = hashOf(show);
}

export const prune = (known: (view: string) => boolean) => tabs.set((list) => pruned(list, known));
