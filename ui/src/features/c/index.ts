// C: the Strip tool (t), the Lasso tool (l, or shift-drag), saved layouts and Copy on the paddock sheet.
// Everything but these registrations loads on first use.

import { createElement, lazy, Suspense, type ComponentType } from "react";
import type { Paddock } from "../../api";
import { tools } from "../../map/tools";
import { overlays, type OverlayHandle } from "../../map/overlays";
import { PADDOCK_SHEET, paddockSheet } from "../../registry";
import { store } from "../../store";
import "../../styles/c.css";

// Strips cut the paddock the herd is in.
tools.register({
  id: "strip", label: "Strip", key: "t", group: "bar", order: 20, minRole: "manager",
  when: (c) => !!c.herd?.paddock_id && c.state.paddocks.some((p) => p.id === c.herd?.paddock_id),
  Tool: lazy(() => import("./StripTool").then((m) => ({ default: m.StripTool }))),
});
// Anything on the map to catch.
tools.register({
  id: "lasso", label: "Lasso", key: "l", group: "bar", order: 40, minRole: "manager",
  when: () => store.get().collars.some((c) => c.last_fix),
  Tool: lazy(() => import("./LassoTool").then((m) => ({ default: m.LassoTool }))),
});

// Shift-drag lassos too; its code arrives with the map.
overlays.register({
  id: "c-lasso",
  slot: "slot-top",
  mount(ctx) {
    let h: OverlayHandle | undefined;
    let gone = false;
    void import("./overlay").then((m) => {
      if (!gone) h = m.mountShiftLasso(ctx);
    });
    return {
      destroy() {
        gone = true;
        h?.destroy();
      },
    };
  },
});

type SheetProps = { paddock: Paddock; herdId?: string };
const later = (C: ComponentType<SheetProps>) => (p: SheetProps) => createElement(Suspense, { fallback: null }, createElement(C, p));
const Layouts = lazy(() => import("./PaddockSections").then((m) => ({ default: m.PaddockLayouts })));
const Copy = lazy(() => import("./PaddockSections").then((m) => ({ default: m.PaddockCopy })));
paddockSheet.register({ id: "c-layouts", order: PADDOCK_SHEET.notes + 10, minRole: "manager", Section: later(Layouts) });
paddockSheet.register({ id: "c-copy", order: PADDOCK_SHEET.actions + 1, minRole: "manager", Section: later(Copy) });
