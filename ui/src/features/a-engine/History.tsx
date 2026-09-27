// Data > Alerts: every alert the herd had in the range, newest first.

import { useEffect, useMemo, useState } from "react";
import type { Alert } from "../../api";
import { alertsApi } from "../../api/a-engine";
import type { DataSectionProps } from "../../registry";
import { alerts } from "../../store/a-engine";
import { Table, type Column } from "../../ui/Table";
import { openFor, RANK, statusText } from "./model";

const when = (iso: string) =>
  new Date(iso).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false });

export function AlertHistory({ herdId, from, to }: DataSectionProps) {
  const [rows, setRows] = useState<Alert[] | null>(null);
  // A live change to any alert reloads the range.
  const live = alerts.use((s) => s.list);
  useEffect(() => {
    let gone = false;
    alertsApi.list({ status: "all", herd_id: herdId, from, to, limit: 1000 }).then((r) => !gone && setRows(r), () => !gone && setRows(null));
    return () => {
      gone = true;
    };
  }, [herdId, from, to, live]);
  const now = Date.now();
  const columns = useMemo<Column<Alert>[]>(() => [
    { id: "opened", label: "Opened", width: 130, sort: (a, b) => a.opened_at.localeCompare(b.opened_at), cell: (a) => <span className="mono">{when(a.opened_at)}</span> },
    { id: "alert", label: "Alert", sort: (a, b) => RANK[a.severity] - RANK[b.severity], cell: (a) => <span className="ahist" data-sev={a.severity}>{a.title}</span> },
    { id: "status", label: "Status", width: 180, cell: (a) => <span className="dim">{statusText(a)}</span> },
    { id: "for", label: "Open for", width: 96, cell: (a) => <span className="mono dim">{openFor(a, now)}</span> },
  ], [now]);
  if (!rows?.length) return null;
  return (
    <div className="ahistory">
      <Table rows={rows} columns={columns} rowKey={(a) => a.id} text={(a) => `${a.title} ${a.kind} ${statusText(a)}`} filter={rows.length > 12}
        height={Math.min(rows.length, 10) * 37 + 42 + (rows.length > 12 ? 44 : 0)} />
    </div>
  );
}
