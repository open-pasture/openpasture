// The Herd view's desktop sidebar: the herd's animals (and collars no animal wears), each with its
// collar's state and battery, to step between animal pages. Filters for the ones that need a look:
// out of the boundary, in the warning band, battery low.

import { useLayoutEffect, useMemo, useRef, useState } from "react";
import type { Collar } from "../../api";
import type { HerdRow } from "../../registry";
import { useStore } from "../../store";
import { LOW_BATTERY } from "../../store/live";
import { Input } from "../../ui";
import { battery, herdRows, pageHash, pageKeys, rowLabel, rowText } from "./herd";

type Filter = "all" | "out" | "near" | "low";
const FILTERS: { id: Filter; label: string; test: (c?: Collar) => boolean }[] = [
  { id: "all", label: "All", test: () => true },
  { id: "out", label: "Out", test: (c) => c?.state === "outside" },
  { id: "near", label: "Near", test: (c) => c?.state === "warning" },
  { id: "low", label: "Low", test: (c) => c?.battery !== undefined && c.battery < LOW_BATTERY },
];

export function AnimalsSidebar({ rest }: { rest: string }) {
  const animals = useStore((s) => s.animals);
  const collars = useStore((s) => s.collars);
  const herdId = useStore((s) => s.herdId);
  const [filter, setFilter] = useState<Filter>("all");
  const [q, setQ] = useState("");
  const rows = useMemo(
    () => herdRows(animals, collars, herdId, "active").sort((a, b) => rowLabel(a).localeCompare(rowLabel(b), undefined, { numeric: true })),
    [animals, collars, herdId],
  );
  const keyOf = useMemo(() => pageKeys(animals), [animals]);
  // The animal page showing, by the key in its hash.
  const open = rest && !rest.startsWith("?") ? decodeURIComponent(rest.split("/")[0]) : undefined;
  const counts = Object.fromEntries(FILTERS.map((f) => [f.id, rows.filter((r) => f.test(r.collar)).length])) as Record<Filter, number>;
  const test = FILTERS.find((f) => f.id === filter)!.test;
  const needle = q.trim().toLowerCase();
  const shown = rows.filter((r) => test(r.collar) && (!needle || rowText(r).toLowerCase().includes(needle)));

  // The open animal stays in sight as pages change.
  const list = useRef<HTMLUListElement>(null);
  useLayoutEffect(() => {
    list.current?.querySelector<HTMLElement>("[aria-current]")?.scrollIntoView({ block: "nearest" });
  }, [open]);

  return (
    <div className="alistside">
      <div className="afilter">
        <Input className="sm" placeholder="Filter" aria-label="Filter animals" value={q} onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => e.key === "Escape" && (setQ(""), (e.target as HTMLInputElement).blur())} />
        <div className="achips" role="tablist" aria-label="Show">
          {FILTERS.filter((f) => f.id === "all" || counts[f.id] > 0 || filter === f.id).map((f) => (
            <button key={f.id} type="button" role="tab" aria-selected={filter === f.id} data-f={f.id} onClick={() => setFilter(f.id)}>
              {f.label}<span className="mono">{counts[f.id]}</span>
            </button>
          ))}
        </div>
      </div>
      <ul className="arows" ref={list} aria-label="Animals">
        {shown.map((r) => <Row key={r.id} row={r} href={`#${pageHash(keyOf(r.animal, r.collar))}`} on={open !== undefined && (open === r.animal?.tag || open === r.animal?.id || open === r.collar?.id)} />)}
        {shown.length === 0 && <li className="anone mono">{needle ? "No match" : "None"}</li>}
      </ul>
    </div>
  );
}

function Row({ row, href, on }: { row: HerdRow; href: string; on: boolean }) {
  const c = row.collar;
  const low = c?.battery !== undefined && c.battery < LOW_BATTERY;
  return (
    <li>
      <a href={href} aria-current={on ? "true" : undefined} data-state={c?.state ?? "none"}>
        <i className="ast" aria-hidden="true" />
        <span className="atag mono">{row.animal?.tag ?? c?.name}</span>
        <span className="aname">{row.animal?.name ?? (row.animal ? "" : "no animal")}</span>
        <span className={"abat mono" + (low ? " low" : "")}>{battery(c?.battery)}</span>
      </a>
    </li>
  );
}
