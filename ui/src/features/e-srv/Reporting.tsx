import { useEffect, useRef, useState } from "react";
import { esrv, type CollarsConfig } from "../../api/e-srv";
import { Input } from "../../ui";
import { seconds } from "./format";

// Settings > Collars: "Report every [60] s, every [10] s while moving". Each number sets the
// report and boundary-poll interval together; collars get it in their next signed config.
export function Reporting() {
  const [cfg, setCfg] = useState<CollarsConfig>();
  // Saves run one after another against what the server last said, so a quick edit of both
  // numbers (Enter, then the other field) never loses one.
  const latest = useRef<CollarsConfig>(undefined);
  const queue = useRef<Promise<void>>(Promise.resolve());
  useEffect(() => {
    esrv.collarsConfig().then((c) => {
      latest.current = c;
      setCfg(c);
    }).catch(() => setCfg(undefined));
  }, []);
  if (!cfg) return null;
  const save = (change: { base?: number; fast?: number }) => {
    queue.current = queue.current.then(async () => {
      const cur = latest.current;
      if (!cur) return;
      const base = change.base ?? cur.report_s;
      const fast = change.fast ?? cur.fast_report_s;
      if (base === cur.report_s && base === cur.poll_s && fast === cur.fast_report_s && fast === cur.fast_poll_s) return;
      const saved = await esrv.saveCollarsConfig({ report_s: base, poll_s: base, fast_report_s: fast, fast_poll_s: fast });
      latest.current = saved;
      setCfg(saved);
    }).catch(() => {});
  };
  return (
    <div className="line esrv-report">
      <span>Report every</span>
      <Secs value={cfg.report_s} label="Report every" onSave={(v) => save({ base: v })} />
      <span>s, every</span>
      <Secs value={cfg.fast_report_s} label="Report every, while moving" onSave={(v) => save({ fast: v })} />
      <span>s while moving</span>
    </div>
  );
}

function Secs({ value, label, onSave }: { value: number; label: string; onSave: (v: number) => void }) {
  const [text, setText] = useState(String(value));
  useEffect(() => setText(String(value)), [value]);
  const commit = () => {
    const v = seconds(text);
    if (v === null) return setText(String(value));
    if (v !== value) onSave(v);
  };
  return (
    <Input mono className="sm secs" inputMode="numeric" value={text} aria-label={label}
      onChange={(e) => setText(e.target.value)} onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") { e.preventDefault(); commit(); }
        if (e.key === "Escape") { e.stopPropagation(); setText(String(value)); }
      }} />
  );
}
