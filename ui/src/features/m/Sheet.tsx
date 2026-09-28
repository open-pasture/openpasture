// The herd panel as a bottom sheet under 760 px: a grab at its top, three rests (peek: the herd's
// alerts, the decision sentence and its buttons, the next move; half; full). Tap the grab to step
// through them, drag it (or the peek itself) to resize, fling to jump. It sizes the panel through
// the view's --m-sheet, which the map's controls use to stay above it.

import { useEffect, useLayoutEffect, useRef } from "react";
import { sheet } from "../../store/m";
import { usePhone } from "./phone";
import { nextState, settle, stops } from "./sheet-model";

// The blocks the peek shows (with the herd name above them), in the panel's order.
const PEEK = [".ph", ".esc.alerts", ".dec", ".sched"];
// Presses on these are for them, not for dragging the sheet.
const CONTROLS = "button, a, input, select, textarea, label, [role=slider], [role=radio], [contenteditable]";

export function SheetGrab({ herdId }: { herdId: string }) {
  const phone = usePhone();
  const ref = useRef<HTMLButtonElement>(null);
  const state = sheet.use((s) => s.state);
  const dragged = useRef(false);
  useEffect(() => {
    sheet.patch({ state: "peek" });
  }, [herdId]);

  useLayoutEffect(() => {
    const grab = ref.current;
    const panel = grab?.closest<HTMLElement>("aside.panel");
    const view = panel?.closest<HTMLElement>(".mapview");
    if (!phone || !grab || !panel || !view) return;
    let content = 0;
    let dragging = false;
    // The peek's height is its blocks' bottom, measured while it rests there (the blocks it hides, the
    // autonomy switch and the reasons, would count otherwise).
    const measure = () => {
      if (sheet.get().state !== "peek" || dragging) return;
      const top = panel.getBoundingClientRect().top - panel.scrollTop;
      let bottom = grab.getBoundingClientRect().bottom;
      for (const sel of PEEK)
        for (const el of panel.querySelectorAll<HTMLElement>(`:scope > ${sel}`))
          if (el.offsetParent) bottom = Math.max(bottom, el.getBoundingClientRect().bottom);
      content = Math.ceil(bottom - top) + 8;
    };
    const set = (h: number) => view.style.setProperty("--m-sheet", `${Math.round(h)}px`);
    const apply = () => {
      const s = sheet.get().state;
      panel.dataset.mSheet = s;
      measure();
      const h = stops(view.clientHeight, content)[s];
      set(h);
      if (s === "peek") panel.scrollTop = 0;
      if (sheet.get().h !== h) sheet.patch({ h });
    };

    // Sections come and go and grow (an alert opens, the queue unfolds): measure again.
    const ro = new ResizeObserver(() => apply());
    const watchChildren = () => {
      ro.disconnect();
      ro.observe(view);
      for (const el of panel.children) ro.observe(el);
    };
    const mo = new MutationObserver(() => {
      watchChildren();
      apply();
    });
    mo.observe(panel, { childList: true });
    watchChildren();
    const off = sheet.subscribe(apply);
    apply();

    const down = (e: PointerEvent) => {
      if (e.button !== 0) return;
      const onGrab = grab.contains(e.target as Node);
      if (!onGrab && (sheet.get().state !== "peek" || (e.target as Element).closest(CONTROLS))) return;
      const y0 = e.clientY;
      const h0 = panel.getBoundingClientRect().height;
      let h = h0;
      let moved = false;
      let last = { t: e.timeStamp, y: e.clientY };
      let v = 0;
      const move = (ev: PointerEvent) => {
        const dy = y0 - ev.clientY;
        if (!moved && Math.abs(dy) < 6) return;
        if (!moved) {
          moved = true;
          dragging = true;
          panel.classList.add("mdrag");
        }
        const s = stops(view.clientHeight, content);
        h = Math.min(s.full, Math.max(s.peek * 0.75, h0 + dy));
        set(h);
        const dt = ev.timeStamp - last.t;
        if (dt > 0) v = (last.y - ev.clientY) / dt;
        last = { t: ev.timeStamp, y: ev.clientY };
      };
      const up = () => {
        window.removeEventListener("pointermove", move);
        window.removeEventListener("pointerup", up);
        window.removeEventListener("pointercancel", up);
        if (!moved) return;
        dragging = false;
        panel.classList.remove("mdrag");
        // A mouse drag ends in a click on the grab; a touch drag doesn't.
        dragged.current = true;
        setTimeout(() => (dragged.current = false), 60);
        const next = settle(stops(view.clientHeight, content), h, v);
        if (next === sheet.get().state) apply();
        else sheet.patch({ state: next });
      };
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", up);
      window.addEventListener("pointercancel", up);
    };
    panel.addEventListener("pointerdown", down);
    return () => {
      panel.removeEventListener("pointerdown", down);
      off();
      ro.disconnect();
      mo.disconnect();
      view.style.removeProperty("--m-sheet");
      delete panel.dataset.mSheet;
      sheet.patch({ h: 0 });
    };
  }, [phone]);

  if (!phone) return null;
  return (
    <button ref={ref} type="button" className="mgrab" aria-label="Herd" aria-expanded={state !== "peek"}
      onClick={() => {
        // The end of a drag isn't a tap.
        if (dragged.current) return void (dragged.current = false);
        sheet.patch({ state: nextState(state) });
      }} />
  );
}
