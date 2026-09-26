import { useState } from "react";
import { Button } from "./Button";

// A value with a quiet mono label and a Copy action. Secrets stay hidden until clicked.
export function Copy({ value, secret, label }: { value: string; secret?: boolean; label: string }) {
  const [shown, setShown] = useState(!secret);
  const [done, setDone] = useState(false);
  return (
    <div className="copy">
      <code onClick={() => setShown(true)}><span>{label}</span>{shown ? value : "•".repeat(Math.min(24, value.length))}</code>
      <Button small kind="plain" onClick={async () => {
        await copyText(value);
        setDone(true);
        setTimeout(() => setDone(false), 1200);
      }}>{done ? "Copied" : "Copy"}</Button>
    </div>
  );
}

// navigator.clipboard only exists on secure origins; a LAN http:// page falls back to execCommand.
async function copyText(value: string) {
  try {
    if (navigator.clipboard) return await navigator.clipboard.writeText(value);
  } catch {
    /* fall through */
  }
  const t = document.createElement("textarea");
  t.value = value;
  t.setAttribute("readonly", "");
  t.style.position = "fixed";
  t.style.opacity = "0";
  document.body.appendChild(t);
  t.select();
  document.execCommand("copy");
  t.remove();
}
