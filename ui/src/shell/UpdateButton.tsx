import { useCallback, useEffect, useState } from "react";
import { Icon } from "../ui";

// The desktop app's updater, over the two commands its capability allows the served UI.
// In a browser there is no updater, so nothing shows.
type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
interface UpdateStatus { version: string; available?: string | null; busy: boolean }
const tauriInvoke = (): Invoke | undefined =>
  (window as unknown as { __TAURI_INTERNALS__?: { invoke?: Invoke } }).__TAURI_INTERNALS__?.invoke;

export function UpdateButton() {
  const [invoke] = useState(tauriInvoke);
  const [st, setSt] = useState<UpdateStatus>();
  const refresh = useCallback(() => {
    invoke?.("update_status").then((s) => setSt(s as UpdateStatus), () => setSt(undefined));
  }, [invoke]);
  const busy = !!st?.busy;
  useEffect(() => {
    if (!invoke) return;
    refresh();
    // Quick while a check or install runs; the app's own check runs every six hours.
    const t = setInterval(refresh, busy ? 1000 : 60_000);
    return () => clearInterval(t);
  }, [invoke, refresh, busy]);
  if (!invoke || !st) return null;
  const title = busy ? "Checking for updates" : st.available ? `Update to openpasture ${st.available}` : `openpasture ${st.version}. Check for updates`;
  return (
    <button type="button" className={"upd" + (st.available ? " on" : "")} disabled={busy} title={title} aria-label={title}
      onClick={() => invoke?.("update_check").then(() => setTimeout(refresh, 250), () => {})}>
      <Icon name="navup" size={12} accent="currentColor" />
      <span>{st.available ? "Update" : busy ? "Checking" : st.version}</span>
    </button>
  );
}
