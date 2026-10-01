// The desktop farm sidebar, under the paddocks: the herd's next call, then its last few as a
// thread, newest at the top, each with when and how it ended. One with a shape draws it faint on
// the map (Esc clears it), as Data > Decisions does; the whole list is there.

import { useEffect, useMemo, useState } from "react";
import { api, type Decision, type Polygon } from "../../api";
import { useStore } from "../../store";
import { showGhost } from "../../store/b";
import { age, useNow } from "../../util";
import { inWords, minutesToCall } from "../../shell/asks";
import { sentence } from "./timeline";

const SHOWN = 5;

export function FarmCalls({ herdId }: { herdId?: string }) {
  const state = useStore((s) => s.state)!;
  const live = useStore((s) => s.decisions);
  const now = useNow(60_000);
  const at = useStore((s) => s.state?.settings.decision_time);
  const tz = useStore((s) => s.state?.farm?.timezone);
  const [list, setList] = useState<Decision[]>([]);
  useEffect(() => {
    if (!herdId) return;
    let on = true;
    api.decisions(herdId, SHOWN * 2).then((l) => on && setList(l), () => on && setList([]));
    return () => {
      on = false;
    };
  }, [herdId]);

  // The store keeps the herd's latest decisions current from the live feed.
  const rows = useMemo(() => {
    const byId = new Map(list.map((d) => [d.id, d]));
    for (const d of live) if (d.herd_id === herdId) byId.set(d.id, d);
    return [...byId.values()].sort((a, b) => b.created_at.localeCompare(a.created_at)).slice(0, SHOWN);
  }, [list, live, herdId]);
  const running = rows.some((d) => d.status === "running");
  const left = at ? minutesToCall(at, tz, now) : undefined;
  if (!rows.length && left === undefined) return null;

  const name = (id?: string) => state.paddocks.find((p) => p.id === id)?.name;
  const shape = (d: Decision): Polygon | undefined => d.geometry ?? state.paddocks.find((p) => p.id === d.to_paddock_id)?.geometry;

  return (
    <ol className="fcalls" aria-label="Calls">
      {/* What comes next leads the thread: the daily call, or the one being made now. */}
      {(running || left !== undefined) && (
        <li data-status={running ? "running" : "next"}>
          <div>
            <i aria-hidden="true" />
            <span className="fcw">{running ? "Deciding now" : `Next call at ${at!.slice(0, 5)}`}</span>
            {!running && <span className="fcm mono"><span>{inWords(left!)}</span></span>}
          </div>
        </li>
      )}
      {rows.filter((d) => d.status !== "running").map((d) => {
        const g = shape(d);
        const body = (
          <>
            <i aria-hidden="true" />
            <span className="fcw">{sentence(d, name)}</span>
            <span className="fcm mono"><span data-status={d.status}>{d.status}</span><span>{age(d.created_at, now)} ago</span></span>
          </>
        );
        return (
          <li key={d.id} data-status={d.status}>
            {g ? <button type="button" title="Show on the map" onClick={() => showGhost({ id: d.id, geometry: g })}>{body}</button> : <div>{body}</div>}
          </li>
        );
      })}
    </ol>
  );
}
