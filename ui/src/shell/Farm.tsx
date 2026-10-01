// The farm sidebar: the herd everything is about, then every paddock as it lies on the ground,
// with who is on it and how long the rest have rested, then what features add (FARM_SIDE).
// Hovering a paddock here lights it on the map, and hovering it on the map lights it here.

import { useEffect, useMemo, useState } from "react";
import { api, type Decision, type Paddock, type PastureRow, type Polygon } from "../api";
import { toXY } from "../geo";
import { FARM_SIDE, farmSide, interleave, sectionNodes, useSections } from "../registry";
import { store, useStore } from "../store";
import { useUnits } from "../units";
import { Icon, Menu, Mark } from "../ui";
import { ask, mapState } from "./bus";

// The rest figures change by the hour; a decision or a move changes them at once.
const PASTURE_EVERY_MS = 5 * 60_000;
const PASTURE_DELAY_MS = 2500;

export function FarmSidebar() {
  const herdId = useStore((s) => s.herdId);
  const props = { herdId };
  const added = sectionNodes(useSections(farmSide, props), props);
  return interleave([{ key: "paddocks", order: FARM_SIDE.paddocks, node: <Paddocks /> }], added);
}

function Paddocks() {
  const state = useStore((s) => s.state)!;
  const herdId = useStore((s) => s.herdId);
  const decisions = useStore((s) => s.decisions);
  const sel = mapState.use((v) => (v.on ? v.paddock : undefined));
  const lit = mapState.use((v) => (v.on ? v.hoverPaddock : undefined));
  const u = useUnits();
  const visits = useVisits(herdId, decisions);
  const pasture = usePasture(`${decisions[0]?.id}:${decisions[0]?.status}:${state.herds.map((h) => h.paddock_id).join()}`);
  const next = decisions.find((d) => d.status === "proposed")?.to_paddock_id;
  // Which herds are in each paddock, by the herds' own records.
  const held = new Map<string, string[]>();
  for (const h of state.herds) if (h.paddock_id) held.set(h.paddock_id, [...(held.get(h.paddock_id) ?? []), h.id]);
  const herdName = (id: string) => state.herds.find((h) => h.id === id)?.name ?? "";
  const list = state.paddocks.slice().sort((a, b) => a.name.localeCompare(b.name, undefined, { numeric: true }));
  // Rest is the time since a herd last spent a real share of a day there (as Data > Pasture), and
  // reads against the most rested paddock on the farm: the fullest bar is the longest rest.
  const now = Date.now();
  const restOf = (r?: PastureRow) => (r?.last_grazed ? Math.max(0, (now - Date.parse(r.last_grazed)) / 86400e3) : undefined);
  const most = Math.max(1, ...pasture.filter((r) => !held.has(r.paddock_id)).map((r) => restOf(r) ?? 0));

  const open = (p: Paddock) => {
    if (!mapState.get().on) location.hash = "#/map";
    ask({ k: "paddock", id: p.id });
  };
  const hover = (id?: string) => mapState.get().on && ask({ k: "hoverPaddock", id });

  return (
    <ul className="plist" aria-label="Paddocks" onMouseLeave={() => hover(undefined)}>
      {list.map((p) => {
        const r = pasture.find((x) => x.paddock_id === p.id);
        const here = held.get(p.id) ?? [];
        const mine = !!herdId && here.includes(herdId);
        const rest = here.length ? undefined : restOf(r);
        const tone = mine ? "grass" : p.id === next ? "blaze" : undefined;
        return (
          <li key={p.id}>
            <button type="button" data-tone={tone} data-lit={lit === p.id || undefined}
              aria-current={sel === p.id ? "true" : undefined}
              title={r?.pressure ? `${auDays(u.toDisplay(r.pressure, "density"))} AU·d/${u.unitLabel("area")}` : undefined}
              onClick={() => open(p)} onMouseEnter={() => hover(p.id)} onFocus={() => hover(p.id)}>
              <Shape poly={p.geometry} />
              <span className="pn">{p.name}</span>
              <span className="pa mono">{u.area(p.area_ha)}</span>
              <span className="pm mono">
                {here.length > 0 ? (
                  <>
                    <i className="plive" data-mine={mine || undefined} />
                    <span data-tone={mine ? "grass" : "fg"}>{here.map(herdName).join(", ")}</span>
                    {r?.grazing_days ? <span>day {Math.max(1, Math.round(r.grazing_days))}</span> : null}
                  </>
                ) : p.id === next ? (
                  <span data-tone="blaze">next</span>
                ) : p.status === "planned" ? (
                  <span>planned</span>
                ) : rest !== undefined ? (
                  <span>rested {restWords(rest)}</span>
                ) : (
                  <span>resting</span>
                )}
              </span>
              {visits.length > 0 ? (
                <Strip spans={visits.filter((v) => v.pad === p.id)} now={now} />
              ) : here.length === 0 && rest !== undefined && (
                <span className="pbar" aria-hidden="true"><i style={{ width: `${Math.max(2, (rest / most) * 100)}%` }} /></span>
              )}
            </button>
          </li>
        );
      })}
    </ul>
  );
}

// ---- where the herd has been ------------------------------------------------------------

// The strip's span: the last thirty days.
const STRIP_MS = 30 * 86400e3;

interface Visit { pad: string; from: number; to: number }

