// Stream F: the pre-send check. A tool footer section under the boundary and strip tools (the
// sentences and facts line) and a map overlay that draws what the check found. The drawing code
// loads with the map.

import { createElement, lazy, Suspense } from "react";
import { overlays, type OverlayHandle } from "../../map/overlays";
import { toolFooter, type ToolFooterProps } from "../../registry";
import "../../styles/f.css";

const Check = lazy(() => import("./Check").then((m) => ({ default: m.PresendCheck })));
toolFooter.register({
  id: "f-check", order: 10, minRole: "hand",
  Section: (p: ToolFooterProps) => createElement(Suspense, { fallback: null }, createElement(Check, p)),
});

overlays.register({
  id: "f-presend",
  slot: "slot-plan",
  mount(ctx) {
    let h: OverlayHandle | undefined;
    let gone = false;
    void import("./overlay").then((m) => {
      if (!gone) h = m.mountPresend(ctx);
    });
    return {
      destroy() {
        gone = true;
        h?.destroy();
      },
    };
  },
});
