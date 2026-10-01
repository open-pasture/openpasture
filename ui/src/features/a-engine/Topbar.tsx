// Top bar: the 6 px dot (red with any critical) and "2 open"; nothing at zero. It, or the
// `a` key, opens the list of every unresolved alert.

import { useEffect, useRef } from "react";
import { useStore } from "../../store";
import { alertList, alerts, loadAlerts } from "../../store/a-engine";
import { openCount } from "./model";
import { AlertRows } from "./Rows";

export function AlertsTopbar() {
  const up = useStore((s) => s.up);
  // Loaded when the app shows, and again whenever the live feed comes back (events were missed).
  useEffect(() => {
    void loadAlerts();
  }, [up]);
  const list = alerts.use((s) => s.list);
  const open = alertList.use((s) => s.open);
  const ref = useRef<HTMLDivElement>(null);
  const { n, critical } = openCount(list);

  useEffect(() => {
    if (!open) return;
    const down = (e: MouseEvent) => ref.current?.contains(e.target as Node) || alertList.set({ open: false });
    const key = (e: KeyboardEvent) => e.key === "Escape" && alertList.set({ open: false });
    window.addEventListener("mousedown", down);
    window.addEventListener("keydown", key);
    return () => {
      window.removeEventListener("mousedown", down);
      window.removeEventListener("keydown", key);
    };
  }, [open]);
  useEffect(() => {
    if (open && !list.length) alertList.set({ open: false });
  }, [open, list.length]);

  if (!n && !open) return null;
  return (
    <div className="alertbtn" ref={ref}>
      <button type="button" className="atrigger" aria-expanded={open} aria-haspopup="dialog" title="Alerts (A)"
        onClick={() => alertList.set({ open: !open })}>
        {n > 0 && <i className={"dot " + (critical ? "crit" : "warn")} />}
        {n > 0 && <span className="mono">{n}<span className="aword"> open</span></span>}
      </button>
      {open && (
        <div className="alist esc" role="dialog" aria-label="Alerts">
          <AlertRows list={list} onPick={() => alertList.set({ open: false })} />
        </div>
      )}
    </div>
  );
}
