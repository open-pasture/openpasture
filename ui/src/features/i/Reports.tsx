// Data > Reports: one sentence row per report with its dates, Print and CSV. The NRCS row
// opens the operator and FSA farm lines; the organic row opens herd weights and mixes, AU
// factors and the feed log.

import { useEffect, useState, type ComponentType } from "react";
import { reportsApi, type FeedEntry, type HerdReport, type Lease, type ReportInfo, type ReportInputs, type ReportQuery } from "../../api/i";
import { useStore } from "../../store";
import { useCan } from "../../store/me";
import { NumberField } from "../../ui/NumberField";
import { Check } from "../../ui/Check";
import { Button } from "../../ui/Button";
import { Input } from "../../ui/Input";
import { num, useUnits } from "../../units";
import { defaultRange, localToday, printHash, validRange } from "./format";

export default function Reports() {
  const [list, setList] = useState<ReportInfo[] | null>(null);
  const [leases, setLeases] = useState<Lease[]>([]);
  const tz = useStore((s) => s.state?.farm?.timezone);
  useEffect(() => {
    reportsApi.list().then(setList, () => setList(null));
    reportsApi.leases().then(setLeases, () => setLeases([]));
  }, []);
  if (!list?.length) return null;
  const today = localToday(tz);
  // The lease report needs a lease; paddock sheets add them.
  const shown = list.filter((r) => r.id !== "lease_head_days" || leases.length > 0);
  return (
    <div className="reports">
      {shown.map((r) => <ReportRow key={r.id} info={r} today={today} More={MORE[r.id]} />)}
    </div>
  );
}

const MORE: Record<string, ComponentType> = { nrcs_528: NrcsInputs, organic_season: OrganicInputs };

function ReportRow({ info, today, More }: { info: ReportInfo; today: string; More?: ComponentType }) {
  const [q, setQ] = useState<ReportQuery>(() => defaultRange(today));
  const [open, setOpen] = useState(false);
  const [err, setErr] = useState<string>();
  const ok = validRange(q);
  const csv = async () => {
    setErr(undefined);
    try {
      await reportsApi.csv(info.id, q);
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <div className="rrow">
      <div className="rline">
        {More
          ? <button type="button" className="rname more" aria-expanded={open} onClick={() => setOpen(!open)}>{info.title}</button>
          : <span className="rname">{info.title}</span>}
        <span className="rdates">
          <Input type="date" mono className="sm" value={q.from} max={q.to} aria-label="From" onChange={(e) => setQ({ ...q, from: e.target.value })} />
          <span className="dim">–</span>
          <Input type="date" mono className="sm" value={q.to} min={q.from} aria-label="To" onChange={(e) => setQ({ ...q, to: e.target.value })} />
        </span>
        <span className="racts">
          {ok
            ? <a className="btn quiet sm" href={printHash(info.id, q)} target="_blank" rel="noopener">Print</a>
            : <Button small disabled>Print</Button>}
          <Button small kind="plain" disabled={!ok} onClick={csv}>CSV</Button>
        </span>
        {err && <span className="mono err">{err}</span>}
      </div>
      {open && More && <div className="rmore"><More /></div>}
    </div>
  );
}

// ---- report settings -----------------------------------------------------------------

function useInputs(): [ReportInputs | undefined, (p: Record<string, unknown>) => Promise<void>, string | undefined] {
  const [inputs, setInputs] = useState<ReportInputs>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    reportsApi.inputs().then(setInputs, (e) => setErr((e as Error).message));
  }, []);
  const save = async (p: Record<string, unknown>) => {
    setErr(undefined);
    try {
      setInputs(await reportsApi.updateInputs(p));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return [inputs, save, err];
}

// A text input that saves on blur or Enter.
function TextField({ value, onSave, label, placeholder, mono, disabled, width }: {
  value: string; onSave: (v: string) => void; label: string; placeholder?: string; mono?: boolean; disabled?: boolean; width?: number;
}) {
  const [v, setV] = useState(value);
  useEffect(() => setV(value), [value]);
  const commit = () => {
    if (v.trim() !== value) onSave(v.trim());
  };
  return (
    <Input className="sm" mono={mono} value={v} disabled={disabled} aria-label={label} placeholder={placeholder} style={width ? { width } : undefined}
      onChange={(e) => setV(e.target.value)} onBlur={commit}
      onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); commit(); } if (e.key === "Escape") { e.stopPropagation(); setV(value); } }} />
  );
}

