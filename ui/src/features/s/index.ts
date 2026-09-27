// S: strip schedules. The Schedule footer under the strip tool, the herd panel's next move
// with its countdown and queue, the warn ring on collars that don't hold the next strip, and
// the time rail along the bottom of the map. Everything but these registrations loads on use.

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { overlays, type OverlayHandle } from "../../map/overlays";
import { herdPanel, HERD_PANEL, toolFooter, type ToolFooterProps } from "../../registry";
import { store } from "../../store";
import { fenced } from "../../store/boundary";
import { reloadSoon, sState } from "../../store/s";
import "../../styles/s.css";

store.on("schedule", (e) => reloadSoon(e.schedule.herd_id));
store.on("resync", () => {
  const h = store.get().herdId;
  if (h) reloadSoon(h);
});
// Staging and reissuing send boundaries; acks change who holds the next strip.
store.on("boundary", (e) => {
  if (sState.get().byHerd[e.herd_id]?.schedule) reloadSoon(e.herd_id, 1500);
});
const lastAck = new Map<string, number>();
store.on("ack_batch", (e) => {
  const hs = sState.get().byHerd[e.herd_id];
  if (!hs?.schedule || hs.schedule.status !== "active") return;
  const t = Date.now();
  if (t - (lastAck.get(e.herd_id) ?? 0) < 15_000) return;
  lastAck.set(e.herd_id, t);
  reloadSoon(e.herd_id, 2000);
});

const later = <P extends object>(C: ComponentType<P>) => (p: P) => createElement(Suspense, { fallback: null }, createElement(C, p));
const Panel = lazy(() => import("./Panel").then((m) => ({ default: m.SchedulePanel })));
const Footer = lazy(() => import("./Footer").then((m) => ({ default: m.ScheduleFooter })));

// Right under the decision: the schedule is what the day's call is about.
herdPanel.register({ id: "s-schedule", order: HERD_PANEL.decision + 5, Section: later(Panel) });
toolFooter.register({
  id: "s-schedule", order: 50, minRole: "manager",
  // A schedule starts from where the herd is fenced now.
  when: (p: ToolFooterProps) => p.tool === "strip" && fenced(store.get().boundary[p.herdId]),
  Section: later(Footer),
});

overlays.register({
  id: "s-rail",
  slot: "slot-plan",
  mount(ctx) {
    let h: OverlayHandle | undefined;
    let gone = false;
    void import("./rail").then((m) => {
      if (!gone) h = m.mountRail(ctx);
    });
    return {
      destroy() {
        gone = true;
        h?.destroy();
      },
    };
  },
});

// Collars that should hold the next strip and don't: the warn ring.
overlays.register({
  id: "s-missing",
  slot: "slot-top",
  mount(ctx) {
    let shown = "";
    const ring = () => {
      const h = ctx.herdId();
      const ids = (h && sState.get().byHerd[h]?.missing) || [];
      const key = ids.join();
      if (key === shown) return;
      shown = key;
      ctx.highlight(ids, "warn");
    };
    const off = sState.subscribe(ring);
    ring();
    return {
      update: ring,
      destroy() {
        off();
        ctx.highlight([]);
      },
    };
  },
});
