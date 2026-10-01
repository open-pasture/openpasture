// The top of the desktop herd panel: openpasture at work. The pilot says what it is doing now,
// how the herd's collars are, what it will do next and how far it may go on its own. Under it,
// only what it needs from the farmer, each ask a question with the answer it expects first; and
// what the farmer said they'd see to, watched quietly until it clears.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { api, type Alert, type Autonomy, type Collar, type Decision, type Herd } from "../api";
import { alertsApi } from "../api/a-engine";
import { focus as focusAlert } from "../features/a-engine/focus";
import { ageText, collarsOf, since } from "../features/a-engine/model";
import { behindOf, collarLabels, outOf, store, useStore } from "../store";
import { alerts, applyAlert } from "../store/a-engine";
import { undrawn } from "../store/live";
import { useCan } from "../store/me";
import { createSlice } from "../store/slice";
import { Button, Icon, Input, Mark, Menu } from "../ui";
import { useUnits } from "../units";
import { clock, useNow } from "../util";
import { targetPaddock, Why } from "../views/HerdPanel";
import { alertWords, asksOf, decisionAnswers, decisionAsk, inWords, minutesToCall, scheduleNext, watched, type Ask } from "./asks";
import { Shape } from "./Farm";

// A collar that hasn't reported in this long reads as quiet in the herd's bar.
const QUIET_MS = 60 * 60_000;
// Clicking Timer steps through these (as the phone's panel).
const TIMER_STEPS = [15, 30, 60, 120, 240, 480];
const mins = (m: number) => (m >= 60 && m % 60 === 0 ? `${m / 60} h` : `${m} m`);

// The rail's Asks button pings this to bring the asks into view.
export const asksPing = createSlice(0);

export interface DeskProps {
  herdId: string;
  changing: boolean;
  onChange: () => void;
  onFocusCollar: (id: string) => void;
  onFocusCollars: (ids: string[]) => void;
}

export function Desk(p: DeskProps) {
  return (
    <>
      <Pilot {...p} />
      <Asks {...p} />
      <Watching herdId={p.herdId} />
    </>
  );
}

// ---- the pilot ---------------------------------------------------------------------------

