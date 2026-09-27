// The welfare record in the app (stream H): the animal page's cue ledger and sparklines, the
// herd menu's Training mode sentence, the herd panel's training line and the Herd table's
// trained column.

import { useEffect, useMemo, useState } from "react";
import type { Animal, Collar } from "../../api";
import { hApi, type AnimalWelfare, type LedgerRow, type Training } from "../../api/h";
import type { HerdRow } from "../../registry";
import { store } from "../../store";
import { setTraining, useHerdWelfare, useWelfareAnimal } from "../../store/h";
import { useCan } from "../../store/me";
import { Check } from "../../ui/Check";
import { NumberField } from "../../ui/NumberField";
import { Spark } from "../../ui/Spark";
import { Table, type Column } from "../../ui/Table";
import { useUnits } from "../../units";
import { endingWord, outcomesText, ringWord, secs, sparks, statusText, toneText, trainingLine } from "./model";

// The last two weeks, like the battery sparkline beside it.
const DAYS = 14;
const ROW = 30;

// "07:41:03" today, "Sep 26 07:41" before, in the reader's clock.
function stamp(iso: string, now = new Date()) {
  const d = new Date(iso);
  const p = (n: number) => String(n).padStart(2, "0");
  if (d.toDateString() === now.toDateString()) return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
  return `${d.toLocaleDateString(undefined, { month: "short", day: "numeric" })} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

// Animal page: cues a day and seconds of tone a day, trained or learning, and the ledger.
export function AnimalWelfareSection({ animal }: { animal?: Animal; collar?: Collar }) {
  const u = useUnits();
  const [w, setW] = useState<AnimalWelfare>();
  const id = animal?.id;
  const lastSeen = useCollarSeen(animal?.collar_id);
  useEffect(() => {
    if (!id) return;
    let live = true;
    hApi.animal(id, { from: `-${DAYS}d` }).then((v) => live && setW(v), () => live && setW(undefined));
    return () => void (live = false);
    // A report from its collar may bring cues.
  }, [id, lastSeen]);
  const columns = useMemo<Column<LedgerRow>[]>(() => [
    { id: "at", label: "time", width: 80, cell: (r) => <span className="mono">{stamp(r.at)}</span> },
    { id: "kind", label: "cue", width: 54, cell: (r) => <span className={r.kind === "outside" ? "hout" : undefined}>{r.kind}</span> },
    { id: "level", label: "level", width: 40, cell: (r) => <span className="mono">{r.level}</span> },
    { id: "tone", label: "tone", width: 44, cell: (r) => <span className="mono">{toneText(r.tone_ms)}</span> },
    { id: "margin", label: "margin", width: 50, cell: (r) => <span className="mono">{u.len(r.margin_m)}</span> },
    { id: "ended", label: "ended", width: 92, cell: (r) => <span className={r.derived ? "dim" : undefined}>{endingWord(r.outcome)}</span> },
    { id: "ring", label: "ring", width: 50, cell: (r) => ringWord(r.ring) },
    { id: "v", label: "boundary", width: 68, cell: (r) => <span className="mono">{r.boundary_version !== undefined ? `v${r.boundary_version}` : ""}</span> },
  ], [u]);
  if (!w || w.animal_id !== id) return null;
  const hasEpisodes = w.learning.status !== undefined;
  if (!hasEpisodes && w.cues.length === 0) return null;
  const s = sparks(w.days);
  const today = w.days[w.days.length - 1];
  const status = statusText(w.learning);
  const episodes = outcomesText(w.learning.outcomes);
  return (
    <section className="hwelf" aria-label="Cues">
      <ul className="kv">
        {status && <li><span>boundary</span><b>{status}</b></li>}
        <li>
          <span>cues</span>
          <b className="hline">
            <Spark values={s.cues} width={64} height={16} label="Cues a day" />
            {`${today ? today.warn + today.outside : 0} today`}
          </b>
        </li>
        <li>
          <span>tone</span>
          <b className="hline">
            <Spark values={s.tone} width={64} height={16} label="Seconds of tone a day" />
            {`${secs(today?.tone_s ?? 0)} today`}
          </b>
        </li>
        {episodes && <li><span>episodes</span><b>{episodes}{w.learning.derived ? "  from fixes" : ""}</b></li>}
      </ul>
      {w.cues.length > 0 && (
        <div className="hledger" style={{ height: Math.min(w.cues.length, 8) * ROW + 34 }}>
          <Table rows={w.cues} columns={columns} rowKey={(r) => `${r.collar_id}:${r.at}:${r.kind}`} rowHeight={ROW} />
        </div>
      )}
      {w.truncated !== undefined && <p className="mono dim">{w.truncated} older</p>}
    </section>
  );
}

// When the animal's collar last reported, so the page refetches after a report.
function useCollarSeen(collarId: string | undefined): string | undefined {
  const [seen, setSeen] = useState<string>();
  useEffect(() => {
    const read = () => setSeen(store.get().collars.find((c) => c.id === collarId)?.last_seen);
    read();
    return store.subscribe(read);
  }, [collarId]);
  // A minute is fine grained enough: cues come in with reports.
  return seen?.slice(0, 16);
}

// Herd menu > Training mode: "Training  warn [33] ft  trained after [5]".
export function TrainingItem({ herdId }: { herdId: string }) {
  const can = useCan("manager");
  const [t, setT] = useState<Training>();
  const [err, setErr] = useState<string>();
  const [n, setN] = useState("");
  useEffect(() => {
    let live = true;
    hApi.training(herdId).then((v) => {
      if (!live) return;
      setT(v);
      setN(String(v.trained_after));
    }, (e: Error) => live && setErr(e.message));
    return () => void (live = false);
  }, [herdId]);
  if (!t) return err ? <p className="mono err">{err}</p> : null;
  const save = async (p: Partial<Training>) => {
    setErr(undefined);
    try {
      const saved = await hApi.saveTraining(herdId, p);
      setT(saved);
      setN(String(saved.trained_after));
      setTraining(herdId, { enabled: saved.enabled, warn_m: saved.warn_m, trained_after: saved.trained_after });
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const commitN = () => {
    const v = Number(n.trim());
    if (!Number.isInteger(v) || v < 1 || v > 50) return setN(String(t.trained_after));
    if (v !== t.trained_after) void save({ trained_after: v });
  };
  return (
    <div className="htraining">
      <span className="hon"><Check checked={t.enabled} disabled={!can} onChange={(on) => void save({ enabled: on })}>Training</Check></span>
      <span className="dim">warn</span>
      <NumberField value={t.warn_m} quantity="len" min={1} max={100} label="Training warning zone" width={2} disabled={!can} onChange={(m) => void save({ warn_m: m })} />
      <span className="dim">trained after</span>
      <input className="input sm mono hn" inputMode="numeric" aria-label="Turned-back episodes in a row to be trained" value={n} disabled={!can}
        onChange={(e) => setN(e.target.value)} onBlur={commitN}
        onKeyDown={(e) => {
          if (e.key === "Enter") { e.preventDefault(); commitN(); }
          if (e.key === "Escape") { e.stopPropagation(); setN(String(t.trained_after)); }
        }} />
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}

// Herd panel, while training is on: "training  31/250 trained".
export function TrainingLine({ herdId }: { herdId: string }) {
  const h = useHerdWelfare(herdId);
  const line = trainingLine(h);
  return line ? <p className="mono htrainline">{line}</p> : null;
}

// Herd table: "trained" or "learning", nothing without episodes.
export function TrainedCell({ row }: { row: HerdRow }) {
  const a = useWelfareAnimal(row.animal?.id, row.animal?.herd_id);
  if (!a?.status) return null;
  return <span className={"hword" + (a.status === "learning" ? " dim" : "")}>{a.status}</span>;
}
