// The herd panel's schedule block: "Next move 07:00" with a countdown and how many collars
// store the next strip; clicked, the queue by day with Skip, Hold, Move now and Edit.

import { useEffect, useState } from "react";
import { sApi, type Schedule } from "../../api/s";
import { store, useStore } from "../../store";
import { useCan } from "../../store/me";
import { loadSchedule, sState } from "../../store/s";
import { Button } from "../../ui";
import { useNow } from "../../util";
import { atLabel, clockIn, dateIn, zonedToUtc } from "../b/when";
import { countdown, nextMove, nextOpen, queue, rowTime, storedLine, type QueueRow } from "./model";

export function SchedulePanel({ herdId }: { herdId: string }) {
  const hs = sState.use((s) => s.byHerd[herdId]);
  const tz = useStore((s) => s.state?.farm?.timezone) ?? "UTC";
  const slots = useStore((s) => s.boundary[herdId]?.slots);
  const now = useNow(1000);
  const manage = useCan("manager");
  const [open, setOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  useEffect(() => void loadSchedule(herdId), [herdId]);
  useEffect(() => setOpen(false), [herdId]);
  const s = hs?.schedule;
  if (!s) return null;

  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setErr(undefined);
    try {
      await f();
      await loadSchedule(herdId);
      await store.refreshHerd();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const of = s.strips.length;
  const next = nextMove(hs.moves);
  const nextStrip = nextOpen(hs.moves);
  if (s.status === "paused")
    return (
      <section className="sched" aria-label="Schedule">
        <p className="snext"><span>Paused</span>{nextStrip && <span className="mono dim">strip {nextStrip.index + 1} of {of}</span>}</p>
        {manage && (
          <div className="acts">
            <Button small kind="plain" disabled={busy} onClick={() => act(() => sApi.end(s.id))}>End</Button>
            <Button small disabled={busy} onClick={() => act(() => sApi.resume(s.id))}>Resume</Button>
          </div>
        )}
        {err && <p className="serr">{err}</p>}
      </section>
    );
  if (!next) return null;
  const stored = storedLine(nextStrip, slots, tz, now);
  return (
    <section className="sched" aria-label="Schedule">
      <button type="button" className="snext" aria-expanded={open} onClick={() => setOpen(!open)}>
        <span>Next move</span>
        <span className="mono">{atLabel(Date.parse(next.at), tz, now)}</span>
        <span className="mono dim cd">{countdown(Date.parse(next.at) - now)}</span>
      </button>
      {stored && <p className="sstored mono">{stored}</p>}
      {open && <Queue s={s} tz={tz} now={now} manage={manage} busy={busy} act={act} />}
      {err && <p className="serr">{err}</p>}
    </section>
  );
}

function Queue({ s, tz, now, manage, busy, act }: {
  s: Schedule; tz: string; now: number; manage: boolean; busy: boolean; act: (f: () => Promise<unknown>) => Promise<void>;
}) {
  const moves = sState.use((x) => x.byHerd[s.herd_id]?.moves) ?? [];
  const days = queue(moves, tz, now);
  return (
    <div className="squeue">
      {days.map((d) => (
        <div key={d.date} className="sqday">
          <p className="mono dim">{d.day}</p>
          <ul>{d.rows.map((r) => <Row key={`${r.kind}:${r.index}:${r.at}`} r={r} s={s} tz={tz} manage={manage} busy={busy} act={act} />)}</ul>
        </div>
      ))}
      {manage && (
        <div className="acts">
          <Button small kind="plain" disabled={busy} onClick={() => act(() => sApi.end(s.id))}>End</Button>
          <Button small kind="plain" disabled={busy} onClick={() => act(() => sApi.pause(s.id))}>Pause</Button>
        </div>
      )}
    </div>
  );
}

function Row({ r, s, tz, manage, busy, act }: {
  r: QueueRow; s: Schedule; tz: string; manage: boolean; busy: boolean; act: (f: () => Promise<unknown>) => Promise<void>;
}) {
  const [edit, setEdit] = useState(false);
  const [text, setText] = useState(() => clockIn(Date.parse(r.at), tz));
  const what = r.kind === "open" ? `strip ${r.index + 1}` : r.kind === "fence" ? "back fence" : r.kind === "held" ? "held" : `strip ${r.index + 1} skipped`;
  const commit = () => {
    setEdit(false);
    const was = clockIn(Date.parse(r.at), tz);
    if (!/^\d{2}:\d{2}$/.test(text) || text === was) return setText(was);
    const at = new Date(zonedToUtc(dateIn(Date.parse(r.at), tz), text, tz)).toISOString();
    void act(() => sApi.setTime(s.id, r.index, at));
  };
  return (
    <li className="sqrow" data-kind={r.kind} data-first={r.first || undefined}>
      {edit ? (
        <input className="input sm mono" type="time" autoFocus value={text} aria-label={`Open time of strip ${r.index + 1}`}
          onChange={(e) => setText(e.target.value)} onBlur={commit}
          onKeyDown={(e) => {
            if (e.key === "Enter") commit();
            if (e.key === "Escape") { e.stopPropagation(); setEdit(false); setText(clockIn(Date.parse(r.at), tz)); }
          }} />
      ) : (
        <span className="mono st">{rowTime(r, tz)}</span>
      )}
      <span className="sw">{what}</span>
      {manage && r.kind === "open" && !edit && (
        <span className="sacts">
          <Button small kind="plain" disabled={busy} onClick={() => act(() => sApi.skip(s.id, r.index))}>Skip</Button>
          {r.first && <Button small kind="plain" disabled={busy} onClick={() => act(() => sApi.hold(s.id))}>Hold</Button>}
          {r.first && <Button small kind="plain" disabled={busy} onClick={() => act(() => sApi.moveNow(s.id))}>Move now</Button>}
          <Button small kind="plain" disabled={busy} onClick={() => setEdit(true)}>Edit</Button>
        </span>
      )}
    </li>
  );
}
