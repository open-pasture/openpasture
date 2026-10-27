import { useEffect, useMemo, useRef, useState } from "react";
import { SIMULATED } from "../../sim";
import type { Animal, Collar, Polygon } from "../../api";
import { esrv, type CollarSlots } from "../../api/e-srv";
import { fleet, wireLog, type CollarConsole, type WireEntry } from "../../api/fleet";
import { signedDistance } from "../../geo";
import { outOf, store, useStore } from "../../store";
import { Copy } from "../../ui";
import { useUnits } from "../../units";
import { age, useNow } from "../../util";
import { addWire, duration, fenceLine, FW_WORDS, qualityLine, wireLine } from "./model";

// Animal page, managers and up: one collar's console for the bench and the field. First
// contact, the fence live, raw health, receiver quality, every boundary it holds or refused,
// acks with their codes, cues (with or without an animal), firmware, its link, and the wire log.
export function Console({ collar }: { animal?: Animal; collar?: Collar }) {
  const id = collar?.id;
  const [c, setC] = useState<CollarConsole>();
  const [slots, setSlots] = useState<CollarSlots>();
  const [err, setErr] = useState<string>();
  const [bump, setBump] = useState(0);
  // Cues and acks change between reports; one refetch a second at most.
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => {
    if (!id) return;
    const soon = () => {
      clearTimeout(timer.current);
      timer.current = setTimeout(() => setBump((n) => n + 1), 1000);
    };
    const offs = [store.on("cue", (e) => e.collar_id === id && soon()), store.on("ack", (e) => e.collar_id === id && soon())];
    return () => {
      offs.forEach((f) => f());
      clearTimeout(timer.current);
    };
  }, [id]);
  useEffect(() => {
    if (!id) return;
    let live = true;
    fleet.console(id).then((v) => { if (live) { setC(v); setErr(undefined); } }, (e) => live && setErr((e as Error).message));
    esrv.collarSlots(id).then((v) => live && setSlots(v), (e) => live && setErr((e as Error).message));
    return () => void (live = false);
  }, [id, collar?.last_seen, collar?.boundary_version, bump]);
  if (!collar) return null;
  if (err) return <section className="fcon"><h3>Console</h3><p className="err">{err}</p></section>;
  if (!c) return null;
  return (
    <section className="fcon">
      <h3>Console{collar.simulated && <i className="sim">{SIMULATED}</i>}</h3>
      <Contact c={c} collar={collar} />
      <Fence collar={collar} />
      <Health c={c} />
      {slots && <Slots s={slots} />}
      <Acks c={c} />
      <Cues c={c} />
      <Wire collarId={collar.id} />
    </section>
  );
}

function Contact({ c, collar }: { c: CollarConsole; collar: Collar }) {
  const now = useNow(1000);
  if (c.contact.state === "waiting") {
    return (
      <div className="fwaiting">
        <p>Waiting for the first report from {collar.name}, added {age(c.contact.since, now)} ago.</p>
        {c.contact.endpoint ? <Copy label="reports to" value={c.contact.endpoint} /> : <p className="dim">{c.contact.why}</p>}
      </div>
    );
  }
  const fw = c.firmware;
  return (
    <ul className="kv">
      <li><span>heard</span><b>{age(collar.last_seen ?? c.contact.last_seen, now)} ago{collar.fw ? `, fw ${collar.fw}` : ""}{c.contact.latency_s !== undefined ? `, newest fix ${duration(c.contact.latency_s)} before` : ""}</b></li>
      {c.unit && <li><span>unit</span><b>{c.unit.serial} {c.unit.device_class} {c.unit.hw_rev}</b></li>}
      {fw && <li><span>firmware</span><b>{fw.current ?? "?"}{fw.target && fw.target !== fw.current ? ` → ${fw.target}` : ""}  {FW_WORDS[fw.state]}{fw.reason && fw.state !== "unsupported" ? `: ${fw.reason}` : ""}</b></li>}
      {c.network && c.network.role !== "direct" && <li><span>link</span><b>{c.network.role}  {qualityLine(c.network.quality, c.network.rssi_dbm, c.network.snr_db)}{c.network.via ? `  via ${c.network.via}` : ""}</b></li>}
      {collar.caps && collar.caps.length > 0 && <li><span>caps</span><b>{collar.caps.join(" ")}</b></li>}
    </ul>
  );
}

// The collar's signed distance to the fence it is held by: its own (an escape) or the herd's.
function Fence({ collar }: { collar: Collar }) {
  const b = useStore((s) => s.boundary[collar.herd_id]);
  const live = useStore((s) => s.collars.find((x) => x.id === collar.id));
  const fmt = useUnits();
  const poly: Polygon | undefined = useMemo(() => outOf(b).find((e) => e.collar_id === collar.id)?.geometry ?? b?.active?.geometry, [b, collar.id]);
  const fix = (live ?? collar).last_fix;
  if (!poly || !fix) return null;
  const line = fenceLine(signedDistance(fix.point, poly), fmt.len);
  return line ? <p className={"ffence" + (line.startsWith("outside") ? " err" : "")}>{line}</p> : null;
}