// A small whole or decimal number (counts, percent, factors) with the words after it.
function Small({ value, onSave, label, after, step = 1, min = 0, max, disabled, places }: {
  value: number | undefined; onSave: (v: number) => void; label: string; after?: string; step?: number; min?: number; max?: number; disabled?: boolean;
  places?: number; // fixed places to show, e.g. 2 for AU factors
}) {
  const shown = value === undefined ? "" : places === undefined ? String(value) : value.toFixed(places);
  const [v, setV] = useState(shown);
  useEffect(() => setV(shown), [shown]);
  const commit = () => {
    const n = Number(v);
    if (v.trim() === "" || !Number.isFinite(n) || n < min || (max !== undefined && n > max)) return setV(shown);
    if (n !== value) onSave(step === 1 ? Math.round(n) : n);
  };
  return (
    <span className="numf">
      <input className="input sm mono" inputMode="decimal" aria-label={label} value={v} disabled={disabled} style={{ width: "calc(4ch + 26px)" }}
        onChange={(e) => setV(e.target.value)} onBlur={commit}
        onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); commit(); } if (e.key === "Escape") { e.stopPropagation(); setV(shown); } }} />
      {after && <span className="mono dim">{after}</span>}
    </span>
  );
}

function NrcsInputs() {
  const [inputs, save, err] = useInputs();
  const can = useCan("manager");
  if (!inputs) return err ? <span className="mono err">{err}</span> : null;
  // Below manager: what is set, as facts.
  if (!can)
    return (inputs.operator || inputs.fsa_farm) ? (
      <div className="rline wrap">
        {inputs.operator && <span className="rfield"><span className="dim">Operator</span><span>{inputs.operator}</span></span>}
        {inputs.fsa_farm && <span className="rfield"><span className="dim">FSA farm</span><span className="mono">{inputs.fsa_farm}</span></span>}
      </div>
    ) : null;
  return (
    <div className="rline wrap">
      <label className="rfield"><span className="dim">Operator</span>
        <TextField value={inputs.operator ?? ""} label="Operator" disabled={!can} width={220} onSave={(v) => save({ operator: v || null })} />
      </label>
      <label className="rfield"><span className="dim">FSA farm</span>
        <TextField value={inputs.fsa_farm ?? ""} label="FSA farm" mono disabled={!can} width={120} onSave={(v) => save({ fsa_farm: v || null })} />
      </label>
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}

function OrganicInputs() {
  const herds = useStore((s) => s.state?.herds ?? []);
  const [inputs, save, err] = useInputs();
  const can = useCan("manager");
  const u = useUnits();
  if (!inputs) return err ? <span className="mono err">{err}</span> : null;
  const herdSave = (id: string, p: Partial<HerdReport>) => save({ herds: { [id]: p } });
  const mixed = herds.some((h) => h.species === "cattle" && inputs.herds[h.id]?.mix);
  const au = inputs.au;
  // Below manager: each herd's weight, intake and mix as facts (only what is set), then the feed log.
  if (!can)
    return (
      <>
        {herds.map((h) => {
          const r = inputs.herds[h.id] ?? { intake_pct: 2.5 };
          const m = h.species === "cattle" ? r.mix : undefined;
          const words = [
            r.mean_weight_kg !== undefined ? u.mass(r.mean_weight_kg) : undefined,
            `${num(r.intake_pct, 1)}%`,
            m && m.cows ? `${m.cows} ${m.pairs ? "pairs" : "cows"}` : undefined,
            m && m.bulls ? `${m.bulls} bulls` : undefined,
            m && !m.pairs && m.calves ? `${m.calves} calves` : undefined,
          ].filter(Boolean);
          return <div className="rline" key={h.id}><span className="rherd">{h.name}</span><span className="mono rfacts">{words.join("  ")}</span></div>;
        })}
        {mixed && (
          <div className="rline">
            <span className="rherd dim">AU</span>
            <span className="mono rfacts">{`${num(au.cow, 2)} cow  ${num(au.bull, 2)} bull  ${num(au.pair, 2)} pair  ${num(au.weaned_calf, 2)} weaned calf`}</span>
          </div>
        )}
        <FeedLog />
      </>
    );
  return (
    <>
      {herds.map((h) => {
        const r = inputs.herds[h.id] ?? { intake_pct: 2.5 };
        const mix = r.mix ?? { cows: 0, bulls: 0, calves: 0, pairs: false };
        const setMix = (p: Partial<typeof mix>) => herdSave(h.id, { mix: { ...mix, ...p } });
        return (
          <div className="rline wrap" key={h.id}>
            <span className="rherd">{h.name}</span>
            <NumberField value={r.mean_weight_kg} quantity="mass" label={`${h.name} mean weight`} min={1} max={3000} disabled={!can}
              onChange={(kg) => herdSave(h.id, { mean_weight_kg: kg })} />
            <Small value={r.intake_pct} label={`${h.name} daily intake`} after="%" step={0.1} places={1} min={0.1} max={10} disabled={!can}
              onSave={(v) => herdSave(h.id, { intake_pct: v })} />
            {h.species === "cattle" && <>
              <Small value={mix.cows} label={`${h.name} cows`} after={mix.pairs ? "pairs" : "cows"} disabled={!can} onSave={(v) => setMix({ cows: v })} />
              <Small value={mix.bulls} label={`${h.name} bulls`} after="bulls" disabled={!can} onSave={(v) => setMix({ bulls: v })} />
              {!mix.pairs && <Small value={mix.calves} label={`${h.name} weaned calves`} after="calves" disabled={!can} onSave={(v) => setMix({ calves: v })} />}
              <Check checked={mix.pairs} disabled={!can} onChange={(v) => setMix({ pairs: v })}>pairs</Check>
            </>}
          </div>
        );
      })}
      {mixed && (
        <div className="rline wrap">
          <span className="rherd dim">AU</span>
          <Small value={au.cow} label="AU per cow" after="cow" step={0.01} places={2} min={0.01} max={5} disabled={!can} onSave={(v) => save({ au: { cow: v } })} />
          <Small value={au.bull} label="AU per bull" after="bull" step={0.01} places={2} min={0.01} max={5} disabled={!can} onSave={(v) => save({ au: { bull: v } })} />
          <Small value={au.pair} label="AU per pair" after="pair" step={0.01} places={2} min={0.01} max={5} disabled={!can} onSave={(v) => save({ au: { pair: v } })} />
          <Small value={au.weaned_calf} label="AU per weaned calf" after="weaned calf" step={0.01} places={2} min={0.01} max={5} disabled={!can} onSave={(v) => save({ au: { weaned_calf: v } })} />
        </div>
      )}
      {err && <span className="mono err">{err}</span>}
      <FeedLog />
    </>
  );
}

// ---- feed log ------------------------------------------------------------------------

function FeedLog() {
  const herds = useStore((s) => s.state?.herds ?? []);
  const herdId = useStore((s) => s.herdId);
  const tz = useStore((s) => s.state?.farm?.timezone);
  const u = useUnits();
  const canAdd = useCan("hand");
  const canEdit = useCan("manager");
  const [rows, setRows] = useState<FeedEntry[] | null>(null);
  const [err, setErr] = useState<string>();
  const [draft, setDraft] = useState({ date: localToday(tz), herd_id: herdId ?? herds[0]?.id ?? "", kg: undefined as number | undefined, kind: "", note: "" });
  const load = () => reportsApi.feed().then(setRows, (e) => setErr((e as Error).message));
  useEffect(() => {
    void load();
  }, []);
  const name = (id: string) => herds.find((h) => h.id === id)?.name ?? id;
  const add = async () => {
    if (draft.kg === undefined || !draft.herd_id || !draft.date) return;
    setErr(undefined);
    try {
      await reportsApi.addFeed({ herd_id: draft.herd_id, date: draft.date, kg_dm: draft.kg, kind: draft.kind.trim() || undefined, note: draft.note.trim() || undefined });
      setDraft({ ...draft, kg: undefined, note: "" });
      await load();
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const remove = async (id: string) => {
    await reportsApi.deleteFeed(id).catch((e) => setErr((e as Error).message));
    await load();
  };
  if (!rows) return err ? <span className="mono err">{err}</span> : null;
  return (
    <div className="feedlog">
      {rows.length > 0 && (
        <table className="tbl">
          <thead><tr><th>date</th>{herds.length > 1 && <th>herd</th>}<th>kind</th><th className="num">{u.unitLabel("mass")} DM</th><th>note</th>{canEdit && <th />}</tr></thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.id}>
                <td>{r.date}</td>
                {herds.length > 1 && <td>{name(r.herd_id)}</td>}
                <td>{r.kind}</td>
                <td className="num">{num(u.toDisplay(r.kg_dm, "mass"), 0)}</td>
                <td className="note">{r.note ?? ""}</td>
                {canEdit && <td className="del"><Button small kind="plain" onClick={() => remove(r.id)}>Delete</Button></td>}
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {canAdd && herds.length > 0 && (
        <form className="rline wrap" onSubmit={(e) => { e.preventDefault(); void add(); }}>
          <Input type="date" mono className="sm" value={draft.date} aria-label="Date" onChange={(e) => setDraft({ ...draft, date: e.target.value })} style={{ width: 150 }} />
          {herds.length > 1 && (
            <select className="input sm" value={draft.herd_id} aria-label="Herd" onChange={(e) => setDraft({ ...draft, herd_id: e.target.value })}>
              {herds.map((h) => <option key={h.id} value={h.id}>{h.name}</option>)}
            </select>
          )}
          <Input className="sm" value={draft.kind} placeholder="hay" aria-label="Kind" onChange={(e) => setDraft({ ...draft, kind: e.target.value })} style={{ width: 110 }} />
          <NumberField value={draft.kg} quantity="mass" label="Dry matter" min={0} onChange={(kg) => setDraft({ ...draft, kg })} />
          <Input className="sm" value={draft.note} placeholder="Note" aria-label="Note" onChange={(e) => setDraft({ ...draft, note: e.target.value })} style={{ width: 180 }} />
          <Button small type="submit">Add</Button>
        </form>
      )}
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
