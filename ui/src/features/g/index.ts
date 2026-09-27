// Stream G: coverage and fleet care. Layers > Coverage, Herd table columns (battery trend, fit
// check due) and "Checked fit" on a selection, the animal page's care section, and the fit-check
// interval in Settings > Collars.

import { layers, type OverlayHandle } from "../../map/overlays";
import { animalPage, herdBulk, herdColumns, settingsSections, type HerdRow } from "../../registry";
import { gApi } from "../../api/g";
import { coverage, fleet, loadFleet, watchCoverage } from "../../store/g";
import "../../styles/g.css";
import { Care, FitCell, FitDays, TrendCell } from "./Care";
import { byDaysLeft, byFitDue, collarIds } from "./model";

// Herd table selection: "Checked fit" for every collar in it at once.
async function checkedFit(rows: HerdRow[]) {
  const ids = collarIds(rows);
  if (!ids.length) throw new Error("None of these wears a collar.");
  await gApi.checkFits(ids);
  await loadFleet();
}

// The layer shows in the Layers menu once the last week has cells; the drawing code loads with
// the map and only when the layer is turned on.
layers.register({
  id: "coverage", label: "Coverage", order: 50,
  available: () => coverage.get().available,
  overlay: {
    id: "coverage", slot: "slot-fill",
    mount(ctx) {
      let h: OverlayHandle | undefined;
      let gone = false;
      void import("./coverage").then((m) => {
        if (!gone) h = m.mountCoverage(ctx);
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
watchCoverage(() => layers.changed());

herdColumns.register({ id: "g-trend", label: "trend", order: 85, width: 110, sort: (a, b) => byDaysLeft(fleet.get().rows)(a, b), Cell: TrendCell });
herdColumns.register({ id: "g-fit", label: "fit check", order: 95, width: 100, sort: (a, b) => byFitDue(fleet.get().rows)(a, b), Cell: FitCell });
herdBulk.register({ id: "g-fit", label: "Checked fit", order: 50, minRole: "hand", run: checkedFit });
animalPage.register({ id: "g-care", order: 30, when: (p) => !!p.collar, Section: Care });
settingsSections.register({ id: "g-fit-days", group: "Collars", label: "Fit checks", order: 85, minRole: "manager", Section: FitDays });
