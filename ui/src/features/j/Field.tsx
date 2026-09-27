// A text field that saves itself when you leave it (or press Enter), with the server's
// answer beside it when it says no.

import { useEffect, useState, type HTMLAttributes } from "react";
import { Input } from "../../ui";

export function Field({ value, label, onSave, mono, type, inputMode, className }: {
  value: string; label: string; onSave: (v: string) => Promise<void>; mono?: boolean; type?: string;
  inputMode?: HTMLAttributes<HTMLInputElement>["inputMode"]; className?: string;
}) {
  const [v, setV] = useState(value);
  const [err, setErr] = useState<string>();
  useEffect(() => setV(value), [value]);
  const save = async () => {
    setErr(undefined);
    try {
      await onSave(v);
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <span className={"field" + (className ? ` ${className}` : "")}>
      <Input className="sm" mono={mono} type={type} inputMode={inputMode} placeholder={label} aria-label={label} value={v}
        aria-invalid={err ? true : undefined} onChange={(e) => setV(e.target.value)} onBlur={save}
        onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); e.currentTarget.blur(); } }} />
      {err && <span className="mono err">{err}</span>}
    </span>
  );
}
