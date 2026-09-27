// The small sheet a clicked feature opens: its name, facts, scope and dates, notes, Delete.
// Managers change it in place; everyone else reads it.

import { useEffect, useLayoutEffect, useRef, useState, type TextareaHTMLAttributes } from "react";
import { featuresApi, type FeaturePatch, type MapFeature } from "../../api/d";
import { useStore } from "../../store";
import { useCan } from "../../store/me";
import { dropFeature, features, putFeature } from "../../store/d";
import { Button, Input, Segmented } from "../../ui";
import { NumberField } from "../../ui/NumberField";
import { useUnits } from "../../units";
import { endOfDay, facts, isActive, lastDay, localDate, radiusOf, scopePaddock, spec } from "./model";

export function FeatureSheet({ id, select, close }: { id: string; select: (id: string | null) => void; close: () => void }) {
  const f = features.use((list) => list.find((x) => x.id === id));
  const [, tick] = useState(0);
  const on = !!f && isActive(f, Date.now());

  useEffect(() => {
    select(id);
    return () => select(null);
  }, [id, select]);
  // Gone (deleted here or elsewhere, or its paddock was), or its time is up: the sheet goes too.
  useEffect(() => {
    if (!on) close();
  }, [on, close]);
  useEffect(() => {
    const end = f?.active_until ? Date.parse(f.active_until) - Date.now() : undefined;
    if (end === undefined || end > 2 ** 31 - 1) return;
    const t = setTimeout(() => tick((n) => n + 1), Math.max(0, end) + 50);
    return () => clearTimeout(t);
  }, [f?.active_until]);

  if (!f || !on) return null;
  return <Body f={f} close={close} />;
}

function Body({ f, close }: { f: MapFeature; close: () => void }) {
  const state = useStore((s) => s.state);
  const paddocks = state?.paddocks ?? [];
  const tz = state?.farm?.timezone ?? "UTC";
  const u = useUnits();
  const edit = useCan("manager");
  const [name, setName] = useState(f.name ?? "");
  const [notes, setNotes] = useState(f.notes ?? "");
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  const kind = spec(f.kind);
  // A saved change (here or from someone else) shows; typing in another field is kept.
  useEffect(() => setName(f.name ?? ""), [f.name]);
  useEffect(() => setNotes(f.notes ?? ""), [f.notes]);

  const change = async (p: FeaturePatch) => {
    setErr(undefined);
    try {
      putFeature(await featuresApi.update(f.id, p));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const remove = async () => {
    setBusy(true);
    try {
      await featuresApi.remove(f.id);
      dropFeature(f.id);
      close();
    } catch (e) {
      setErr((e as Error).message);
      setBusy(false);
    }
  };

  // An exclusion keeps to its paddock or covers the whole farm.
  const home = f.paddock_id ? paddocks.find((p) => p.id === f.paddock_id) : f.geometry.type === "Polygon" ? scopePaddock(f.geometry, paddocks) : undefined;
  const radius = f.geometry.type === "Point" && f.kind === "hazard" ? radiusOf(f) : undefined;
  // Exclusions and hazards are the temporary kinds (a wet spot, reseeding, an open trench).
  const dated = f.kind === "exclusion" || f.kind === "hazard";

  return (
    <>
      <form onSubmit={(e) => { e.preventDefault(); if (name.trim() !== (f.name ?? "")) void change({ name: name.trim() || null }); }}>
        <Input className="title" value={name} placeholder={kind.label} aria-label="Name" readOnly={!edit}
          onChange={(e) => setName(e.target.value)}
          onBlur={() => { if (edit && name.trim() !== (f.name ?? "")) void change({ name: name.trim() || null }); }} />
      </form>
      <p className="facts mono">{facts(f, u, paddocks, tz, edit ? { radius: false, until: !dated } : undefined)}</p>
      {edit && f.kind === "exclusion" && home && (
        <Segmented label="Scope" value={f.paddock_id ? "paddock" : "farm"}
          options={[{ value: "paddock", label: "This paddock", title: home.name }, { value: "farm", label: "Whole farm" }]}
          onChange={(v) => void change({ paddock_id: v === "paddock" ? home.id : null })} />
      )}
      {edit && radius !== undefined && (
        <div className="dline">
          <NumberField label="Radius" quantity="len" value={radius} min={0.5} onChange={(m) => void change({ props: { ...f.props, radius_m: m } })} />
          <span className="dim">around</span>
        </div>
      )}
      {edit && dated && (
        <div className="dline">
          <span className="dim">until</span>
          <input type="date" className="input sm mono" aria-label="Until" min={localDate(Date.now(), tz)}
            value={f.active_until ? lastDay(f.active_until, tz) : ""}
            onChange={(e) => void change({ active_until: e.target.value ? endOfDay(e.target.value, tz) : null })} />
        </div>
      )}
      {edit ? (
        <AutoText value={notes} onChange={setNotes} placeholder="Notes" aria-label="Notes"
          onBlur={() => { const v = notes.trim(); if (v !== (f.notes ?? "")) void change({ notes: v || null }); }} />
      ) : (
        f.notes && <p className="dnotes">{f.notes}</p>
      )}
      {err && <p className="err dmsg">{err}</p>}
      {edit && (
        <div className="acts">
          <Button small kind="plain" className="danger" onClick={remove} disabled={busy}>Delete</Button>
        </div>
      )}
    </>
  );
}

// A textarea that grows with its text (as the paddock sheet's notes).
function AutoText({ value, onChange, ...rest }: { value: string; onChange: (v: string) => void } & Omit<TextareaHTMLAttributes<HTMLTextAreaElement>, "value" | "onChange">) {
  const ref = useRef<HTMLTextAreaElement>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${el.scrollHeight + 2}px`;
  }, [value]);
  return <textarea ref={ref} className="input notes" rows={1} spellCheck={false} value={value} onChange={(e) => onChange(e.target.value)} {...rest} />;
}
