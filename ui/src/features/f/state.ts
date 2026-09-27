// The check the open tool last got back, for the map to draw, and the map's context while it is
// up, so a sentence can fly to its cause.

import type { Finding } from "../../api";
import type { CheckResult } from "../../api/f";
import type { OverlayCtx } from "../../map/overlays";
import { createSlice } from "../../store/slice";
import { causeOf, collarsOf } from "./model";

export const presend = createSlice<{ result?: CheckResult }>({});

let map: OverlayCtx | null = null;
let clear: ReturnType<typeof setTimeout> | undefined;

export function attach(ctx: OverlayCtx) {
  map = ctx;
}

export function detach() {
  map = null;
  clearTimeout(clear);
}

// Fly to what a finding is about and ring its animals for a few seconds.
export function flyToCause(f: Finding) {
  const ctx = map;
  if (!ctx) return;
  const pos = ctx.positions();
  const to = causeOf(f, (id) => pos.get(id)?.fix.point);
  if (to) ctx.flyTo(to);
  const ids = collarsOf(f);
  if (!ids.length) return;
  ctx.highlight(ids, f.severity === "critical" ? "red" : "warn");
  clearTimeout(clear);
  clear = setTimeout(() => ctx.highlight([]), 6000);
}
