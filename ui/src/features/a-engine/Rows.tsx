// Alert rows, the .esc sentence rows: "214 outside P3  6m" Ack Resolve. Clicking the words
// flies the map to it. Used by the herd panel, its sheet and the top bar's list.

import { useState } from "react";
import type { Alert } from "../../api";
import { alertsApi } from "../../api/a-engine";
import { alerts, applyAlert } from "../../store/a-engine";
import { useCan } from "../../store/me";
import { Button } from "../../ui";
import { useNow } from "../../util";
import { focus } from "./focus";
import { ageText, byUrgency, since } from "./model";

export function AlertRow({ a, now, onPick }: { a: Alert; now: number; onPick?: () => void }) {
  const hand = useCan("hand");
  const [busy, setBusy] = useState(false);
  const act = async (f: (id: string) => Promise<Alert>) => {
    setBusy(true);
    try {
      applyAlert(await f(a.id));
    } catch {
      /* the live event or the next load shows where it stands */
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="acts arow" data-sev={a.severity} data-acked={a.status === "acked" || undefined}>
      <button type="button" className="behind" title={a.title} onClick={() => { onPick?.(); focus(a); }}>{a.title}</button>
      <span className="rem mono" title={new Date(since(a)).toLocaleString()}>{ageText(since(a), now)}</span>
      {hand && (
        <span className="aacts">
          {a.status === "open" && <Button small kind="plain" disabled={busy} onClick={() => act(alertsApi.ack)}>Ack</Button>}
          <Button small kind="plain" disabled={busy} onClick={() => act(alertsApi.resolve)}>Resolve</Button>
        </span>
      )}
    </div>
  );
}

export function AlertRows({ list, onPick }: { list: Alert[]; onPick?: () => void }) {
  const now = useNow(15_000);
  return <>{list.map((a) => <AlertRow key={a.id} a={a} now={now} onPick={onPick} />)}</>;
}

// Every unresolved alert of one herd, for the sheet the panel's "4 more" opens.
export function HerdSheet({ herdId }: { herdId: string }) {
  const list = alerts.use((s) => s.list);
  const rows = list.filter((a) => a.herd_id === herdId && a.kind !== "decision_waiting").sort(byUrgency);
  return <div className="esc asheet"><AlertRows list={rows} /></div>;
}
