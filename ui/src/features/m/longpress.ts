// Drawing by touch: a long press finishes the shape being drawn (a boundary, paddock, exclusion or
// line) with the corners placed so far, as Enter does with a keyboard. terra-draw listens for keys on
// the map's canvas, so the press becomes an Enter there.

import type { Map as MLMap } from "maplibre-gl";

// How long a press is held, and how far the finger may wander meanwhile.
export const HOLD_MS = 500;
export const SLOP_PX = 10;
const DRAWING = new Set(["paddock", "boundary", "exclusion", "line"]);

export function finishOnLongPress(map: MLMap, mode: () => string) {
  const el = map.getCanvasContainer();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let at: [number, number] = [0, 0];
  const cancel = () => {
    clearTimeout(timer);
    timer = undefined;
  };
  el.addEventListener("touchstart", (e) => {
    cancel();
    if (e.touches.length !== 1 || !DRAWING.has(mode())) return;
    at = [e.touches[0].clientX, e.touches[0].clientY];
    timer = setTimeout(() => {
      timer = undefined;
      map.getCanvas().dispatchEvent(new KeyboardEvent("keyup", { key: "Enter", bubbles: true }));
      navigator.vibrate?.(12);
    }, HOLD_MS);
  }, { passive: true });
  el.addEventListener("touchmove", (e) => {
    const t = e.touches[0];
    if (timer && (e.touches.length !== 1 || Math.hypot(t.clientX - at[0], t.clientY - at[1]) > SLOP_PX)) cancel();
  }, { passive: true });
  el.addEventListener("touchend", cancel, { passive: true });
  el.addEventListener("touchcancel", cancel, { passive: true });
}
