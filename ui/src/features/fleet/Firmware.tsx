import { useEffect, useState } from "react";
import { fleet, type Firmware, type FirmwarePolicy, type Rollout, type SigningKey, type UnitFirmware } from "../../api/fleet";
import { useStore } from "../../store";
import { useCan } from "../../store/me";
import { Button, Input, Segmented } from "../../ui";
import { FilePick } from "../../ui/FilePick";
import { FW_WORDS, stageLine } from "./model";

// Firmware: the policy, signed releases, rollouts in stages, where each collar stands, and
// the keys this server trusts.
export function FirmwareTab() {
  const owner = useCan("owner");
  const herdId = useStore((s) => s.herdId);
  const [policy, setPolicy] = useState<FirmwarePolicy>();
  const [releases, setReleases] = useState<Firmware[]>();
  const [rollouts, setRollouts] = useState<Rollout[]>();
  const [units, setUnits] = useState<UnitFirmware[]>();
  const [keys, setKeys] = useState<SigningKey[]>();
  const [err, setErr] = useState<string>();
  const fail = (e: unknown) => setErr((e as Error).message);
  const load = () => {
    fleet.policy().then(setPolicy, fail);
    fleet.firmware().then(setReleases, fail);
    fleet.rollouts().then(setRollouts, fail);
    fleet.fwUnits({ herd_id: herdId }).then(setUnits, fail);
    fleet.keys().then(setKeys, fail);
  };
  useEffect(load, [herdId]); // eslint-disable-line react-hooks/exhaustive-deps
  const act = async (f: () => Promise<unknown>) => {
    setErr(undefined);
    try {
      await f();
      load();
    } catch (e) {
      fail(e);
    }
  };
  return (
    <>
      {err && <p className="err">{err}</p>}
      {policy && (
        <div className="line fline">
          <Segmented label="Updates" value={policy.mode} onChange={(mode) => act(async () => setPolicy(await fleet.savePolicy({ ...policy, mode })))}
            options={[{ value: "auto", label: "Automatic" }, { value: "paused", label: "Paused" }]} />
          <Segmented label="Channel" value={policy.channel} onChange={(channel) => act(async () => setPolicy(await fleet.savePolicy({ ...policy, channel })))}
            options={[{ value: "stable", label: "Stable" }, { value: "beta", label: "Beta too" }]} />
          <span className="dim">never below {Math.round(policy.battery_floor * 100)} % battery; the first herd after {policy.wave_hours} h, every herd after {policy.wave_hours * 3} h</span>
        </div>
      )}
      <h3>Releases</h3>
      {owner && <AddRelease onAdded={load} />}
      {releases && (releases.length === 0 ? <p className="dim">No releases yet.</p> : (
        <table className="tbl">
          <thead><tr><th>version</th><th>classes</th><th>boards</th><th>signed by</th><th>image</th><th>channel</th><th></th></tr></thead>
          <tbody>
            {releases.map((f) => (
              <tr key={f.id}>
                <td>{f.version}</td><td>{f.classes.join(" ")}</td><td>{f.hw_revs.join(" ")}</td>
                <td>{f.signer ? `${f.signer.name}` : f.signer_kid}</td>
                <td>{f.withdrawn_at ? "withdrawn" : f.has_image ? `${Math.round(f.size / 1024)} KB` : owner ? <FilePick onPick={(file) => act(() => fleet.uploadImage(f.id, file))}>Upload</FilePick> : "not here yet"}</td>
                <td>{f.channel}</td>
                <td>{!f.withdrawn_at && <span className="line">
                  <Button small kind="plain" onClick={() => act(() => fleet.startRollout(f.id))}>Roll out</Button>
                  {owner && <Button small kind="plain" className="danger" onClick={() => act(() => fleet.withdraw(f.id))}>Withdraw</Button>}
                </span>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ))}
      {rollouts && rollouts.length > 0 && (
        <>
          <h3>Rollouts</h3>
          <table className="tbl">
            <thead><tr><th>version</th><th>stage</th><th>state</th><th>confirmed</th><th>installing</th><th>held</th><th>no OTA</th><th></th></tr></thead>
            <tbody>
              {rollouts.map((r) => (
                <tr key={r.id}>
                  <td>{r.version}</td><td>{stageLine(r)}</td>
                  <td className={r.state === "paused" ? "err wrap" : "wrap"}>{r.state}{r.paused_reason ? `: ${r.paused_reason}` : ""}</td>
                  <td>{r.counts.confirmed}/{r.counts.targeted}</td><td>{r.counts.offered}</td><td>{r.counts.held + r.counts.waiting}</td><td>{r.counts.unsupported}</td>
                  <td>{(r.state === "active" || r.state === "paused") && <span className="line">
                    {r.state === "active" ? <Button small kind="plain" onClick={() => act(() => fleet.rolloutAct(r.id, "pause"))}>Pause</Button>
                      : <Button small kind="plain" onClick={() => act(() => fleet.rolloutAct(r.id, "resume"))}>Resume</Button>}
                    <Button small kind="plain" className="danger" onClick={() => act(() => fleet.rolloutAct(r.id, "cancel"))}>Cancel</Button>
                  </span>}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
      {units && units.length > 0 && (
        <>
          <h3>Collars in this herd</h3>
          <table className="tbl">
            <thead><tr><th>collar</th><th>runs</th><th>to</th><th>state</th><th></th></tr></thead>
            <tbody>
              {units.map((u) => (
                <tr key={u.collar_id}><td>{u.name}</td><td>{u.current ?? ""}</td><td>{u.target ?? ""}</td><td>{FW_WORDS[u.state]}</td><td className="wrap dim">{u.reason ?? ""}</td></tr>
              ))}
            </tbody>
          </table>
        </>
      )}
      <h3>Signing keys</h3>
      {keys && (keys.length === 0 ? <p className="dim">This server trusts no firmware key yet. OpenCollar's release key comes with the first signed release; a builder who signs their own firmware enrols theirs here.</p> : (
        <table className="tbl">
          <thead><tr><th>kid</th><th>name</th><th>for</th><th>from</th><th></th></tr></thead>
          <tbody>
            {keys.map((k) => (
              <tr key={k.kid}>
                <td>{k.kid}</td><td>{k.name}</td><td>{k.purpose === "image" ? "firmware" : "factory records"}</td><td>{k.kind === "vendor" ? "built in" : (k.created_at ?? "").slice(0, 10)}</td>
                <td>{k.revoked_at ? "revoked" : owner && k.kind === "owner" && <Button small kind="plain" className="danger" onClick={() => act(() => fleet.revokeKey(k.kid))}>Revoke</Button>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ))}
      {owner && <AddKey onAdded={load} />}
    </>
  );
}

function AddRelease({ onAdded }: { onAdded: () => void }) {
  const [manifest, setManifest] = useState("");
  const [beta, setBeta] = useState(false);
  const [err, setErr] = useState<string>();
  const add = async () => {
    setErr(undefined);
    try {
      await fleet.addRelease({ manifest: manifest.trim(), channel: beta ? "beta" : "stable" });
      setManifest("");
      onAdded();
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <>
      <form className="line fline" onSubmit={(e) => { e.preventDefault(); void add(); }}>
        <Input mono className="sm grow" placeholder="Signed release manifest, hex" value={manifest} onChange={(e) => setManifest(e.target.value)} aria-label="Release manifest" />
        <Segmented label="Channel" value={beta ? "beta" : "stable"} onChange={(v) => setBeta(v === "beta")} options={[{ value: "stable", label: "Stable" }, { value: "beta", label: "Beta" }]} />
        <Button small type="submit" disabled={!manifest.trim()}>Add</Button>
      </form>
      {err && <p className="err">{err}</p>}
    </>
  );
}

function AddKey({ onAdded }: { onAdded: () => void }) {
  const [name, setName] = useState("");
  const [key, setKey] = useState("");
  const [purpose, setPurpose] = useState<"image" | "maker">("image");
  const [err, setErr] = useState<string>();
  const add = async () => {
    setErr(undefined);
    try {
      await fleet.addKey({ name: name.trim(), public_key: key.trim(), purpose });
      setName("");
      setKey("");
      onAdded();
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <>
      <form className="line fline" onSubmit={(e) => { e.preventDefault(); void add(); }}>
        <Input className="sm" placeholder="Name" value={name} onChange={(e) => setName(e.target.value)} aria-label="Key name" />
        <Input mono className="sm grow" placeholder="Ed25519 public key, base64" value={key} onChange={(e) => setKey(e.target.value)} aria-label="Public key" />
        <Segmented label="Signs" value={purpose} onChange={setPurpose} options={[{ value: "image", label: "Firmware" }, { value: "maker", label: "Factory records" }]} />
        <Button small type="submit" disabled={!name.trim() || !key.trim()}>Enrol</Button>
      </form>
      {err && <p className="err">{err}</p>}
    </>
  );
}
