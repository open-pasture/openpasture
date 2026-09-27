import { useEffect, useMemo, useState } from "react";
import { api, type Decision, type Polygon } from "../../api";
import type { DataSectionProps } from "../../registry";
import { useStore } from "../../store";
import { showGhost } from "../../store/b";
import { day, detail, hhmm, sentence, since } from "./timeline";

// Data > Decisions: every call for the herd in the range, newest first. An entry with a shape
// opens the map with that shape drawn faint (Esc clears it).
export function Decisions({ herdId, from }: DataSectionProps) {
  const state = useStore((s) => s.state)!;
  const live = useStore((s) => s.decisions);
  const tz = state.farm?.timezone;
  const [list, setList] = useState<Decision[]>([]);
  useEffect(() => {
    if (!herdId) return;
    let on = true;
    api.decisions(herdId, 500).then((l) => on && setList(l), () => on && setList([]));
    return () => {
      on = false;
    };
  }, [herdId, from]);

  // The store keeps the herd's latest decisions current from the live feed.
  const rows = useMemo(() => {
    const byId = new Map(list.map((d) => [d.id, d]));
    for (const d of live) if (d.herd_id === herdId) byId.set(d.id, d);
    return since([...byId.values()].sort((a, b) => b.created_at.localeCompare(a.created_at)), from);
  }, [list, live, herdId, from]);

  if (!rows.length) return null;
  const name = (id?: string) => state.paddocks.find((p) => p.id === id)?.name;
  const shape = (d: Decision): Polygon | undefined => d.geometry ?? state.paddocks.find((p) => p.id === d.to_paddock_id)?.geometry;

  return (
    <ul className="btl">
      {rows.map((d) => {
        const g = shape(d);
        const body = (
          <>
            <span className="mono dim when">{day(d.created_at, tz)} {hhmm(d.created_at, tz)}</span>
            <span className="what">{sentence(d, name)}</span>
            <span className="mono st" data-status={d.status}>{d.status}</span>
            <span className="mono dim how">{detail(d, tz)}</span>
            {d.reasoning && <span className="why">{d.reasoning}</span>}
          </>
        );
        return (
          <li key={d.id}>
            {g ? <button type="button" onClick={() => showGhost({ id: d.id, geometry: g })}>{body}</button> : <div>{body}</div>}
          </li>
        );
      })}
    </ul>
  );
}