// The herd's stays over the last thirty days, from the moves that took it into each paddock: a
// stay runs from one move to the next, the last one to now. A herd with no move on record has
// none, and the rows keep their rest bars.
function useVisits(herdId: string | undefined, live: readonly Decision[]): Visit[] {
  const [list, setList] = useState<Decision[]>([]);
  useEffect(() => {
    if (!herdId) return;
    let on = true;
    api.decisions(herdId, 200).then((l) => on && setList(l), () => on && setList([]));
    return () => {
      on = false;
    };
  }, [herdId]);
  return useMemo(() => visitsOf([...list, ...live.filter((d) => d.herd_id === herdId)], Date.now()), [list, live, herdId]);
}

export function visitsOf(decisions: readonly Decision[], now: number): Visit[] {
  const seen = new Set<string>();
  const moves = decisions
    .filter((d) => d.action === "MOVE" && d.to_paddock_id && (d.status === "applied" || d.status === "approved") && !seen.has(d.id) && seen.add(d.id))
    .map((d) => ({ pad: d.to_paddock_id!, at: Date.parse(d.responded_at ?? d.created_at) }))
    .sort((a, b) => a.at - b.at);
  const out: Visit[] = [];
  moves.forEach((m, i) => {
    const to = moves[i + 1]?.at ?? now;
    // A move into the paddock the herd was already in carries the stay on.
    const last = out[out.length - 1];
    if (last && last.pad === m.pad) last.to = to;
    else out.push({ pad: m.pad, from: m.at, to });
  });
  return out.filter((v) => v.to > now - STRIP_MS);
}

// Thirty days as a hairline, the herd's stays in grass, today at the right end.
function Strip({ spans, now }: { spans: Visit[]; now: number }) {
  const start = now - STRIP_MS;
  const at = (t: number) => Math.min(100, Math.max(0, ((t - start) / STRIP_MS) * 100));
  return (
    <span className="pstrip" aria-hidden="true">
      {spans.map((v) => <i key={v.from} style={{ left: `${at(v.from)}%`, width: `${Math.max(1.5, at(v.to) - at(v.from))}%` }} />)}
    </span>
  );
}

const auDays = (v: number) => v.toFixed(v < 10 ? 1 : 0);
const restWords = (d: number) => (d >= 1.5 ? `${Math.round(d)} d` : `${Math.max(1, Math.round(d * 24))} h`);

// The farm's pasture figures, refetched when `key` changes and every few minutes.
function usePasture(key: string): PastureRow[] {
  const [rows, setRows] = useState<PastureRow[]>([]);
  useEffect(() => {
    let live = true;
    let busy = false;
    // One at a time, and not while the app is still loading: on a busy server the analytics read
    // is slow, and the browser's few connections should go to the farm and the view first.
    const load = () => {
      if (busy) return;
      busy = true;
      api.pasture().then((r) => live && setRows(r), () => {}).finally(() => (busy = false));
    };
    const first = setTimeout(load, PASTURE_DELAY_MS);
    const t = setInterval(load, PASTURE_EVERY_MS);
    return () => {
      live = false;
      clearTimeout(first);
      clearInterval(t);
    };
  }, [key]);
  return rows;
}

// The herd everything is about: its name and head count, and the others to switch to.
export function HerdSwitch() {
  const herds = useStore((s) => s.state?.herds ?? []);
  const herdId = useStore((s) => s.herdId);
  const herd = herds.find((h) => h.id === herdId) ?? herds[0];
  if (!herd) return null;
  const face = (
    <span className="hsw">
      <span className="hswn">{herd.name}</span>
      <span className="hswc mono">{herd.count} head</span>
      {herds.length > 1 && <Icon name="chev" size={6} />}
    </span>
  );
  if (herds.length < 2) return <a className="hswrap" href="#/herd">{face}</a>;
  return (
    <div className="hswrap">
      <Menu trigger={face} items={herds.map((h) => ({ label: <><span className="hn">{h.name}</span><span className="mono dim">{h.count}</span></>, current: h.id === herd.id, onSelect: () => store.setHerd(h.id) }))} />
    </div>
  );
}

// Over views that aren't about one herd: the farm's name.
export function FarmHead() {
  const name = useStore((s) => s.state?.farm?.name);
  return (
    <div className="hswrap">
      <span className="hsw"><Mark size={12} /><span className="hswn">{name}</span></span>
    </div>
  );
}

// A paddock's outline, drawn to fit a small square the way it lies on the ground.
export function Shape({ poly }: { poly: Polygon }) {
  const ring = poly.coordinates[0] ?? [];
  if (ring.length < 3) return <span className="pshape" />;
  const lat0 = ring.reduce((s, p) => s + p[1], 0) / ring.length;
  const pts = ring.map((p) => toXY(p as [number, number], lat0));
  const xs = pts.map((p) => p[0]), ys = pts.map((p) => p[1]);
  const [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
  const k = 20 / Math.max(x1 - x0, y1 - y0, 1e-9);
  const ox = (24 - (x1 - x0) * k) / 2, oy = (24 - (y1 - y0) * k) / 2;
  // North up: y grows southward on screen.
  const d = pts.map(([x, y], i) => `${i ? "L" : "M"}${(ox + (x - x0) * k).toFixed(2)} ${(oy + (y1 - y) * k).toFixed(2)}`).join("") + "Z";
  return (
    <svg className="pshape" viewBox="0 0 24 24" aria-hidden="true"><path d={d} /></svg>
  );
}
