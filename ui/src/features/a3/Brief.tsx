// Beside Daily in Settings: the morning brief by text and its farm time.

import { useEffect, useState } from "react";
import { useChannels } from "../../store/a-notify";
import { Check } from "../../ui/Check";
import { canBrief, validTime } from "./logic";
import { saveTexting, useTexting } from "./state";

export function BriefTime() {
  const t = useTexting();
  const c = useChannels();
  const [v, setV] = useState("");
  const [err, setErr] = useState<string>();
  useEffect(() => setV(t?.brief.time ?? ""), [t?.brief.time]);
  if (!t || !c || !canBrief(c.configured)) return null;
  const save = async (brief: { enabled?: boolean; time?: string }) => {
    setErr(undefined);
    try {
      await saveTexting({ brief });
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const saveTime = () => {
    if (v !== t.brief.time && validTime(v)) void save({ time: v });
  };
  return (
    <div className="line a3brief">
      <Check checked={t.brief.enabled} onChange={(enabled) => void save({ enabled })}>Brief</Check>
      <input className="input sm mono" type="time" value={v} aria-label="Morning brief time" onChange={(e) => setV(e.target.value)} onBlur={saveTime}
        onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); saveTime(); } }} />
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
