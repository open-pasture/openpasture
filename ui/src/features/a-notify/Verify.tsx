// In a person's row: "Verify" beside an unverified phone, the code entered inline; nothing
// once verified (or when nothing can text).

import { useState } from "react";
import type { User } from "../../api";
import { notifyApi } from "../../api/a-notify";
import { useChannels } from "../../store/a-notify";
import { Button, Input } from "../../ui";
import { cleanCode, needsVerify } from "./logic";

export function Verify({ user }: { user: User }) {
  const c = useChannels();
  const [step, setStep] = useState<"start" | "code" | "done">("start");
  const [code, setCode] = useState("");
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  if (!c || step === "done" || !needsVerify(user, c.configured)) return null;

  const act = async (f: () => Promise<void>) => {
    setBusy(true);
    setErr(undefined);
    try {
      await f();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const send = () => act(async () => {
    const r = await notifyApi.sendCode(user.id);
    setCode("");
    setStep(r.verified ? "done" : "code");
  });
  const confirm = () => act(async () => {
    await notifyApi.confirmCode(user.id, code);
    setStep("done");
  });

  if (step === "start") {
    return (
      <div className="verify line">
        <Button small onClick={() => void send()} disabled={busy}>Verify</Button>
        {err && <span className="mono err">{err}</span>}
      </div>
    );
  }
  return (
    <form className="verify line" onSubmit={(e) => { e.preventDefault(); void confirm(); }}>
      <Input mono className="sm code" value={code} onChange={(e) => setCode(cleanCode(e.target.value))} inputMode="numeric"
        autoComplete="one-time-code" placeholder="Code" aria-label={`Code texted to ${user.name}`} autoFocus />
      <Button small kind="primary" type="submit" disabled={busy || code.length !== 6}>Confirm</Button>
      <Button small kind="plain" onClick={() => void send()} disabled={busy}>Resend</Button>
      {err && <span className="mono err">{err}</span>}
    </form>
  );
}
