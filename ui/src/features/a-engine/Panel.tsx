// The herd panel's first rows: its open alerts, three at most, then "4 more" for a sheet.

import { alerts } from "../../store/a-engine";
import { openSheet } from "./focus";
import { herdRows } from "./model";
import { AlertRows, HerdSheet } from "./Rows";

const SHOWN = 3;

export function HerdAlerts({ herdId }: { herdId: string }) {
  const list = alerts.use((s) => s.list);
  const rows = herdRows(list, herdId);
  if (!rows.length) return null;
  const more = rows.length - SHOWN;
  return (
    <section className="esc alerts" aria-label="Alerts">
      <AlertRows list={rows.slice(0, SHOWN)} />
      {more > 0 && (
        <button type="button" className="behind more mono" onClick={() => openSheet(<HerdSheet herdId={herdId} />)}>{more} more</button>
      )}
    </section>
  );
}
