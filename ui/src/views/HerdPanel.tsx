import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { api, type Autonomy, type Decision, type Herd, type NewCollar, type Paddock, type Polygon } from "../api";
import { areaHa, centroid, inside } from "../geo";
import { guarded, HERD_PANEL, herdMenu, herdPanel, interleave, sectionNodes, useSections } from "../registry";
import { behindOf, collarLabels, outOf, store, useStore } from "../store";
import { useCan } from "../store/me";
import { summarize } from "../store/live";
import { Button, Copy, Input, Menu, Segmented } from "../ui";
import { useUnits } from "../units";
import { age, clock, useNow } from "../util";

// Clicking Timer again steps through these.
const TIMER_STEPS = [15, 30, 60, 120, 240, 480];
// Above this many collars the panel shows one summary line; the Herd view has every row.
export const LIST_UP_TO = 30;
const mins = (m: number) => (m >= 60 && m % 60 === 0 ? `${m / 60}h` : `${m}m`);

export function HerdPanel({ onChange, changing, onFocusCollar, onFocusCollars, onHoverCollar }: {
  onChange: () => void; changing: boolean; onFocusCollar: (id: string) => void; onFocusCollars: (ids: string[]) => void;
  onHoverCollar: (id?: string) => void;
}) {
  const state = useStore((s) => s.state)!;
  const herdId = useStore((s) => s.herdId);
  const decisions = useStore((s) => s.decisions);
  const bstat = useStore((s) => (s.herdId ? s.boundary[s.herdId] : undefined));
  const logs = useStore((s) => s.logs);
  const animals = useStore((s) => s.animals);
  useStore((s) => s.collars.length);
  const now = useNow(1000);
  const u = useUnits();
  const [busy, setBusy] = useState(false);
  const [reply, setReply] = useState("");
  const [err, setErr] = useState<string>();
  // The herd menu entry open under the name, if any.
  const [openItem, setOpenItem] = useState<string>();
  const menu = herdMenu.use();
  const panelProps = { herdId: herdId ?? "" };
  const sections = useSections(herdPanel, panelProps);
  useEffect(() => setOpenItem(undefined), [herdId]);
  // Controls show only to roles that may use them: hands stop moves and let animals go,
  // managers decide, answer and set up.
  const manage = useCan("manager");
  const tend = useCan("hand");

  // Fix ages step every five seconds, so twelve rows don't tick at once.
  const calm = Math.ceil(now / 5000) * 5000;
  const herd = state.herds.find((h) => h.id === herdId);
  if (!herd) return <aside className="panel" />;
  const all = store.get().collars;
  const names = collarLabels(all, animals);
  const collars = all.filter((c) => c.herd_id === herd.id)
    .map((c) => ({ ...c, label: names.get(c.id) ?? c.name }));
  const byId = new Map(collars.map((c) => [c.id, c] as const));
  const latest = decisions[0];
  const live = latest && (latest.status === "running" || latest.status === "proposed") ? latest : undefined;

  const pad = (id?: string) => state.paddocks.find((p) => p.id === id)?.name;
  const move = bstat?.move;
  const sweeping = move?.status === "sweeping";
  const behind = behindOf(move, collars);
  const out = outOf(bstat);
  const behindSet = new Set([...behind, ...out.map((e) => e.collar_id)]);
  // Where the move goes: the paddock the target mostly covers, if any.
  const moveTo = move && targetPaddock(move.target, state.paddocks);
  // S: a call about a strip schedule names the next strip; N holds today's strip.
  const onSchedule = (d: Decision) => {
    const sc = (d.inputs as { schedule?: { status?: string; next?: { strip: number; of: number; opens: string } } } | undefined)?.schedule;
    return sc?.status === "active" ? sc.next : undefined;
  };
  const sentence = (d: Decision) =>
    d.action === "MOVE" ? `Move to ${pad(d.to_paddock_id) ?? "the new boundary"}.`
      : d.action === "STAY" && onSchedule(d) ? `Strip ${onSchedule(d)!.strip} of ${onSchedule(d)!.of} opens ${onSchedule(d)!.opens}.`
        : d.action === "STAY" ? `Stay in ${pad(herd.paddock_id) ?? "place"}.`
          : d.action === "HOLD" ? "Hold today's strip."
            : d.need ?? "Needs more information.";
  const keeps = !!live && live.action === "STAY" && !!onSchedule(live);

  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setErr(undefined);
    try {
      await f();
      await store.refreshHerd();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const setAutonomy = (autonomy: Autonomy) => act(async () => {
    // Timer again: the next length.
    const patch: Partial<Herd> = autonomy === "timer" && herd.autonomy === "timer"
      ? { timer_minutes: TIMER_STEPS.find((m) => m > herd.timer_minutes) ?? TIMER_STEPS[0] }
      : { autonomy };
    await api.updateHerd(herd.id, patch);
    await store.refresh();
  });

  // The ack count for whatever went out last: pending if one is in flight, else active.
  const b = bstat?.pending ?? bstat?.active;
  const applied = b ? (bstat?.acks ?? []).filter((a) => a.version === b.version && a.status === "applied").length : 0;
  const done = collars.length > 0 && applied >= collars.length;
  const item = menu.find((m) => m.id === openItem);

  const decision = collars.length > 0 && <>
      {manage && <div className="auto">
        <Segmented label="Autonomy" value={herd.autonomy} onChange={setAutonomy} options={[
          { value: "propose", label: "Propose" },
          { value: "timer", label: herd.autonomy === "timer" ? <>Timer <span className="mono">{mins(herd.timer_minutes)}</span></> : "Timer",
            title: herd.autonomy === "timer" ? "Click to change the timer" : undefined },
          { value: "auto", label: "Auto" },
        ]} />
      </div>}

      <section className="dec">
        {sweeping && (
          <>
            <p className="call sweep">
              <span>{moveTo ? `Moving to ${moveTo}` : "Moving"}</span>
              {move.remaining_m >= 1 && <span className="rem mono" title="To the target">{u.len(move.remaining_m)}</span>}
            </p>
            <div className="acts">
              {behind.length > 0 && (
                <button type="button" className="behind mono" onClick={() => onFocusCollars(behind)}>{behind.length} behind</button>
              )}
              {tend && <Button small kind="plain" className="stop" disabled={busy} onClick={() => act(() => api.stopMove(herd.id))}>Stop</Button>}
            </div>
          </>
        )}
        {!sweeping && <>
        {live?.status === "running" && (
          <>
            <p className="call dim">Thinking</p>
            <ul className="log mono">{(logs[live.id] ?? []).map((l, i) => <li key={i}>{l}</li>)}</ul>
          </>
        )}
        {live?.status === "proposed" && live.action === "NEEDS_INFO" && (
          <>
            <p className="call">{sentence(live)}</p>
            {live.reasoning && <Why key={live.id} text={live.reasoning} />}
            {/* The answer goes back as the farmer's note, then the brain decides again with it. */}
            {manage && <form className="reply" onSubmit={(e) => {
              e.preventDefault();
              const note = reply.trim();
              if (!note) return;
              void act(async () => {
                await api.respond(live.id, { action: "approve", note });
                setReply("");
                await api.decide(herd.id);
              });
            }}>
              <Input className="sm" value={reply} onChange={(e) => setReply(e.target.value)} placeholder="Reply" aria-label="Reply" disabled={busy} />
              <div className="acts">
                <Button small kind="plain" disabled={busy} onClick={() => act(() => api.respond(live.id, { action: "reject" }))}>Dismiss</Button>
                <Button small kind="primary" type="submit" disabled={busy || !reply.trim()}>Send</Button>
              </div>
            </form>}
          </>
        )}
        {live?.status === "proposed" && live.action !== "NEEDS_INFO" && (
          <>
            <p className="call">{sentence(live)}</p>
            {live.reasoning && <Why key={live.id} text={live.reasoning} />}
            {live.apply_at && <p className="clock">{clock(Date.parse(live.apply_at) - now)}</p>}
            {manage && <div className="acts">
              <Button small kind="plain" disabled={busy} onClick={() => act(() => api.respond(live.id, { action: "reject" }))}>{keeps ? "Hold" : "Reject"}</Button>
              {live.geometry && <Button small disabled={busy || changing} onClick={onChange}>Change</Button>}
              <Button small kind="primary" disabled={busy} onClick={() => act(() => api.respond(live.id, { action: "approve" }))}>
                {live.apply_at ? "Send now" : keeps ? "Keep" : "Approve"}
              </Button>
            </div>}
          </>
        )}
        {!live && move?.status === "done" ? (
          <p className="call dim">{moveTo ? `Moved to ${moveTo}.` : "Moved."}</p>
        ) : !live && latest?.status === "applied" && latest.source !== "farmer" && latest.action === "MOVE" && (
          <p className="call dim">Moved to {pad(latest.to_paddock_id) ?? "the new boundary"}.</p>
        )}
        {!live && latest?.status === "failed" && latest.error && !err && <p className="why err">{latest.error}</p>}
        {err && <p className="why err">{err}</p>}
        {!live && (manage || behind.length > 0) && (
          <div className="acts">
            {manage && <Button small disabled={busy} onClick={() => act(() => api.decide(herd.id))}>Decide</Button>}
            {behind.length > 0 && (
              <button type="button" className="behind mono end" onClick={() => onFocusCollars(behind)}>{behind.length} behind</button>
            )}
          </div>
        )}
        </>}
      </section>
      </>;

  const escapes = collars.length > 0 && out.length > 0 && (
        <section className="esc" aria-label="Out">
          {out.map((e) => (
            <div key={e.id} className="acts">
              <button type="button" className="behind" onClick={() => onFocusCollar(e.collar_id)}>
                {byId.get(e.collar_id)?.label ?? "Collar"} out
              </button>
              {e.remaining_m >= 1 && <span className="rem mono" title="To the herd's boundary">{u.len(e.remaining_m)}</span>}
              {tend && <Button small kind="plain" className="stop" disabled={busy} onClick={() => act(() => api.stopEscape(e.collar_id))}>Let go</Button>}
            </div>
          ))}
        </section>
      );

  const summary = collars.length > LIST_UP_TO ? summarize(collars) : undefined;
  const rows = summary ? [] : collars.sort((a, b) => a.label.localeCompare(b.label, undefined, { numeric: true }));
  const list = (
      <ul className="collars" onMouseLeave={() => onHoverCollar(undefined)}>
        {b && collars.length > 0 && (
          <li className="acks mono" title={`Boundary v${b.version}: ${applied} of ${collars.length} collars applied`}>
            <span>v{b.version}</span><b className={done ? "ok" : undefined}>{applied}/{collars.length}</b>
          </li>
        )}
        {summary && (
          <li className="sum mono">
            <span>{summary.total} collars</span>
            {summary.outside.length > 0 && <button type="button" title="Outside the boundary" onClick={() => onFocusCollars(summary.outside)}>{summary.outside.length} out</button>}
            {summary.warning.length > 0 && <button type="button" title="In the warning band" onClick={() => onFocusCollars(summary.warning)}>{summary.warning.length} near</button>}
            {summary.low.length > 0 && <button type="button" title="Battery under 20%" onClick={() => onFocusCollars(summary.low)}>{summary.low.length} low</button>}
          </li>
        )}
        {rows.map((c) => (
          <li key={c.id} data-state={c.state} data-behind={behindSet.has(c.id) || undefined}>
            <button type="button" onClick={() => onFocusCollar(c.id)} onMouseEnter={() => onHoverCollar(c.id)}
              onFocus={() => onHoverCollar(c.id)} onBlur={() => onHoverCollar(undefined)}>
              <span className="cn">{c.label}</span>
              <span className={c.battery !== undefined && c.battery < 0.2 ? "low" : undefined}>{c.battery !== undefined ? `${Math.round(c.battery * 100)}%` : "–"}</span>
              <span>{age(c.last_seen, calm)}</span>
            </button>
          </li>
        ))}
      </ul>
  );

  return (
    <aside className="panel" aria-label="Herd">
      {(state.herds.length > 1 || menu.length > 0) && (
        <div className="ph">
          <Menu trigger={<span className="hname">{herd.name}</span>}
            items={[
              ...(state.herds.length > 1 ? state.herds.map((h) => ({ label: h.name, current: h.id === herd.id, onSelect: () => store.setHerd(h.id) })) : []),
              ...menu.map((m) => ({ label: m.label, current: m.id === openItem, onSelect: () => setOpenItem(m.id === openItem ? undefined : m.id) })),
            ]} />
        </div>
      )}
      {item && <div className="hitem">{guarded(item.id, <item.Item herdId={herd.id} />)}</div>}
      {interleave([
        { key: "decision", order: HERD_PANEL.decision, node: decision },
        { key: "escapes", order: HERD_PANEL.escapes, node: escapes },
        { key: "collars", order: HERD_PANEL.collars, node: list },
        { key: "addCollar", order: HERD_PANEL.addCollar, node: manage && <AddCollar herdId={herd.id} /> },
      ], sectionNodes(sections, panelProps))}
    </aside>
  );
}

function targetPaddock(target: Polygon, paddocks: Paddock[]) {
  const c = centroid(target);
  const p = paddocks.find((p) => inside(c, p.geometry));
  return p && areaHa(target) >= areaHa(p.geometry) / 2 ? p.name : undefined;
}

// The reasoning, two lines until clicked.
function Why({ text }: { text: string }) {
  const ref = useRef<HTMLParagraphElement>(null);
  const [open, setOpen] = useState(false);
  const [more, setMore] = useState(false);
  useLayoutEffect(() => {
    const el = ref.current;
    if (el && !open) setMore(el.scrollHeight > el.clientHeight + 1);
  }, [text, open]);
  const toggle = () => more && setOpen(!open);
  return (
    <p ref={ref} className={"why" + (open ? " open" : "") + (more ? " more" : "")} onClick={toggle}
      role={more ? "button" : undefined} tabIndex={more ? 0 : undefined} aria-expanded={more ? open : undefined}
      onKeyDown={(e) => (e.key === "Enter" || e.key === " ") && (e.preventDefault(), toggle())}>
      {text}
    </p>
  );
}

// Name a collar, then show what the device needs: endpoint, its key (only now), our public key.
function AddCollar({ herdId }: { herdId: string }) {
  const [step, setStep] = useState<"idle" | "name">("idle");
  const [name, setName] = useState("");
  const [made, setMade] = useState<NewCollar>();
  const [err, setErr] = useState<string>();
  const create = async () => {
    setErr(undefined);
    try {
      setMade(await api.createCollar({ herd_id: herdId, name: name.trim() || undefined }));
      setStep("idle");
      setName("");
      await store.refreshHerd();
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  if (made)
    return (
      <div className="addcollar">
        <Copy label="endpoint" value={made.endpoint} />
        <Copy label="key" value={made.key} />
        <Copy label="public key" value={made.public_key} />
        <Button small kind="plain" onClick={() => setMade(undefined)}>Done</Button>
      </div>
    );
  if (step === "name")
    return (
      <form className="addcollar row" onSubmit={(e) => { e.preventDefault(); void create(); }}>
        <Input autoFocus className="sm" placeholder="Name" aria-label="Collar name" value={name} onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === "Escape" && setStep("idle")} />
        <Button small kind="primary" type="submit">Add</Button>
        {err && <span className="mono err">{err}</span>}
      </form>
    );
  return (
    <div className="addcollar">
      <Button small onClick={() => setStep("name")}>Add collar</Button>
    </div>
  );
}
