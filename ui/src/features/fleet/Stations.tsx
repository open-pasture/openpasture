import { useEffect, useState } from "react";
import { fleet, type ProvisioningSettings, type Station } from "../../api/fleet";
import { Button, Copy, Input } from "../../ui";
import { age, useNow } from "../../util";

const REGIONS = ["US915", "AU915", "EU868", "AS923", "IN865", "KR920", "CN470"];

// Stations (owner): a token per fixture PC, which can only register, test, provision and
// print labels; and what provisioning writes (LoRa region, the maker in factory records).
export function Stations() {
  const now = useNow(30_000);
  const [list, setList] = useState<Station[]>();
  const [name, setName] = useState("");
  const [made, setMade] = useState<{ station: Station; token: string }>();
  const [prov, setProv] = useState<ProvisioningSettings>();
  const [err, setErr] = useState<string>();
  const fail = (e: unknown) => setErr((e as Error).message);
  const load = () => {
    fleet.stations().then(setList, fail);
    fleet.provisioning().then(setProv, fail);
  };
  useEffect(load, []);
  const add = async () => {
    setErr(undefined);
    try {
      setMade(await fleet.addStation(name.trim()));
      setName("");
      load();
    } catch (e) {
      fail(e);
    }
  };
  const saveProv = async (next: { lora_region: string; maker?: string }) => {
    setErr(undefined);
    try {
      setProv(await fleet.saveProvisioning(next));
    } catch (e) {
      fail(e);
    }
  };
  return (
    <>
      {err && <p className="err">{err}</p>}
      <form className="line fline" onSubmit={(e) => { e.preventDefault(); void add(); }}>
        <Input className="sm grow" placeholder="Station name, e.g. bench-1" value={name} onChange={(e) => setName(e.target.value)} aria-label="Station name" />
        <Button small type="submit" disabled={!name.trim()}>Add station</Button>
      </form>
      {made && (
        <div className="fdone">
          <p>{made.station.name}'s token, shown once. Put it in the station's config.</p>
          <Copy label="token" value={made.token} secret />
        </div>
      )}
      {list && list.length > 0 && (
        <table className="tbl">
          <thead><tr><th>station</th><th>last used</th><th>added</th><th></th></tr></thead>
          <tbody>
            {list.map((s) => (
              <tr key={s.id}>
                <td>{s.name}</td><td>{s.last_used_at ? `${age(s.last_used_at, now)} ago` : "never"}</td><td>{s.created_at.slice(0, 10)}</td>
                <td>{s.revoked_at ? "revoked" : <Button small kind="plain" className="danger" onClick={async () => { try { await fleet.revokeStation(s.id); load(); } catch (e) { fail(e); } }}>Revoke</Button>}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {prov && (
        <>
          <h3>Provisioning</h3>
          <div className="line fline">
            <select className="input sm" value={prov.lora_region} aria-label="LoRa region" onChange={(e) => void saveProv({ lora_region: e.target.value, maker: prov.maker })}>
              {REGIONS.map((r) => <option key={r} value={r}>{r}</option>)}
            </select>
            <Input className="sm grow" placeholder="Maker, as it goes in factory records (the farm's name when blank)" defaultValue={prov.maker ?? ""} aria-label="Maker"
              onBlur={(e) => {
                const maker = e.target.value.trim() || undefined;
                if (maker !== prov.maker) void saveProv({ lora_region: prov.lora_region, maker });
              }} />
          </div>
          <p className="dim">This server signs its units' factory records with maker key {prov.maker_kid}. Farms that buy your units enrol it to see those records as verified.</p>
          <Copy label="maker key" value={prov.maker_key} />
        </>
      )}
    </>
  );
}
