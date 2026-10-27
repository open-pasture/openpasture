import { useEffect, useState } from "react";
import { fleet, type Claimed, type Provisioning, type TestRun, type Unit, type UnitDetail } from "../../api/fleet";
import { store, useStore } from "../../store";
import { useCan } from "../../store/me";
import { Button, Copy, Input } from "../../ui";
import { grouped, readClaim, STATE_WORDS, stepLine } from "./model";

const STATES = ["", "built", "tested", "provisioned", "claimed", "in_service", "rma", "retired"] as const;

// Units: every unit by serial, newest first; one unit's tests and history; claiming one.
export function Units({ serial }: { serial?: string }) {
  if (serial) return <UnitPage serial={serial} />;
  return <UnitList />;
}

function UnitList() {
  const [units, setUnits] = useState<Unit[]>();
  const [state, setState] = useState("");
  const [q, setQ] = useState("");
  const [err, setErr] = useState<string>();
  const load = () => fleet.units({ state: state || undefined, q: q || undefined }).then((u) => { setUnits(u); setErr(undefined); }, (e) => setErr((e as Error).message));
  useEffect(() => {
    void load();
    // A station working while this is open: its units show up as they come.
    const off = store.on("fleet_unit", () => void load());
    return off;
  }, [state, q]); // eslint-disable-line react-hooks/exhaustive-deps
  return (
    <>
      <Claim onClaimed={() => void load()} />
      <div className="line fline">
        <Input className="sm grow" placeholder="Serial, device id or radio id" value={q} onChange={(e) => setQ(e.target.value)} aria-label="Find a unit" />
        <select className="input sm" value={state} onChange={(e) => setState(e.target.value)} aria-label="State">
          {STATES.map((s) => <option key={s} value={s}>{s ? STATE_WORDS[s] : "every state"}</option>)}
        </select>
      </div>
      {err && <p className="err">{err}</p>}
      {units && (units.length === 0 ? <p className="dim">No units yet. A station registers them as it tests them.</p> : (
        <table className="tbl">
          <thead><tr><th>serial</th><th>class</th><th>board</th><th>firmware</th><th>state</th><th>last test</th><th>station</th><th>collar</th></tr></thead>
          <tbody>
            {units.map((u) => (
              <tr key={u.serial} className="click" onClick={() => (location.hash = `#/fleet/units/${encodeURIComponent(u.serial)}`)}>
                <td>{u.serial}</td><td>{u.device_class}</td><td>{u.hw_rev}</td><td>{u.fw_version}</td><td>{STATE_WORDS[u.state]}</td>
                <td className={u.last_test?.result === "fail" ? "err" : ""}>{u.last_test ? `${u.last_test.result} ${u.last_test.finished_at.slice(0, 16).replace("T", " ")}` : ""}</td>
                <td>{u.station ?? ""}</td><td>{u.collar_id ? collarName(u.collar_id) : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ))}
    </>
  );
}

function collarName(id: string) {
  return store.get().collars.find((c) => c.id === id)?.name ?? id;
}

// Claim a unit into a herd: its card scanned (OC2:…) or its claim code typed.
function Claim({ onClaimed }: { onClaimed: () => void }) {
  const herds = useStore((s) => s.state?.herds ?? []);
  const current = useStore((s) => s.herdId);
  const [text, setText] = useState("");
  const [herd, setHerd] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  const [done, setDone] = useState<Claimed>();
  const herdId = herd ?? current ?? herds[0]?.id;
  const claim = async () => {
    const what = readClaim(text);
    if (!herdId || (!what.card && !what.claim_code)) return;
    setBusy(true);
    setErr(undefined);
    try {
      setDone(await fleet.claim({ ...what, herd_id: herdId }));
      setText("");
      onClaimed();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="fclaim">
      <form className="line" onSubmit={(e) => { e.preventDefault(); void claim(); }}>
        <Input mono className="sm grow" placeholder="Scan the card or type the claim code" value={text} onChange={(e) => setText(e.target.value)} aria-label="Card or claim code" />
        <select className="input sm" value={herdId ?? ""} onChange={(e) => setHerd(e.target.value)} aria-label="Herd">
          {herds.map((h) => <option key={h.id} value={h.id}>{h.name}</option>)}
        </select>
        <Button small type="submit" disabled={busy || !text.trim() || !herdId}>Claim</Button>
      </form>
      {err && <p className="err">{err}</p>}
      {done && (
        <div className="fdone">
          <p>{done.unit.serial} is {done.collar.name} in {herds.find((h) => h.id === done.collar.herd_id)?.name ?? "the herd"}.</p>
          <p className="dim">For firmware that speaks protocol v1, set it up with this once:</p>
          {done.v1.line ? <Copy label="console" value={done.v1.line} secret /> : <p className="dim">{done.v1.why}</p>}
        </div>
      )}
    </div>
  );
}

function UnitPage({ serial }: { serial: string }) {
  const [d, setD] = useState<UnitDetail>();
  const [err, setErr] = useState<string>();
  const [prov, setProv] = useState<Provisioning>();
  const [busy, setBusy] = useState(false);
  const manager = useCan("manager");
  const load = () => fleet.unit(serial).then(setD, (e) => setErr((e as Error).message));
  useEffect(() => {
    void load();
    const off = store.on("fleet_unit", (e) => e.unit.serial === serial && void load());
    return off;
  }, [serial]); // eslint-disable-line react-hooks/exhaustive-deps
  if (err) return <p className="err">{err}</p>;
  if (!d) return null;
  const u = d.unit;
  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setErr(undefined);
    try {
      await f();
      await load();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const tested = u.last_test?.result === "pass";
  return (
    <div className="funit">
      <p><a href="#/fleet/units" className="dim">Units</a> / {u.serial}</p>
      <ul className="kv">
        <li><span>class, board</span><b>{u.device_class} {u.hw_rev}</b></li>
        <li><span>firmware as built</span><b>{u.fw_version}</b></li>
        <li><span>state</span><b>{STATE_WORDS[u.state]}{u.origin === "card" ? ", from its card" : ""}</b></li>
        <li><span>device id</span><b>{u.device_id}</b></li>
        {u.maker && <li><span>made by</span><b>{u.maker}{u.maker_verified ? "" : ", unverified"}{u.made_at ? ` ${u.made_at.slice(0, 10)}` : ""}</b></li>}
        {u.claim_code && <li><span>claim code</span><b>{grouped(u.claim_code)}</b></li>}
        {u.radios.lora_eui && <li><span>LoRa EUI</span><b>{u.radios.lora_eui}</b></li>}
        {u.radios.imei && <li><span>IMEI</span><b>{u.radios.imei}</b></li>}
        {u.radios.ble_addr && <li><span>BLE</span><b>{u.radios.ble_addr}</b></li>}
        {u.collar_id && <li><span>collar</span><b>{collarName(u.collar_id)}</b></li>}
      </ul>
      {manager && (
        <div className="line">
          {(tested || u.state === "provisioned") && <Button small disabled={busy} onClick={() => act(async () => setProv(await fleet.provision(serial)))}>Provision</Button>}
          {u.claim_code && <a className="btn quiet sm" href={`#/print/fleet-labels/${encodeURIComponent(serial)}`} target="_blank" rel="noreferrer">Label</a>}
          {u.state !== "rma" && u.state !== "retired" && <Button small kind="plain" disabled={busy} onClick={() => act(() => fleet.setState(serial, "rma"))}>Send back (RMA)</Button>}
          {u.state !== "retired" && <Button small kind="plain" className="danger" disabled={busy} onClick={() => act(() => fleet.setState(serial, "retired"))}>Retire</Button>}
        </div>
      )}
      {prov && (
        <div className="fdone">
          <Copy label="card" value={prov.card} />
          <p className="dim">claim code {grouped(prov.claim_code)} · {prov.link.transport} {prov.link.lora_region}{prov.factory_record ? ` · factory record signed by ${prov.factory_record.maker_kid}` : ""}</p>
        </div>
      )}
      <h3>Tests</h3>
      {d.runs.length === 0 ? <p className="dim">Not tested yet.</p> : d.runs.map((r) => <Run key={r.id} run={r} />)}
      <h3>History</h3>
      <table className="tbl">
        <tbody>
          {d.events.map((e, i) => (
            <tr key={i}><td>{e.at.slice(0, 19).replace("T", " ")}</td><td>{e.kind.replace("_", " ")}</td><td>{e.by ?? ""}</td><td className="wrap">{detailText(e.detail)}</td></tr>
          ))}
        </tbody>
      </table>
      {d.keys.length > 1 && (
        <>
          <h3>Device keys</h3>
          <table className="tbl"><tbody>{d.keys.map((k) => <tr key={k.device_kid}><td>{k.device_kid}</td><td>{k.from.slice(0, 10)}</td><td>{k.to ? `until ${k.to.slice(0, 10)}` : "current"}</td></tr>)}</tbody></table>
        </>
      )}
    </div>
  );
}

function detailText(d: Record<string, unknown>) {
  return Object.entries(d).filter(([, v]) => v !== null && v !== undefined && v !== "" && !(Array.isArray(v) && v.length === 0)).map(([k, v]) => `${k} ${Array.isArray(v) ? v.join(" ") : String(v)}`).join("  ");
}

function Run({ run }: { run: TestRun }) {
  const [open, setOpen] = useState(run.result === "fail");
  return (
    <div className="frun">
      <button type="button" className="line" onClick={() => setOpen(!open)} aria-expanded={open}>
        <span className={run.result === "fail" ? "err" : "ok"}>{run.result}</span>
        <span className="mono">{run.finished_at.slice(0, 19).replace("T", " ")}</span>
        <span className="dim">{run.plan_id} v{run.plan_version}{run.station ? ` · ${run.station}` : ""}{run.failed.length ? ` · ${run.failed.join(", ")}` : ""}</span>
      </button>
      {open && (
        <table className="tbl">
          <tbody>
            {run.steps.map((s) => (
              <tr key={s.name}>
                <td>{s.name}</td>
                <td className={s.verdict === "fail" || (s.required && (s.verdict === "missing" || s.verdict === "skip")) ? "err" : s.verdict === "pass" ? "ok" : ""}>{s.verdict}</td>
                <td className="wrap">{stepLine(s)}{s.detail ? `  ${s.detail}` : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </div>
  );
}
