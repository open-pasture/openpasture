import { useState } from "react";
import { api, type Settings } from "../../api";
import { store, useStore } from "../../store";
import { Segmented } from "../../ui";

// Settings > Farm: the units every number in the app, the reports and the texts are shown in.
export function UnitsSetting() {
  const units = useStore((s) => s.state?.settings.units);
  const [err, setErr] = useState<string>();
  if (!units) return null;
  const choose = async (u: Settings["units"]) => {
    if (u === units) return;
    setErr(undefined);
    try {
      await api.updateSettings({ units: u });
      await store.refresh();
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <div className="line">
      <Segmented label="Units" value={units} onChange={choose}
        options={[{ value: "metric", label: "Metric" }, { value: "imperial", label: "Imperial" }]} />
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