function Pilot({ herdId, onFocusCollar, onFocusCollars }: DeskProps) {
  const state = useStore((s) => s.state)!;
  const decisions = useStore((s) => s.decisions);
  const bstat = useStore((s) => s.boundary[herdId]);
  const logs = useStore((s) => s.logs);
  const all = useStore((s) => s.collars);
  const animals = useStore((s) => s.animals);
  const list = alerts.use((s) => s.list);
  const now = useNow(1000);
  const u = useUnits();
  const tend = useCan("hand");
  const [busy, setBusy] = useState(false);
  const herd = state.herds.find((h) => h.id === herdId);
  if (!herd) return null;

  const off = undrawn(all, animals);
  const collars = all.filter((c) => c.herd_id === herdId && !off.has(c.id));
  const names = collarLabels(all, animals);
  const pad = (id?: string) => state.paddocks.find((x) => x.id === id)?.name;
  const move = bstat?.move;
  const sweeping = move?.status === "sweeping";
  const behind = behindOf(move, collars);
  const out = outOf(bstat);
  const running = decisions.find((d) => d.status === "running");
  const silent = list.some((a) => a.kind === "herd_silent" && a.herd_id === herdId);
  const moveTo = move && targetPaddock(move.target, state.paddocks);
  const day = daysIn(decisions, herd, now);

  // One sentence for what openpasture is doing with the herd now.
  let line: ReactNode;
  let tone: "live" | "warn" | "busy" = "live";
  if (silent) {
    line = <>Can't hear {herd.name}'s collars</>;
    tone = "warn";
  } else if (sweeping) {
    line = <>Moving {herd.name} to {moveTo ?? "the new boundary"}{move.remaining_m >= 1 && <span className="prem mono">{u.len(move.remaining_m)}</span>}</>;
    tone = "busy";
  } else if (running) {
    line = <>Deciding where {herd.name} goes next</>;
    tone = "busy";
  } else {
    line = <>Holding {herd.name} in {pad(herd.paddock_id) ?? "its boundary"}{day !== undefined && <span className="prem mono">day {day}</span>}</>;
  }

  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    try {
      await f();
      await store.refreshHerd();
    } finally {
      setBusy(false);
    }
  };
  const log = running ? (logs[running.id] ?? []).slice(-3) : [];

  return (
    <section className="pilot" data-tone={tone} aria-label="openpasture">
      <p className="pline"><i className="pdot" aria-hidden="true" /><span>{line}</span></p>
      <Vitals collars={collars} now={now} onFocus={onFocusCollars} />
      {log.length > 0 && <ul className="plog mono">{log.map((l, i) => <li key={i}>{l}</li>)}</ul>}
      {(sweeping && behind.length > 0) || out.length > 0 ? (
        <ul className="pjobs">
          {sweeping && behind.length > 0 && (
            <li>
              <button type="button" className="pjob" onClick={() => onFocusCollars(behind)}>{behind.length} behind the sweep</button>
              {tend && <Button small kind="plain" disabled={busy} onClick={() => act(() => api.stopMove(herdId))}>Stop the move</Button>}
            </li>
          )}
          {out.map((e) => (
            <li key={e.id}>
              <button type="button" className="pjob" onClick={() => onFocusCollar(e.collar_id)}>
                Bringing {names.get(e.collar_id) ?? "a collar"} back{e.remaining_m >= 1 && <span className="mono"> {u.len(e.remaining_m)}</span>}
              </button>
              {tend && <Button small kind="plain" disabled={busy} onClick={() => act(() => api.stopEscape(e.collar_id))}>Let go</Button>}
            </li>
          ))}
        </ul>
      ) : null}
      {sweeping && !behind.length && tend && (
        <div className="pjobs solo"><Button small kind="plain" disabled={busy} onClick={() => act(() => api.stopMove(herdId))}>Stop the move</Button></div>
      )}
      <Rules herd={herd} now={now} />
    </section>
  );
}

// Days the herd has been where it is, from the move that took it there.
function daysIn(decisions: readonly Decision[], herd: Herd, now: number): number | undefined {
  const d = decisions.find((x) => x.action === "MOVE" && (x.status === "applied" || x.status === "approved") && x.to_paddock_id === herd.paddock_id);
  if (!d) return undefined;
  return Math.max(1, Math.ceil((now - Date.parse(d.responded_at ?? d.created_at)) / 86400e3));
}

// The herd's collars as one bar: inside, near the edge, outside, and quiet (not heard from).
function Vitals({ collars, now, onFocus }: { collars: Collar[]; now: number; onFocus: (ids: string[]) => void }) {
  if (!collars.length) return null;
  const quiet = collars.filter((c) => !c.last_seen || now - Date.parse(c.last_seen) > QUIET_MS);
  const heard = collars.filter((c) => !quiet.includes(c));
  const groups = [
    { k: "in", ids: heard.filter((c) => c.state === "inside").map((c) => c.id), word: "in" },
    { k: "near", ids: heard.filter((c) => c.state === "warning").map((c) => c.id), word: "near" },
    { k: "out", ids: heard.filter((c) => c.state === "outside").map((c) => c.id), word: "out" },
    { k: "quiet", ids: [...quiet, ...heard.filter((c) => c.state === "unknown")].map((c) => c.id), word: "quiet" },
  ];
  return (
    <div className="vitals">
      <div className="vbar" aria-hidden="true">
        {groups.map((g) => g.ids.length > 0 && <i key={g.k} data-k={g.k} style={{ flexGrow: g.ids.length }} />)}
      </div>
      <p className="vkey mono">
        {groups.map((g) => g.ids.length > 0 && (
          <button key={g.k} type="button" data-k={g.k} disabled={g.k === "in"} onClick={() => onFocus(g.ids)}>{g.ids.length} {g.word}</button>
        ))}
      </p>
    </div>
  );
}

