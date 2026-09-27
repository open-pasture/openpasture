// Settings > Alerts: each rule a sentence with its number inline and an on/off check; the
// notify check, quiet hours and escalation only once a channel can send.

import { useEffect, useState } from "react";
import { alertsApi, type RuleConfig, type RulesChange, type RulesView, type RuleView } from "../../api/a-engine";
import { Check } from "../../ui/Check";
import { NumberField } from "../../ui/NumberField";
import { splitSentence } from "./model";

// A whole number with its unit after it: "[20] min". Saves on Enter or blur; Esc puts it back.
export function IntField({ value, unit, min, max, label, onChange }: {
  value: number; unit: string; min: number; max: number; label: string; onChange: (v: number) => void;
}) {
  const [text, setText] = useState(String(value));
  useEffect(() => setText(String(value)), [value]);
  const commit = () => {
    const v = Number(text.trim());
    if (!Number.isInteger(v) || v < min || v > max) return setText(String(value));
    if (v !== value) onChange(v);
  };
  return (
    <span className="numf">
      <input className="input sm mono" inputMode="numeric" aria-label={label} value={text} style={{ width: "calc(4ch + 26px)" }}
        onChange={(e) => setText(e.target.value)} onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") { e.preventDefault(); commit(); }
          if (e.key === "Escape") { e.stopPropagation(); setText(String(value)); }
        }} />
      <span className="mono dim">{unit}</span>
    </span>
  );
}

// A farm-time HH:MM, saved on change; empty clears it.
export function TimeField({ value, label, placeholder, onChange }: { value?: string; label: string; placeholder?: string; onChange: (v: string | null) => void }) {
  const [v, setV] = useState(value ?? "");
  useEffect(() => setV(value ?? ""), [value]);
  const save = () => {
    if (v === (value ?? "")) return;
    if (v === "" || /^\d\d:\d\d$/.test(v)) onChange(v || null);
  };
  return (
    <input className="input sm mono atime" type="time" aria-label={label} value={v} placeholder={placeholder}
      onChange={(e) => setV(e.target.value)} onBlur={save}
      onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); save(); } }} />
  );
}

function Number_({ r, onSave }: { r: RuleView; onSave: (c: Partial<RuleConfig>) => void }) {
  if (r.unit === "min") {
    const v = r.after_min ?? r.default.after_min ?? 1;
    return <IntField value={v} unit="min" min={1} max={10080} label={r.kind} onChange={(after_min) => onSave({ after_min })} />;
  }
  if (r.unit === "%") {
    const v = r.threshold ?? r.default.threshold ?? 1;
    return <IntField value={Math.round(v)} unit="%" min={1} max={99} label={r.kind} onChange={(threshold) => onSave({ threshold })} />;
  }
  if (r.unit === "m") {
    const v = r.threshold ?? r.default.threshold;
    return <NumberField value={v} quantity="len" min={0.5} max={1000} label={r.kind} onChange={(threshold) => onSave({ threshold })} />;
  }
  return null;
}

function Rule({ r, notify, onSave }: { r: RuleView; notify: boolean; onSave: (c: Partial<RuleConfig>) => void }) {
  const parts = splitSentence(r.sentence);
  return (
    <div className="arule" data-off={!r.enabled || undefined}>
      <Check checked={r.enabled} onChange={(enabled) => onSave({ enabled })} label={`${r.sentence.replace("{n}", "n")}: on`} />
      <span className="asent">{parts ? <>{parts[0]}<Number_ r={r} onSave={onSave} />{parts[1]}</> : r.sentence}</span>
      {notify && r.severity !== "info" && (
        <Check checked={r.notify} disabled={!r.enabled} onChange={(n) => onSave({ notify: n })}>Notify</Check>
      )}
    </div>
  );
}

export function AlertSettings() {
  const [v, setV] = useState<RulesView>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    void alertsApi.rules().then(setV, () => setV(undefined));
  }, []);
  if (!v) return null;
  const save = async (change: RulesChange) => {
    setErr(undefined);
    try {
      setV(await alertsApi.setRules(change));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const notify = v.configured.length > 0;
  return (
    <div className="arules">
      {v.rules.map((r) => <Rule key={r.kind} r={r} notify={notify} onSave={(c) => save({ rules: { [r.kind]: c } })} />)}
      {notify && (
        <>
          <div className="line aquiet">
            <span>Quiet from</span>
            <TimeField value={v.policy.quiet_start} label="Quiet from" onChange={(t) => save({ policy: { quiet_start: t, quiet_end: t === null ? null : v.policy.quiet_end ?? "06:00" } })} />
            <span>to</span>
            <TimeField value={v.policy.quiet_end} label="Quiet to" onChange={(t) => save({ policy: { quiet_end: t, quiet_start: t === null ? null : v.policy.quiet_start ?? "22:00" } })} />
          </div>
          <div className="line">
            <span>Escalate after</span>
            <IntField value={v.policy.escalate_after_min} unit="min" min={1} max={1440} label="Escalate after" onChange={(m) => save({ policy: { escalate_after_min: m } })} />
          </div>
        </>
      )}
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
