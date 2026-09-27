import { lazy, Suspense, useEffect, useMemo, useState, type ReactNode } from "react";
import { api, type Herd, type ParkReason } from "../../api";
import { kAnimals, type ImportPreview, type ImportResult, type Mapping, type RowError } from "../../api/k-animals";
import { guarded, herdBulk, herdColumns, type HerdBulk, type HerdRow } from "../../registry";
import { store, useStore } from "../../store";
import { useCan } from "../../store/me";
import { kAnimalsSlice, keepBatch, type Pending } from "../../store/k-animals";
import { Button, Input, Menu, Segmented } from "../../ui";
import { FilePick } from "../../ui/FilePick";
import { MappingRow } from "../../ui/MappingRow";
import { Table, type Column } from "../../ui/Table";
import { age, useNow } from "../../util";
import { Edit } from "./Edit";
import { battery, by, cardsPossible, day, each, FIELDS, herdRows, keysCsv, mappedRows, reasonWord, rowText, sexWord, type Shown } from "./herd";

const AnimalPage = lazy(() => import("./AnimalPage").then((m) => ({ default: m.AnimalPage })));

// #/herd: the table. #/herd/<tag>: that animal's page.
export function HerdView({ rest }: { rest: string }) {
  if (rest) return <Suspense fallback={null}><AnimalPage rest={rest} /></Suspense>;
  return <HerdTable />;
}

const PARK: { value: ParkReason; label: string }[] = [{ value: "charging", label: "Charging" }, { value: "shelf", label: "Shelf" }, { value: "repair", label: "Repair" }];

type Flow = { k: "import"; file: File } | { k: "link"; file: File };

