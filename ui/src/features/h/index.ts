// Stream H: the welfare record. The animal page's cue ledger, cues a day and tone a day, and
// trained or learning; Layers > Cues; the herd menu's Training mode and, while it is on, the
// herd panel's training line; the Herd table's trained column; fix rate and cell signal on
// the coverage map. Data > Reports lists the Welfare record from /api/reports.

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { coverageMetrics } from "../g/metrics";
import { layers, type OverlayHandle } from "../../map/overlays";
import { animalPage, herdColumns, herdMenu, herdPanel, HERD_PANEL } from "../../registry";
import { coverageExtra, cueLayer, watchCoverageExtra, watchCues, welfare } from "../../store/h";
import "../../styles/h.css";
import { statusRank } from "./model";

// A part that loads when first shown, inside its own Suspense.
function later<P extends object>(load: () => Promise<ComponentType<P>>): ComponentType<P> {
  const L = lazy(async () => ({ default: await load() }));
  return (p: P) => createElement(Suspense, { fallback: null }, createElement(L, p));
}
const AnimalWelfareSection = later(() => import("./Welfare").then((m) => m.AnimalWelfareSection));
const TrainingItem = later(() => import("./Welfare").then((m) => m.TrainingItem));
const TrainingLine = later(() => import("./Welfare").then((m) => m.TrainingLine));
const TrainedCell = later(() => import("./Welfare").then((m) => m.TrainedCell));

animalPage.register({ id: "h-welfare", order: 35, when: (p) => !!p.animal, Section: AnimalWelfareSection });

layers.register({
  id: "cues", label: "Cues", order: 60,
  available: () => cueLayer.get().available,
  overlay: {
    id: "cues", slot: "slot-points",
    mount(ctx) {
      let h: OverlayHandle | undefined;
      let gone = false;
      void import("./cues").then((m) => {
        if (!gone) h = m.mountCues(ctx);
      });
      return {
        update: () => h?.update?.(),
        destroy() {
          gone = true;
          h?.destroy();
        },
      };
    },
  },
});
watchCues(() => layers.changed());

herdMenu.register({ id: "h-training", label: "Training mode", order: 50, minRole: "manager", Item: TrainingItem });
// Just above the ack line and collar summary.
herdPanel.register({ id: "h-training", order: HERD_PANEL.collars - 5, Section: TrainingLine });

const rank = (r: { animal?: { id: string } }) => statusRank(r.animal ? welfare.get().animals[r.animal.id]?.status : undefined);
herdColumns.register({ id: "h-trained", label: "trained", order: 88, width: 88, sort: (a, b) => rank(a) - rank(b), Cell: TrainedCell });

// Fix rate: the share of fix attempts the receivers got (weak below 0.9, red below 0.75).
// Cell: the LTE-M signal the collars measured, RSRP dBm (weak below -110, red below -120).
coverageMetrics.register({
  id: "fix_rate", label: "fix rate", order: 30,
  tone: (v) => (v >= 0.9 ? "good" : v >= 0.75 ? "fair" : "poor"),
  text: (v) => `${Math.round(v * 100)}%`,
  available: () => coverageExtra.get().fix_rate,
});
coverageMetrics.register({
  id: "cell", label: "cell", order: 40,
  tone: (v) => (v >= -110 ? "good" : v >= -120 ? "fair" : "poor"),
  text: (v) => `${Math.round(v)} dBm`,
  available: () => coverageExtra.get().cell,
});
watchCoverageExtra(() => coverageMetrics.changed());
