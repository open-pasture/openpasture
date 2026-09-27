// Data > Messages: every text, email and webhook in the Data range, newest first, live.

import { useEffect, useMemo, useState } from "react";
import type { MessageLog } from "../../api";
import { notifyApi } from "../../api/a-notify";
import type { DataSectionProps } from "../../registry";
import { store } from "../../store";
import { Table, type Column } from "../../ui/Table";
import { fmtPhone, fmtWhen, statusText, upsertMessage } from "./logic";

const ROW = 37;

export function Messages({ from, to }: DataSectionProps) {
  const [rows, setRows] = useState<MessageLog[] | null>(null);
  useEffect(() => {
    let live = true;
    notifyApi.messages({ from, to, limit: 1000 }).then((r) => live && setRows(r), () => live && setRows(null));
    return () => void (live = false);
  }, [from, to]);
  useEffect(() => store.on("message", (e) => setRows((r) => (r ? upsertMessage(r, e.message) : r))), []);

  const columns = useMemo<Column<MessageLog>[]>(() => [
    { id: "at", label: "time", width: 120, sort: (a, b) => a.created_at.localeCompare(b.created_at), cell: (m) => fmtWhen(m.created_at) },
    { id: "dir", label: "", width: 56, cell: (m) => <span className="dim">{m.direction}</span> },
    { id: "channel", label: "channel", width: 96, sort: (a, b) => a.channel.localeCompare(b.channel), cell: (m) => m.channel },
    { id: "who", label: "address", width: 180, cell: (m) => <span title={m.address}>{fmtPhone(m.address)}</span> },
    { id: "kind", label: "kind", width: 80, sort: (a, b) => a.kind.localeCompare(b.kind), cell: (m) => m.kind },
    { id: "text", label: "text", cell: (m) => <span title={m.subject ? `${m.subject}\n\n${m.text}` : m.text}>{m.text}</span> },
    {
      id: "status", label: "status", width: 220, sort: (a, b) => a.status.localeCompare(b.status),
      cell: (m) => {
        const s = statusText(m);
        return <span className={s.tone} title={s.text}>{s.text}</span>;
      },
    },
  ], []);

  if (!rows?.length) return null;
  const height = Math.min(rows.length, 12) * ROW + ROW + 52;
  return (
    <div className="msgs">
      <Table rows={rows} columns={columns} rowKey={(m) => m.id} filter height={height}
        text={(m) => `${m.channel} ${m.address} ${fmtPhone(m.address)} ${m.kind} ${m.text} ${m.status} ${m.error ?? ""}`} />
    </div>
  );
}
