// Stream D: exclusions and farm infrastructure. Draw-group tools (with Paddock they make the
// Draw menu), the map overlay, and the features store kept current by live events.

import { lazy } from "react";
import type { FeatureKind } from "../../api";
import { overlays, type OverlayHandle } from "../../map/overlays";
import { tools } from "../../map/tools";
import { features, startFeatures } from "../../store/d";
import "../../styles/d.css";
import { KINDS } from "./kinds";

startFeatures();

// The farm has one boundary: its tool shows only while there is none.
const hasFarmBoundary = () => features.get().some((f) => f.kind === "farm_boundary");
let had = hasFarmBoundary();
features.subscribe(() => {
  if (hasFarmBoundary() !== had) {
    had = !had;
    tools.changed();
  }
});

const toolOf = (kind: FeatureKind) => lazy(() => import("./tool").then((m) => ({ default: m.toolFor(kind) })));

for (const k of KINDS)
  tools.register({
    id: `feature-${k.kind}`, label: k.label, key: k.key, group: "draw", order: k.order, minRole: "manager",
    when: k.kind === "farm_boundary" ? () => !hasFarmBoundary() : undefined,
    Tool: toolOf(k.kind),
  });

// Zones under the boundaries, icons under the animals. The drawing code loads with the map.
overlays.register({
  id: "features", slot: "slot-zones",
  mount(ctx) {
    let h: OverlayHandle | undefined;
    let gone = false;
    void import("./overlay").then((m) => {
      if (!gone) h = m.mountFeatures(ctx);
    });
    return {
      update: () => h?.update?.(),
      destroy() {
        gone = true;
        h?.destroy();
      },
    };
  },
});
