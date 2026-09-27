import { useEffect, useMemo, useState } from "react";
import { api, type Animal, type Behaviour as Row, type Collar } from "../../api";
import type { DataSectionProps } from "../../registry";
import { useStore } from "../../store";
import { Spark } from "../../ui/Spark";
import { Table, type Column } from "../../ui/Table";
import { num, useUnits, type Fmt } from "../../units";

// Metres walked on an average day with data.
const perDay = (r: Row) => (r.days.length ? (r.distance_km * 1000) / r.days.length : 0);
const hours = (h: number) => (h > 0 ? `${num(h, h < 10 ? 1 : 0)} h` : "0 h");
// Cues a day falling, rising or flat over the range (falling: the animal meets the line less).
const trend = (r: Row) => r.learning?.trend;

function columns(u: Fmt, label: (r: Row) => string): Column<Row>[] {
  return [
    { id: "animal", label: "animal", width: 140, sort: (a, b) => label(a).localeCompare(label(b), undefined, { numeric: true }), cell: label },
    { id: "walk", label: "walked a day", width: 130, sort: (a, b) => perDay(a) - perDay(b), cell: (r) => u.len(perDay(r)) },
    { id: "out", label: "outside", width: 90, sort: (a, b) => a.outside_hours - b.outside_hours, cell: (r) => hours(r.outside_hours) },
    { id: "cues", label: "cues", width: 70, sort: (a, b) => a.cues - b.cues, cell: (r) => r.cues },
    { id: "day", label: "cues a day", width: 110, cell: (r) => <Spark values={r.cues_per_day} width={72} label={`Cues a day: ${r.cues_per_day.join(", ")}`} /> },
    { id: "trend", label: "cues trend", cell: (r) => <span className={r.learning?.trend === "falling" ? "ok" : undefined}>{trend(r) ?? "–"}</span> },
  ];
}

// Data > Behaviour: how each animal moved and met the line over the range.
export function Behaviour({ herdId, from, to }: DataSectionProps) {
  const u = useUnits();
  const collars = useStore((s) => s.collars);
  const [rows, setRows] = useState<Row[]>([]);
  useEffect(() => {
    if (!herdId) return;
    let on = true;
    api.behaviour({ herd_id: herdId, from, to }).then((r) => on && setRows(r), () => on && setRows([]));
    return () => {
      on = false;
    };
  }, [herdId, from, to]);
  const label = useMemo(() => (r: Row) => r.tag ?? r.name ?? collars.find((c) => c.id === r.collar_id)?.name ?? r.collar_id, [collars]);
  const cols = useMemo(() => columns(u, label), [u, label]);
  const shown = rows.filter((r) => r.fixes > 0);
  if (!shown.length) return null;
  return (
    <div className="bbehave">
      <Table rows={shown} columns={cols} rowKey={(r) => r.collar_id} text={label} filter={shown.length > 12}
        height={Math.min(33 + shown.length * 37 + (shown.length > 12 ? 40 : 0), 480)} initialSort={{ col: "cues", dir: "desc" }} />
    </div>
  );
}

// The animal page: the same, for one animal over the last week.
export function AnimalBehaviour({ animal, collar }: { animal?: Animal; collar?: Collar }) {
  const u = useUnits();
  const herdId = animal?.herd_id ?? collar?.herd_id;
  const collarId = collar?.id ?? animal?.collar_id;
  const [row, setRow] = useState<Row | null>(null);
  useEffect(() => {
    if (!herdId) return;
    let on = true;
    api.behaviour({ herd_id: herdId, from: "-7d", to: "now" }).then(
      (rows) => on && setRow(rows.find((r) => (collarId && r.collar_id === collarId) || (animal && r.animal_id === animal.id)) ?? null),
      () => on && setRow(null),
    );
    return () => {
      on = false;
    };
  }, [herdId, collarId, animal]);
  if (!row || row.fixes === 0) return null;
  return (
    <ul className="kv bbehave1">
      <li><span>walked a day</span><b>{u.len(perDay(row))}</b></li>
      <li><span>outside</span><b>{hours(row.outside_hours)}</b></li>
      <li><span>cues</span><b className="bspark">{row.cues}<Spark values={row.cues_per_day} width={72} label={`Cues a day: ${row.cues_per_day.join(", ")}`} /></b></li>
      {trend(row) && <li><span>cues trend</span><b className={row.learning?.trend === "falling" ? "ok" : undefined}>{trend(row)}</b></li>}
    </ul>
  );
}
