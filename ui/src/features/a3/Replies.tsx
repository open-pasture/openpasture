// Settings > Texting, below the channel form: how texts reach the farm. "Replies by webhook"
// with the URL to give Twilio, "Replies checked every 10 s" or "Replies through the relay",
// and the last check's failure in words.

import { useEffect, useRef, useState } from "react";
import { useChannels } from "../../store/a-notify";
import { Copy } from "../../ui";
import { Check } from "../../ui/Check";
import { repliesLine } from "./logic";
import { loadTexting, saveTexting, useTexting } from "./state";

export function Replies() {
  const t = useTexting();
  const c = useChannels();
  const [err, setErr] = useState<string>();
  const configured = c?.configured.join(",");
  // A channel saved above changes how texts come in.
  const seen = useRef(configured);
  useEffect(() => {
    if (seen.current !== undefined && configured !== undefined && configured !== seen.current) void loadTexting(true);
    seen.current = configured;
  }, [configured]);
  if (!t || !c) return null;
  const line = repliesLine(t, c.configured);
  if (!line) return null;
  const flip = async (inbound: boolean) => {
    setErr(undefined);
    try {
      await saveTexting({ inbound });
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <div className="a3replies">
      <div className="line">
        <Check checked={t.inbound} onChange={(on) => void flip(on)}>{line.label}</Check>
        {(err ?? line.error) && <span className="mono err">{err ?? line.error}</span>}
      </div>
      {line.urls.map((u) => <Copy key={u.label} label={u.label} value={u.url} />)}
    </div>
  );
}
