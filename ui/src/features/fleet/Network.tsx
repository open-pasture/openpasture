import { useEffect, useState } from "react";
import { fleet, type FixedAnchor, type HerdNetwork } from "../../api/fleet";
import { useStore } from "../../store";
import { Button, Input } from "../../ui";
import { age, useNow } from "../../util";
import { qualityLine } from "./model";

// Network: the herd's anchors (and which one is its link now), every collar's link, the
// nodes not heard lately, and anchors on posts.
export function NetworkTab() {
  const herdId = useStore((s) => s.herdId);
  const herds = useStore((s) => s.state?.herds ?? []);
  const now = useNow(10_000);
  const [n, setN] = useState<HerdNetwork>();
  const [fixed, setFixed] = useState<FixedAnchor[]>();
  const [err, setErr] = useState<string>();
  const load = () => {
    if (herdId) fleet.network(herdId).then(setN, (e) => setErr((e as Error).message));
    fleet.anchors().then(setFixed, (e) => setErr((e as Error).message));
  };
  useEffect(load, [herdId]); // eslint-disable-line react-hooks/exhaustive-deps
  if (err) return <p className="err">{err}</p>;
  if (!herdId) return <p className="dim">Pick a herd.</p>;
  const name = (id?: string) => (id ? n?.anchors.find((a) => a.id === id)?.name ?? n?.members.find((m) => m.collar_id === id)?.name ?? id : "");
  return (
    <>
      {n && (
        <>
          <p className="dim">{herds.find((h) => h.id === herdId)?.name}: {n.active_anchor ? `${name(n.active_anchor)} is the link now` : n.anchors.length ? "no anchor beaconing" : "no anchors; its collars talk to the server themselves"}</p>
          {n.anchors.length > 0 && (
            <table className="tbl">
              <thead><tr><th>anchor</th><th>on</th><th>beacon</th><th>battery</th><th>LTE</th></tr></thead>
              <tbody>
                {n.anchors.map((a) => (
                  <tr key={a.id}><td>{a.name}{a.active ? " (link)" : ""}</td><td>{a.source === "fixed" ? "post" : "collar"}</td>
                    <td>{a.last_beacon_at ? `${age(a.last_beacon_at, now)} ago` : ""}</td><td>{a.battery !== undefined ? `${Math.round(a.battery * 100)} %` : ""}</td>
                    <td>{a.lte_rsrp_dbm !== undefined ? `${a.lte_rsrp_dbm} dBm` : ""}</td></tr>
                ))}
              </tbody>
            </table>
          )}
          <table className="tbl">
            <thead><tr><th>collar</th><th>part</th><th>link</th><th>via</th><th>heard</th></tr></thead>
            <tbody>
              {n.members.map((m) => (
                <tr key={m.collar_id}><td>{m.name}</td><td>{m.role}</td><td className={m.quality === "lost" || m.quality === "poor" ? "err" : ""}>{qualityLine(m.quality, m.rssi_dbm, m.snr_db)}</td>
                  <td>{name(m.via)}</td><td>{m.last_heard_at ? `${age(m.last_heard_at, now)} ago` : "never"}</td></tr>
              ))}
            </tbody>
          </table>
          {n.gaps.length > 0 && (
            <>
              <h3>Not heard lately</h3>
              <ul className="kv">
                {n.gaps.map((g) => <li key={g.collar_id}><span>{g.name}</span><b>{age(g.since, now)} ago{g.point ? `  at ${g.point[1].toFixed(5)}, ${g.point[0].toFixed(5)}` : ""}</b></li>)}
              </ul>
            </>
          )}
        </>
      )}
      <h3>Anchors on posts</h3>
      {fixed && fixed.length > 0 && (
        <table className="tbl"><tbody>{fixed.map((a) => <tr key={a.id}><td>{a.name}</td><td>{a.point[1].toFixed(5)}, {a.point[0].toFixed(5)}</td><td>{a.serial ?? ""}</td></tr>)}</tbody></table>
      )}
      <AddAnchor onAdded={load} />
    </>
  );
}

function AddAnchor({ onAdded }: { onAdded: () => void }) {
  const [name, setName] = useState("");
  const [at, setAt] = useState("");
  const [err, setErr] = useState<string>();
  const add = async () => {
    // "42.03012, -93.62044" as a person reads it: latitude first.
    const m = at.match(/^\s*(-?\d+(?:\.\d+)?)\s*[, ]\s*(-?\d+(?:\.\d+)?)\s*$/);
    if (!m) return setErr("Where is it? Latitude, longitude.");
    setErr(undefined);
    try {
      await fleet.addAnchor({ name: name.trim(), point: [Number(m[2]), Number(m[1])] });
      setName("");
      setAt("");
      onAdded();
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <>
      <form className="line fline" onSubmit={(e) => { e.preventDefault(); void add(); }}>
        <Input className="sm" placeholder="Name" value={name} onChange={(e) => setName(e.target.value)} aria-label="Anchor name" />
        <Input mono className="sm grow" placeholder="Latitude, longitude" value={at} onChange={(e) => setAt(e.target.value)} aria-label="Where" />
        <Button small type="submit" disabled={!name.trim() || !at.trim()}>Add</Button>
      </form>
      {err && <p className="err">{err}</p>}
    </>
  );
}
