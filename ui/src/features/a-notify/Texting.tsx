// Settings > Texting: one channel's fields at a time, each with Test and a mono result line,
// then rows other streams register (textingRows).

import { useEffect, useState } from "react";
import { notifyApi, type Channels, type TestResult } from "../../api/a-notify";
import { Sections } from "../../registry";
import { useMe } from "../../store/me";
import { channels as channelsSlice, useChannels } from "../../store/a-notify";
import { Check } from "../../ui/Check";
import { Button, Input, Segmented } from "../../ui";
import { defaultTo, FIELDS, firstSegment, initialDraft, isOn, isSet, patchFor, placeholder, SEGMENTS, testChannel, type Draft, type Segment } from "./logic";
import { textingRows } from "./registry";

export function Texting() {
  const c = useChannels();
  const [seg, setSeg] = useState<Segment>();
  useEffect(() => {
    if (c && !seg) setSeg(firstSegment(c.configured));
  }, [c, seg]);
  if (!c || !seg) return null;
  return (
    <div className="texting">
      <Segmented label="Channel" value={seg} onChange={setSeg}
        options={SEGMENTS.map((s) => ({ value: s.value, label: <><i className={"dot" + (isOn(s.value, c.configured) ? " ok" : "")} />{s.label}</> }))} />
      <Form key={seg} seg={seg} c={c} />
      <Sections of={textingRows} props={{}} />
    </div>
  );
}

function Form({ seg, c }: { seg: Segment; c: Channels }) {
  const [draft, setDraft] = useState<Draft>(() => initialDraft(seg, c));
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  const patch = patchFor(seg, draft, c);
  const set = (id: string, v: string) => setDraft((d) => ({ ...d, [id]: v }));

  const save = async () => {
    if (!patch) return;
    setBusy(true);
    setErr(undefined);
    try {
      const next = await notifyApi.saveChannels(patch);
      channelsSlice.set(next);
      setDraft(initialDraft(seg, next));
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="tfields">
      <form className="tgrid" onSubmit={(e) => { e.preventDefault(); void save(); }}>
        {FIELDS[seg].map((f) => (
          <Input key={f.id} mono className={f.numeric ? "port" : undefined}
            type={f.secret && !f.plain ? "password" : "text"} inputMode={f.numeric ? "numeric" : undefined}
            value={draft[f.id] ?? ""} onChange={(e) => set(f.id, f.numeric ? e.target.value.replace(/\D/g, "") : e.target.value)}
            placeholder={placeholder(f, c)} aria-label={f.label} />
        ))}
        {seg === "email" && (
          <select className="input mono" value={draft.tls} aria-label="Encryption" onChange={(e) => set("tls", e.target.value)}>
            <option value="starttls">STARTTLS</option>
            <option value="tls">TLS</option>
            <option value="none">None</option>
          </select>
        )}
        {(patch || err) && (
          <div className="line">
            {patch && <Button small kind="primary" type="submit" disabled={busy}>Save</Button>}
            {err && <span className="mono err">{err}</span>}
          </div>
        )}
      </form>
      {seg === "relay" && <RelaySwitch c={c} />}
      <TestLine seg={seg} c={c} />
    </div>
  );
}

// The relay turns on only once it answered for this server's key.
function RelaySwitch({ c }: { c: Channels }) {
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  if (!isSet(c, "hosted_api_key") && !c.relay.enabled) return null;
  const flip = async (on: boolean) => {
    setBusy(true);
    setErr(undefined);
    try {
      channelsSlice.set(await notifyApi.saveChannels({ relay: { enabled: on } }));
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="line">
      <Check checked={c.relay.enabled} disabled={busy} onChange={(on) => void flip(on)}>Send through the relay</Check>
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}

function TestLine({ seg, c }: { seg: Segment; c: Channels }) {
  const me = useMe();
  const [to, setTo] = useState(() => defaultTo(seg, me));
  const [res, setRes] = useState<TestResult | "running">();
  if (!isOn(seg, c.configured)) return null;
  const run = async () => {
    setRes("running");
    try {
      setRes(await notifyApi.test(testChannel(seg, c.configured), to.trim() || undefined));
    } catch (e) {
      setRes({ ok: false, detail: (e as Error).message });
    }
  };
  return (
    <form className="line" onSubmit={(e) => { e.preventDefault(); void run(); }}>
      {seg !== "webhook" && (
        <Input mono className="sm to" value={to} onChange={(e) => setTo(e.target.value)} aria-label="Send the test to"
          placeholder={seg === "email" ? "To address" : "To number"} />
      )}
      <Button small type="submit" disabled={res === "running" || (seg === "email" && !to.trim())}>Test</Button>
      {res && res !== "running" && <span className={"mono " + (res.ok ? "ok" : "err")}>{res.detail}</span>}
    </form>
  );
}