function HerdTable() {
  const state = useStore((s) => s.state)!;
  const herdId = useStore((s) => s.herdId);
  const animals = useStore((s) => s.animals);
  const collars = useStore((s) => s.collars);
  const now = useNow(5000);
  const canEdit = useCan("manager");
  const canSelect = useCan("hand");
  const extra = herdColumns.use();
  const bulk = herdBulk.use();
  const pending = kAnimalsSlice.use((s) => s.pending);
  const [shown, setShown] = useState<Shown>("active");
  const [q, setQ] = useState("");
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [flow, setFlow] = useState<Flow>();
  const [msg, setMsg] = useState<string>();
  const [busy, setBusy] = useState(false);
  const herd = state.herds.find((h) => h.id === herdId);

  const rows = useMemo(() => herdRows(animals, collars, herdId, shown), [animals, collars, herdId, shown]);
  // A new herd or tab starts with nothing selected; rows that went away drop out.
  useEffect(() => setSel(new Set()), [herdId, shown]);
  useEffect(() => {
    const ids = new Set(rows.map((r) => r.id));
    setSel((s) => ([...s].every((id) => ids.has(id)) ? s : new Set([...s].filter((id) => ids.has(id)))));
  }, [rows]);
  useEffect(() => () => kAnimalsSlice.patch({ pending: undefined }), []);

  const edit = (r: HerdRow, field: Parameters<typeof Edit>[0]["field"], placeholder?: string) =>
    r.animal ? <Edit a={r.animal} field={field} can={canEdit} onError={setMsg} placeholder={placeholder} /> : null;
  const columns: Column<HerdRow>[] = useMemo(() => {
    const core: (Column<HerdRow> & { order: number })[] = [
      { id: "tag", order: 10, label: "tag", width: 96, sort: by((r) => r.animal?.tag ?? ""), cell: (r) => (
        <a className="tagl" href={`#/herd/${encodeURIComponent(r.animal?.tag ?? r.collar?.id ?? "")}`}>{r.animal?.tag ?? <span className="dim">–</span>}</a>
      ) },
      { id: "name", order: 20, label: "name", width: 150, sort: by((r) => r.animal?.name), cell: (r) => edit(r, "name") },
      { id: "eid", order: 30, label: "EID", width: 160, sort: by((r) => r.animal?.eid), cell: (r) => edit(r, "eid") },
      { id: "breed", order: 40, label: "breed", width: 140, sort: by((r) => r.animal?.breed), cell: (r) => edit(r, "breed") },
      { id: "sex", order: 50, label: "sex", width: 110, sort: by((r) => sexWord(r.animal?.sex)), cell: (r) => edit(r, "sex") },
      { id: "born", order: 60, label: "born", width: 130, sort: by((r) => r.animal?.born), cell: (r) => edit(r, "born") },
    ];
    if (shown === "removed") {
      core.push({ id: "removed", order: 70, label: "removed", sort: by((r) => r.animal?.removed_at), cell: (r) => `${reasonWord(r.animal?.removed_reason)}  ${day(r.animal?.removed_at)}` });
      return core;
    }
    core.push(
      { id: "collar", order: 70, label: "collar", width: 150, sort: by((r) => r.collar?.name), cell: (r) => r.collar && (
        <span className={r.collar.parked_at ? "dim" : undefined}>{r.collar.name}{r.collar.parked_reason && <span className="dim">  {r.collar.parked_reason}</span>}</span>
      ) },
      { id: "battery", order: 80, label: "battery", width: 80, sort: by((r) => r.collar?.battery), cell: (r) => (
        <span className={r.collar?.battery !== undefined && r.collar.battery < 0.2 ? "low" : undefined}>{battery(r.collar?.battery)}</span>
      ) },
      { id: "seen", order: 90, label: "seen", width: 70, sort: by((r) => (r.collar?.last_seen ? -Date.parse(r.collar.last_seen) : undefined)), cell: (r) => r.collar?.last_seen ? age(r.collar.last_seen, now) : "" },
    );
    const added = extra.map((c) => ({ id: c.id, order: c.order, label: c.label, width: c.width, sort: c.sort, cell: (r: HerdRow) => guarded(c.id, <c.Cell row={r} />) }));
    return [...core, ...added].sort((a, b) => a.order - b.order).map((c, i, all) => (i === all.length - 1 ? { ...c, width: undefined } : c));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [shown, extra, canEdit, now]);

  const picked = rows.filter((r) => sel.has(r.id));
  const others = state.herds.filter((h) => h.id !== herdId);
  // Our own actions show only where they can do something.
  const fits = (b: HerdBulk) =>
    b.id === "move" ? others.length > 0 && shown === "active"
      : b.id === "park" ? picked.some((r) => r.collar && !r.collar.parked_at)
        : b.id === "unpark" ? picked.some((r) => r.collar?.parked_at)
          : true;

  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setMsg(undefined);
    try {
      await f();
      setSel(new Set());
      kAnimalsSlice.patch({ pending: undefined });
      await store.refresh();
    } catch (e) {
      setMsg((e as Error).message);
      await store.refresh();
    } finally {
      setBusy(false);
    }
  };

  // A registered action runs on the selection; ours that need a choice (herd, reason) ask for it here first.
  const runBulk = async (b: HerdBulk) => {
    setBusy(true);
    setMsg(undefined);
    try {
      await b.run(picked);
      if (kAnimalsSlice.get().pending) return;
      setSel(new Set());
      await store.refresh();
    } catch (e) {
      setMsg((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const moveTo = (p: Pending, h: Herd) => act(() => each(p.rows, async (r) => {
    if (r.animal && r.animal.herd_id !== h.id) await api.updateAnimal(r.animal.id, { herd_id: h.id });
    if (r.collar && r.collar.herd_id !== h.id) await api.updateCollar(r.collar.id, { herd_id: h.id });
  }));
  const park = (p: Pending, reason: ParkReason) => act(() => each(p.rows.filter((r) => r.collar && !r.collar.parked_at), (r) => kAnimals.park(r.collar!.id, reason)));

  let right: ReactNode;
  if (pending?.kind === "move") {
    right = <>
      <span className="mono dim">Move {pending.rows.length} to</span>
      {others.map((h) => <Button key={h.id} small disabled={busy} onClick={() => moveTo(pending, h)}>{h.name}</Button>)}
      <Button small kind="plain" onClick={() => kAnimalsSlice.patch({ pending: undefined })}>Cancel</Button>
    </>;
  } else if (pending?.kind === "park") {
    right = <>
      <span className="mono dim">Park {pending.rows.filter((r) => r.collar && !r.collar.parked_at).length}</span>
      {PARK.map((p) => <Button key={p.value} small disabled={busy} onClick={() => park(pending, p.value)}>{p.label}</Button>)}
      <Button small kind="plain" onClick={() => kAnimalsSlice.patch({ pending: undefined })}>Cancel</Button>
    </>;
  } else if (picked.length) {
    right = <>
      <span className="mono dim">{picked.length} selected</span>
      {bulk.filter(fits).map((b) => <Button key={b.id} small disabled={busy} onClick={() => void runBulk(b)}>{b.label}</Button>)}
      <Button small kind="plain" onClick={() => setSel(new Set())}>Clear</Button>
    </>;
  } else if (canEdit && herd) {
    right = <>
      <FilePick accept=".csv,.txt,text/csv,text/plain" onPick={(file) => { setMsg(undefined); setFlow({ k: "import", file }); }}>Import CSV</FilePick>
      <FilePick accept=".csv,.txt,text/csv,text/plain" onPick={(file) => { setMsg(undefined); setFlow({ k: "link", file }); }}>Link collars</FilePick>
    </>;
  }

  if (!herd) return null;
  const count = rows.filter((r) => r.animal).length;
  return (
    <div className="herdv">
      <div className="hbar">
        {state.herds.length > 1 ? (
          <Menu trigger={<span className="hname">{herd.name}</span>}
            items={state.herds.map((h) => ({ label: h.name, current: h.id === herd.id, onSelect: () => store.setHerd(h.id) }))} />
        ) : <span className="hname">{herd.name}</span>}
        <Segmented label="Animals" value={shown} onChange={setShown} options={[{ value: "active", label: "Active" }, { value: "removed", label: "Removed" }]} />
        <Input className="sm hfilter" placeholder="Filter" aria-label="Filter" value={q} onChange={(e) => setQ(e.target.value)} />
        <span className="mono dim hcount">{count}</span>
        <div className="hacts">{right}</div>
      </div>
      {flow?.k === "import" && <ImportFlow key={flow.file.name + flow.file.lastModified} file={flow.file} herd={herd} onDone={() => setFlow(undefined)} />}
      {flow?.k === "link" && <LinkFlow key={flow.file.name + flow.file.lastModified} file={flow.file} herd={herd} onDone={() => setFlow(undefined)} />}
      {msg && <p className="mono err hmsg">{msg}</p>}
      <div className="htable">
        <Table rows={rows} columns={columns} rowKey={(r) => r.id} text={rowText} query={q}
          selected={canSelect && shown === "active" ? sel : undefined} onSelect={canSelect && shown === "active" ? setSel : undefined}
          initialSort={{ col: "tag", dir: "asc" }} height="100%" />
      </div>
    </div>
  );
}

function Errors({ errors }: { errors: RowError[] }) {
  if (!errors.length) return null;
  const shown = errors.slice(0, 6);
  return (
    <ul className="herrs mono">
      {shown.map((e) => <li key={e.row}><span className="dim">row {e.row}</span>{e.error}</li>)}
      {errors.length > shown.length && <li className="dim">{errors.length - shown.length} more</li>}
    </ul>
  );
}

// Pick a file, match its columns, import. The result says what changed and which rows didn't go in.
function ImportFlow({ file, herd, onDone }: { file: File; herd: Herd; onDone: () => void }) {
  const [p, setP] = useState<ImportPreview>();
  const [mapping, setMapping] = useState<Mapping>({});
  const [result, setResult] = useState<ImportResult>();
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    kAnimals.preview(file, herd.id).then((x) => { setP(x); setMapping(x.mapping); }, (e) => setErr((e as Error).message));
  }, [file, herd.id]);
  const commit = async () => {
    if (!p) return;
    setBusy(true);
    setErr(undefined);
    try {
      setResult(await kAnimals.commit(p.import_id, mapping, herd.id));
      await store.refresh();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  if (result)
    return (
      <section className="hflow">
        <div className="hline">
          <span className="mono">{result.created} added  {result.updated} updated  {result.unchanged} unchanged</span>
          {result.errors.length > 0 && <span className="mono warn">{result.errors.length} skipped</span>}
          <span className="sp" />
          <Button small onClick={onDone}>Done</Button>
        </div>
        <Errors errors={result.errors} />
      </section>
    );
  // The preview's errors are for the guessed columns; a new choice shows its own after importing.
  const same = p && JSON.stringify(p.mapping) === JSON.stringify(mapping);
  const cols = FIELDS.filter((f) => mapping[f.key]);
  return (
    <section className="hflow">
      <div className="hline">
        <span className="mono">{file.name}</span>
        {p && <span className="mono dim">{p.total} rows</span>}
        {same && p.errors.length > 0 && <span className="mono warn">{p.errors.length} to fix</span>}
        <span className="sp" />
        <Button small kind="plain" onClick={onDone}>Cancel</Button>
        <Button small kind="primary" disabled={!p || !mapping.tag || busy} onClick={commit}>Import</Button>
      </div>
      {err && <p className="mono err">{err}</p>}
      {p && <>
        <MappingRow fields={FIELDS} columns={p.columns} value={mapping} onChange={(m) => setMapping(m as Mapping)} />
        {cols.length > 0 && (
          <table className="tbl hpeek">
            <thead><tr>{cols.map((f) => <th key={f.key}>{f.label}</th>)}</tr></thead>
            <tbody>{mappedRows(p, mapping).map((r, i) => <tr key={i}>{cols.map((f) => <td key={f.key}>{r[f.key]}</td>)}</tr>)}</tbody>
          </table>
        )}
        {same && <Errors errors={p.errors} />}
      </>}
    </section>
  );
}

// Rows "tag,collar name" make the collars and put each on its animal. The keys come back
// once: as a CSV, or as cards when the server has an https public URL.
function LinkFlow({ file, herd, onDone }: { file: File; herd: Herd; onDone: () => void }) {
  const publicUrl = useStore((s) => s.state?.settings.server.public_url);
  const [batch, setBatch] = useState<{ id: string; n: number }>();
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  const link = async () => {
    setBusy(true);
    setErr(undefined);
    try {
      const b = await kAnimals.link(file, herd.id);
      keepBatch(b.batch_id, b.collars);
      setBatch({ id: b.batch_id, n: b.collars.length });
      await store.refresh();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const download = () => {
    const collars = kAnimalsSlice.get().batches[batch!.id] ?? [];
    const url = URL.createObjectURL(new Blob([keysCsv(collars)], { type: "text/csv" }));
    const a = document.createElement("a");
    a.href = url;
    a.download = `collar-keys-${herd.name.toLowerCase().replace(/\W+/g, "-")}.csv`;
    a.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  };
  if (batch)
    return (
      <section className="hflow">
        <div className="hline">
          <span className="mono">{batch.n} collars linked</span>
          <span className="sp" />
          <Button small onClick={download}>Keys CSV</Button>
          {cardsPossible(publicUrl) && <Button small kind="primary" onClick={() => (location.hash = `/print/cards/${batch.id}`)}>Print cards</Button>}
          <Button small kind="plain" onClick={onDone}>Done</Button>
        </div>
      </section>
    );
  return (
    <section className="hflow">
      <div className="hline">
        <span className="mono">{file.name}</span>
        <span className="mono dim">tag, collar name</span>
        <span className="sp" />
        <Button small kind="plain" onClick={onDone}>Cancel</Button>
        <Button small kind="primary" disabled={busy} onClick={link}>Link</Button>
      </div>
      {err && <p className="mono err">{err}</p>}
    </section>
  );
}