function Health({ c }: { c: CollarConsole }) {
  const h = c.health;
  const f = c.fix;
  if (!h && !f) return null;
  const rows: [string, string][] = [];
  if (h) {
    const batt = [h.battery !== undefined ? `${Math.round(h.battery * 100)} %` : "", h.battery_v !== undefined ? `${h.battery_v.toFixed(2)} V` : "", h.charging ? "charging" : ""].filter(Boolean).join("  ");
    if (batt) rows.push(["battery", batt]);
    if (h.temp_c !== undefined) rows.push(["temperature", `${h.temp_c.toFixed(1)} °C`]);
    if (h.uptime_s !== undefined || h.reset) rows.push(["up", [h.uptime_s !== undefined ? duration(h.uptime_s) : "", h.reset ? `last reset ${h.reset}` : ""].filter(Boolean).join("  ")]);
    if (h.cell) rows.push(["cell", [h.cell.mode, h.cell.band !== undefined ? `band ${h.cell.band}` : "", h.cell.rsrp_dbm !== undefined ? `RSRP ${h.cell.rsrp_dbm}` : "", h.cell.rsrq_db !== undefined ? `RSRQ ${h.cell.rsrq_db}` : "", h.cell.snr_db !== undefined ? `SNR ${h.cell.snr_db}` : "", h.cell.cell_id ?? ""].filter(Boolean).join("  ")]);
    if (h.still_s !== undefined || h.tilt_deg !== undefined) rows.push(["motion", [h.still_s !== undefined ? `still ${duration(h.still_s)}` : "", h.tilt_deg !== undefined ? `tilt ${Math.round(h.tilt_deg)}°` : ""].filter(Boolean).join("  ")]);
    if (h.fix_attempts !== undefined) rows.push(["fixes", `${h.fix_ok ?? 0} of ${h.fix_attempts}`]);
  }
  if (f) rows.push(["last fix", [`${f.sats} sats`, f.cn0 !== undefined ? `C/N0 ${f.cn0}` : "", f.hdop !== undefined ? `HDOP ${f.hdop}` : "", `±${f.accuracy_m} m`, f.ttf_s !== undefined ? `TTFF ${f.ttf_s} s` : ""].filter(Boolean).join("  ")]);
  return <ul className="kv">{rows.map(([k, v]) => <li key={k}><span>{k}</span><b>{v}</b></li>)}</ul>;
}

// Every boundary it holds or refused, with codes; its limits and its config.
function Slots({ s }: { s: CollarSlots }) {
  const when = (iso?: string) => (iso ? new Date(iso).toLocaleString([], { weekday: "short", hour: "2-digit", minute: "2-digit" }) : "");
  return (
    <ul className="kv">
      {s.slots.map((x) => (
        <li key={x.version}><span>v{x.version}{x.copy_of ? ` (copy of v${x.copy_of})` : ""}</span>
          <b className={x.status === "rejected" ? "err" : ""}>{x.status === "received" ? `staged ${when(x.effective_at)}` : x.status}{x.code ? ` ${x.code}` : ""}</b></li>
      ))}
      <li><span>limits</span><b>{s.limits.outer} outer / {s.limits.holes} holes / {s.limits.slots} slots</b></li>
      {s.config && <li><span>config</span><b className={s.config.refused ? "err" : ""}>v{s.config.version}  report {s.config.report_s} s  poll {s.config.poll_s} s{s.config.fast_until ? `  fast until ${when(s.config.fast_until)}` : ""}{s.config.refused ? "  refused" : ""}</b></li>}
    </ul>
  );
}

function Acks({ c }: { c: CollarConsole }) {
  if (!c.acks.length) return null;
  return (
    <details className="flog">
      <summary>Acks <span className="dim">{c.acks.length}</span></summary>
      <table className="tbl"><tbody>
        {c.acks.map((a, i) => (
          <tr key={i}><td>{a.at.slice(0, 19).replace("T", " ")}</td><td>v{a.version}</td><td className={a.status === "rejected" ? "err" : ""}>{a.status}{a.code ? ` ${a.code}` : ""}</td><td className="wrap dim">{a.reason ?? ""}</td></tr>
        ))}
      </tbody></table>
    </details>
  );
}

function Cues({ c }: { c: CollarConsole }) {
  const now = useNow(1000);
  const fmt = useUnits();
  if (!c.cues.length) return <p className="dim">No cues yet.</p>;
  const last = c.cues[0];
  return (
    <details className="flog">
      <summary>Cues <span className="dim">last {age(last.at, now)} ago, {last.kind} level {last.level}, margin {last.margin_m < 0 ? "−" : ""}{fmt.len(Math.abs(last.margin_m))}</span></summary>
      <table className="tbl"><tbody>
        {c.cues.map((q, i) => (
          <tr key={i}><td>{q.at.slice(0, 19).replace("T", " ")}</td><td>{q.kind}</td><td>level {q.level}</td><td>{q.margin_m.toFixed(1)} m</td><td>{q.ring !== undefined ? `ring ${q.ring}` : ""}</td><td>{q.boundary_version !== undefined ? `v${q.boundary_version}` : ""}</td></tr>
        ))}
      </tbody></table>
    </details>
  );
}

// The wire log from the collar bridge: every exchange on /collar/v1, live.
function Wire({ collarId }: { collarId: string }) {
  const [entries, setEntries] = useState<WireEntry[] | null>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    let live = true;
    wireLog(collarId).then((v) => live && setEntries(v), (e) => live && setErr((e as Error).message));
    const off = store.on("collar_wire", (e) => {
      if (e.collar_id === collarId) setEntries((cur) => (cur === null ? cur : addWire(cur ?? [], e.entry)));
    });
    return () => {
      live = false;
      off();
    };
  }, [collarId]);
  if (err) return <p className="err">Wire log: {err}</p>;
  if (entries === undefined) return null;
  if (entries === null) return <p className="dim">This server keeps no wire log.</p>;
  return (
    <details className="flog" open={entries.length > 0}>
      <summary>Wire <span className="dim">{entries.length ? `${entries.length} exchanges` : "nothing yet"}</span></summary>
      {entries.length > 0 && <pre className="fwire">{entries.slice().reverse().map((e, i) => (
        <span key={i} className={e.status !== undefined && e.status >= 400 ? "err" : ""}>{wireLine(e)}{"\n"}</span>
      ))}</pre>}
    </details>
  );
}