// How far openpasture may go on its own, and when it next decides.
function Rules({ herd, now }: { herd: Herd; now: number }) {
  const settings = useStore((s) => s.state?.settings);
  const tz = useStore((s) => s.state?.farm?.timezone);
  const manage = useCan("manager");
  const left = settings?.decision_time ? minutesToCall(settings.decision_time, tz, now) : undefined;
  const words: Record<Autonomy, string> = {
    propose: "Asks before it moves",
    timer: `Moves ${mins(herd.timer_minutes)} after asking`,
    auto: "Moves on its own",
  };
  const set = (patch: Partial<Herd>) => void api.updateHerd(herd.id, patch).then(() => store.refresh());
  return (
    <div className="prules mono">
      {manage ? (
        <Menu trigger={<span className="pmode">{words[herd.autonomy]}<Icon name="chev" size={5} /></span>} items={[
          { label: "Ask before moving", current: herd.autonomy === "propose", onSelect: () => set({ autonomy: "propose" }) },
          ...TIMER_STEPS.map((m) => ({ label: `Move ${mins(m)} after asking`, current: herd.autonomy === "timer" && herd.timer_minutes === m, onSelect: () => set({ autonomy: "timer", timer_minutes: m }) })),
          { label: "Move on its own", current: herd.autonomy === "auto", onSelect: () => set({ autonomy: "auto" }) },
        ]} />
      ) : <span className="pmode">{words[herd.autonomy]}</span>}
      {left !== undefined && <span>next call {settings!.decision_time.slice(0, 5)}, {inWords(left)}</span>}
    </div>
  );
}

// ---- asks --------------------------------------------------------------------------------

function Asks(p: DeskProps) {
  const decisions = useStore((s) => s.decisions);
  const list = alerts.use((s) => s.list);
  const asks = asksOf(decisions, list, p.herdId);
  const [open, setOpen] = useState<string>();
  // What was just answered, said back for a moment.
  const [said, setSaid] = useState<string>();
  useEffect(() => {
    if (!said) return;
    const t = setTimeout(() => setSaid(undefined), 3200);
    return () => clearTimeout(t);
  }, [said]);
  // The rail's Asks button brings them into view.
  const ping = asksPing.use((v) => v);
  const ref = useRef<HTMLElement>(null);
  useEffect(() => {
    if (!ping || !ref.current) return;
    ref.current.scrollIntoView({ block: "nearest", behavior: "smooth" });
    ref.current.classList.remove("pinged");
    void ref.current.offsetWidth;
    ref.current.classList.add("pinged");
  }, [ping]);

  const shown = asks.find((a) => a.id === open) ?? asks[0];
  return (
    <section className="asks" ref={ref} aria-label="Needs you" data-none={!asks.length || undefined}>
      {said && <p className="said">{said}</p>}
      {!asks.length && !said && <p className="calm"><Icon name="check" size={9} accent="currentColor" />Nothing needs you</p>}
      {shown && <AskView key={shown.id} ask={shown} {...p} onSaid={setSaid} />}
      {asks.length > 1 && (
        <ul className="amore">
          {asks.filter((a) => a !== shown).map((a) => (
            <li key={a.id}><button type="button" onClick={() => setOpen(a.id)} data-rank={a.rank >= 4 ? "high" : undefined}><i />{headline(a)}</button></li>
          ))}
        </ul>
      )}
    </section>
  );
}

function headline(a: Ask): string {
  if (a.k === "alert") return a.a.title;
  const s = store.get().state;
  const herd = s?.herds.find((h) => h.id === a.herdId);
  return decisionAsk(a.d, herd?.name ?? "the herd", (id) => s?.paddocks.find((p) => p.id === id)?.name, herd?.paddock_id);
}

