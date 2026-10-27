import { useEffect, useState } from "react";
import { fleet, type FirmwarePolicy, type UnitFirmware } from "../../api/fleet";
import { Check } from "../../ui/Check";
import { Segmented } from "../../ui";
import { advanced } from "./advanced";
import { FW_WORDS } from "./model";
import { showFleet } from "./view";

// Settings > Advanced: the Fleet view in the rail. Turning it off reloads, which takes it out.
export function AdvancedToggle() {
  const on = advanced.use((v) => v);
  return (
    <div className="line">
      <Check checked={on} onChange={(next) => {
        advanced.set(next);
        if (next) showFleet();
        else location.reload();
      }}>Fleet: units, factory tests, firmware and the herd network</Check>
    </div>
  );
}

// Settings > Collars: updates are automatic unless paused, and what the collars run.
export function FirmwareUpdates() {
  const [p, setP] = useState<FirmwarePolicy>();
  const [units, setUnits] = useState<UnitFirmware[]>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    fleet.policy().then(setP, (e) => setErr((e as Error).message));
    fleet.fwUnits().then(setUnits, () => setUnits(undefined));
  }, []);
  if (err) return <p className="err">{err}</p>;
  if (!p) return null;
  const save = async (next: FirmwarePolicy) => {
    setErr(undefined);
    try {
      setP(await fleet.savePolicy(next));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <div className="fwset">
      <div className="line">
        <Segmented label="Firmware updates" value={p.mode} onChange={(mode) => void save({ ...p, mode })}
          options={[{ value: "auto", label: "Automatic" }, { value: "paused", label: "Paused" }]} />
      </div>
      {units && units.length > 0 && <p className="dim">{versionsLine(units)}</p>}
    </div>
  );
}

// "12 on 1.0.0, 2 on 0.2.0 · 14 can't update over the air yet"
export function versionsLine(units: UnitFirmware[]): string {
  const by = new Map<string, number>();
  for (const u of units) by.set(u.current ?? "unknown", (by.get(u.current ?? "unknown") ?? 0) + 1);
  const versions = [...by.entries()].sort((a, b) => b[1] - a[1]).map(([v, n]) => `${n} on ${v}`).join(", ");
  const states = new Map<string, number>();
  for (const u of units) if (u.state !== "current" && u.state !== "confirmed") states.set(FW_WORDS[u.state], (states.get(FW_WORDS[u.state]) ?? 0) + 1);
  const rest = [...states.entries()].map(([w, n]) => `${n} ${w}`).join(", ");
  return rest ? `${versions} · ${rest}` : versions;
}
