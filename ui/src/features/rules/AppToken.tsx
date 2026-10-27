// The app token, for the owner only and only when asked: no read ever carries it.

import { useState } from "react";
import { useCan } from "../../store/me";
import { Button, Copy } from "../../ui";
import { rulesApi } from "./api";

export function AppToken() {
  const owner = useCan("owner");
  const [token, setToken] = useState<string>();
  const [err, setErr] = useState<string>();
  if (!owner) return null;
  const run = (f: () => Promise<{ app_token: string }>) => f().then((r) => setToken(r.app_token), (e: Error) => setErr(e.message));
  return (
    <div className="line">
      {token ? <Copy label="token" value={token} secret /> : <Button small kind="plain" onClick={() => void run(rulesApi.revealToken)}>Show token</Button>}
      {token && <Button small kind="plain" onClick={() => void run(rulesApi.rotateToken)}>New token</Button>}
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
