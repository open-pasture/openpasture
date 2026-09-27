// Shift-drag anywhere on the map lassos animals and opens the Lasso tool with them. MapLibre
// gives shift-drag to box zoom, so box zoom is off while this overlay is mounted.

import type { MapMouseEvent } from "maplibre-gl";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { cState } from "../../store/c";
import { can } from "../../store/me";
import { caughtIn, drag, lassoLayers, removeLasso, setLasso, startTool, toolsIdle } from "./freehand";

export function mountShiftLasso(ctx: OverlayCtx): OverlayHandle {
    const { map } = ctx;
    map.boxZoom.disable();
    let stop: (() => void) | undefined;
    const down = (e: MapMouseEvent) => {
      if (!e.originalEvent.shiftKey || e.originalEvent.button !== 0) return;
      if (!can("manager") || !toolsIdle(map)) return;
      lassoLayers(map, ctx.beforeId("slot-top"));
      stop = drag(map, e, (ring) => {
        if (!ring) return setLasso(map);
        cState.patch({ lasso: { ring, ids: caughtIn(ring, ctx.positions()), at: Date.now() } });
        startTool("l");
      });
    };
    map.on("mousedown", down);
    return {
      destroy() {
        map.off("mousedown", down);
        stop?.();
        removeLasso(map);
        map.boxZoom.enable();
      },
    };
}
