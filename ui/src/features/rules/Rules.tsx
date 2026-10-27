// Settings > Farm rules: the farmer's own rules for the farm agent, in their words, each with
// who wrote it and when. Managers add, change and remove them. A rule that plainly keeps
// ground out offers to become a no-go zone (or a dated exclusion) on the map. Under them, the
// one gate setting the farm chooses: water in every boundary.

import { useEffect, useState } from "react";
import { putFeature } from "../../store/d";
import { useStore } from "../../store";
import { useCan } from "../../store/me";
import { Button, Input } from "../../ui";
import { Check } from "../../ui/Check";
import { rulesApi, type FarmRule, type GateSettings } from "./api";
import { byLine } from "./model";

export function Rules() {
  const [list, setList] = useState<FarmRule[] | null>(null);
  const [gate, setGate] = useState<GateSettings>();
  const [err, setErr] = useState<string>();
  const edits = useCan("manager");
  const tz = useStore((s) => s.state?.farm?.timezone);
  useEffect(() => {
    rulesApi.list().then(setList, (e: Error) => setErr(e.message));
    rulesApi.gate().then(setGate, () => {});
  }, []);
  if (!list) return err ? <p className="mono err">{err}</p> : null;

  const put = (r: FarmRule) => setList((l) => l && (l.some((x) => x.id === r.id) ? l.map((x) => (x.id === r.id ? r : x)) : [...l, r]));
  return (
    <div className="rules">
      {list.length > 0 && (
        <ol>
          {list.map((r) => (
            <Rule key={r.id} r={r} tz={tz} edits={edits} onChanged={put} onRemoved={() => setList((l) => l && l.filter((x) => x.id !== r.id))} />
          ))}
        </ol>
      )}
      {edits && <AddRule onAdded={put} />}
      {gate && (
        <Check checked={gate.require_water} disabled={!edits}
          onChange={(v) => rulesApi.setGate({ require_water: v }).then(setGate, (e: Error) => setErr(e.message))}>
          Every boundary has water inside
        </Check>
      )}
      {err && <p className="mono err">{err}</p>}
    </div>
  );
}

function Rule({ r, tz, edits, onChanged, onRemoved }: { r: FarmRule; tz?: string; edits: boolean; onChanged: (r: FarmRule) => void; onRemoved: () => void }) {
  const [text, setText] = useState(r.text);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  useEffect(() => setText(r.text), [r.text]);
  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    setErr(undefined);
    try {
      await f();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const save = () => text.trim() && text.trim() !== r.text && run(async () => onChanged(await rulesApi.edit(r.id, text)));
  return (
    <li>
      {edits ? (
        <form onSubmit={(e) => { e.preventDefault(); void save(); }}>
          <Input value={text} aria-label="Rule" onChange={(e) => setText(e.target.value)} onBlur={() => void save()} />
        </form>
      ) : (
        <p>{r.text}</p>
      )}
      <div className="rline">
        <span className="mono dim">{byLine(r, tz)}{r.feature_id ? " · on the map" : ""}</span>
        {edits && r.suggestion && (
          <Button small kind="plain" disabled={busy} onClick={() => run(async () => {
            const made = await rulesApi.convert(r.id);
            putFeature(made.feature);
            onChanged(made.rule);
          })}>{r.suggestion.text}</Button>
        )}
        {edits && <Button small kind="plain" className="danger" disabled={busy} onClick={() => run(async () => { await rulesApi.remove(r.id); onRemoved(); })}>Remove</Button>}
        {err && <span className="mono err">{err}</span>}
      </div>
    </li>
  );
}

function AddRule({ onAdded }: { onAdded: (r: FarmRule) => void }) {
  const [text, setText] = useState("");
  const [err, setErr] = useState<string>();
  const add = async () => {
    if (!text.trim()) return;
    try {
      onAdded(await rulesApi.add(text));
      setText("");
      setErr(undefined);
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <form className="radd" onSubmit={(e) => { e.preventDefault(); void add(); }}>
      <Input value={text} placeholder="Never put the heifers in the creek paddock before July" aria-label="New rule" onChange={(e) => setText(e.target.value)} />
      {text.trim() && <Button small kind="primary" type="submit">Add</Button>}
      {err && <span className="mono err">{err}</span>}
    </form>
  );
}