function AskView(p: DeskProps & { ask: Ask; onSaid: (s: string) => void }) {
  return p.ask.k === "decision" ? <DecisionAsk {...p} d={p.ask.d} /> : <AlertAsk a={p.ask.a} herdId={p.herdId} onSaid={p.onSaid} />;
}

function DecisionAsk({ d, changing, onChange, onSaid }: DeskProps & { d: Decision; onSaid: (s: string) => void }) {
  const state = useStore((s) => s.state)!;
  const manage = useCan("manager");
  const now = useNow(1000);
  const [busy, setBusy] = useState(false);
  const [reply, setReply] = useState("");
  const [err, setErr] = useState<string>();
  const herd = state.herds.find((h) => h.id === d.herd_id);
  const pad = (id?: string) => state.paddocks.find((x) => x.id === id);
  const q = decisionAsk(d, herd?.name ?? "the herd", (id) => pad(id)?.name, herd?.paddock_id);
  const { yes, no } = decisionAnswers(d);
  const from = pad(herd?.paddock_id);
  const to = d.action === "MOVE" ? pad(d.to_paddock_id) : undefined;
  const sched = scheduleNext(d);

  const answer = async (f: () => Promise<unknown>, said: string) => {
    setBusy(true);
    setErr(undefined);
    try {
      await f();
      onSaid(said);
      await store.refreshHerd();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const approve = () => answer(() => api.respond(d.id, { action: "approve" }),
    d.action === "MOVE" ? `Moving ${herd?.name} to ${to?.name ?? "the new boundary"}.` : d.action === "STAY" ? `${herd?.name} stays in ${from?.name ?? "place"}.` : "Done.");
  const reject = () => answer(() => api.respond(d.id, { action: "reject" }), d.action === "NEEDS_INFO" ? "Skipped. It'll decide without that." : "Holding. Nothing changes.");
  const send = () => {
    const note = reply.trim();
    if (!note) return;
    void answer(async () => {
      await api.respond(d.id, { action: "approve", note });
      setReply("");
      await api.decide(d.herd_id);
    }, "Thanks. Deciding again with that.");
  };

  // A timed call goes ahead by itself: how long it has left, as a line running down.
  const total = d.apply_at ? Date.parse(d.apply_at) - Date.parse(d.created_at) : 0;
  const left = d.apply_at ? Math.max(0, Date.parse(d.apply_at) - now) : 0;

  return (
    <article className="ask" data-kind={d.action === "NEEDS_INFO" ? "question" : "decision"}>
      <h3><Mark size={12} />{q}</h3>
      {(to || (d.action === "STAY" && from)) && (
        <div className="aroute" aria-hidden="true">
          {from && <span className="arp"><Shape poly={from.geometry} /><span className="mono">{from.name}</span></span>}
          {to && <><span className="arrow" /><span className="arp to"><Shape poly={(d.geometry ?? to.geometry)} /><span className="mono">{to.name}</span></span></>}
          {sched && <span className="mono dim">strip {sched.strip}/{sched.of}</span>}
        </div>
      )}
      {d.reasoning && <Why key={d.id} text={d.reasoning} />}
      {d.apply_at && (
        <div className="atimer">
          <p className="mono">{d.action === "MOVE" ? "Moves" : "Goes ahead"} in {clock(left)} unless you hold it</p>
          <span className="adrain"><i style={{ width: `${total > 0 ? (left / total) * 100 : 0}%` }} /></span>
        </div>
      )}
      {err && <p className="aerr">{err}</p>}
      {manage && d.action === "NEEDS_INFO" ? (
        <form className="areply" onSubmit={(e) => { e.preventDefault(); send(); }}>
          <Input value={reply} onChange={(e) => setReply(e.target.value)} placeholder="Tell openpasture" aria-label="Reply" disabled={busy} />
          <div className="answers">
            <Button small kind="primary" type="submit" disabled={busy || !reply.trim()}>Send</Button>
            <Button small kind="plain" disabled={busy} onClick={reject}>{no}</Button>
          </div>
        </form>
      ) : manage ? (
        <div className="answers">
          <Button small kind="primary" disabled={busy} onClick={approve}>{yes}</Button>
          {d.geometry && <Button small disabled={busy || changing} onClick={onChange}>Change</Button>}
          <Button small kind="plain" disabled={busy} onClick={reject}>{no}</Button>
        </div>
      ) : (
        <p className="aerr dim">A manager answers this one.</p>
      )}
    </article>
  );
}

function AlertAsk({ a, herdId, onSaid }: { a: Alert; herdId: string; onSaid: (s: string) => void }) {
  const hand = useCan("hand");
  const herds = useStore((s) => s.state?.herds ?? []);
  const all = useStore((s) => s.collars);
  const animals = useStore((s) => s.animals);
  const now = useNow(15_000);
  const [busy, setBusy] = useState(false);
  const w = alertWords(a.kind);
  const other = a.herd_id && a.herd_id !== herdId ? herds.find((h) => h.id === a.herd_id) : undefined;
  const names = collarLabels(all, animals);
  const who = collarsOf(a).map((id) => names.get(id) ?? "").filter(Boolean);
  const act = async (f: (id: string) => Promise<Alert>, said: string) => {
    setBusy(true);
    try {
      applyAlert(await f(a.id));
      onSaid(said);
    } catch {
      /* the live event or the next load shows where it stands */
    } finally {
      setBusy(false);
    }
  };

  // Another herd's waiting call: answered in that herd.
  if (a.kind === "decision_waiting") {
    return (
      <article className="ask" data-kind="decision">
        <h3><Mark size={12} />{a.title}</h3>
        <div className="answers"><Button small kind="primary" onClick={() => a.herd_id && store.setHerd(a.herd_id)}>Open {other?.name ?? "that herd"}</Button></div>
      </article>
    );
  }
  return (
    <article className="ask" data-kind="alert" data-sev={a.severity}>
      <h3><Mark size={12} />{other && <span className="aherd">{other.name}</span>}{a.title}</h3>
      <p className="aask">{w.ask || a.body}<span className="mono"> {ageText(since(a), now)}</span></p>
      {who.length > 0 && (
        <button type="button" className="awho mono" title="Show on the map" onClick={() => focusAlert(a)}>
          {who.slice(0, 8).join("  ")}{who.length > 8 && `  +${who.length - 8}`}
        </button>
      )}
      {hand ? (
        <div className="answers">
          <Button small kind="primary" disabled={busy} onClick={() => act(alertsApi.ack, "Noted. openpasture will keep watching it.")}>{w.take}</Button>
          <Button small kind="plain" disabled={busy} onClick={() => act(alertsApi.resolve, "Resolved.")}>{w.done}</Button>
          {w.show && who.length > 0 && <Button small kind="plain" onClick={() => focusAlert(a)}>{w.show}</Button>}
        </div>
      ) : null}
    </article>
  );
}

// ---- watching ----------------------------------------------------------------------------

function Watching({ herdId }: { herdId: string }) {
  const list = alerts.use((s) => s.list);
  const hand = useCan("hand");
  const now = useNow(15_000);
  const rows = watched(list, herdId);
  if (!rows.length) return null;
  return (
    <ul className="watch" aria-label="Watching">
      {rows.map((a) => (
        <li key={a.id} data-sev={a.severity}>
          <button type="button" className="wt" onClick={() => focusAlert(a)} title={a.acked_by?.name ? `${a.acked_by.name} is on it` : "On it"}>
            <i aria-hidden="true" />{a.title}
          </button>
          <span className="mono">{ageText(since(a), now)}</span>
          {hand && <button type="button" className="wr mono" onClick={() => void alertsApi.resolve(a.id).then(applyAlert, () => {})}>resolve</button>}
        </li>
      ))}
    </ul>
  );
}
