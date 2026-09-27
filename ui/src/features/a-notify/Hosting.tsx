// In Settings > Hosting: whether this server relays texts for the keys it issued. Shown once
// there is a key to relay for (or relaying is already on).

import { useEffect, useState } from "react";
import { api } from "../../api";
import { notifyApi, type Hosting } from "../../api/a-notify";
import { Check } from "../../ui/Check";

export function RelayHosting() {
  const [h, setH] = useState<Hosting | null>(null);
  const [keys, setKeys] = useState(0);
  const [err, setErr] = useState<string>();
  useEffect(() => {
    notifyApi.hosting().then(setH, () => setH(null));
    api.hostedKeys().then((k) => setKeys(k.length), () => setKeys(0));
  }, []);
  if (!h || (!h.enabled && keys === 0)) return null;
  const flip = async (enabled: boolean) => {
    setErr(undefined);
    try {
      setH(await notifyApi.saveHosting({ enabled }));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <div className="line">
      <Check checked={h.enabled} onChange={(on) => void flip(on)}>Relay texts</Check>
      {h.enabled && <span className="mono dim">{h.per_key_minute}/min  {h.per_key_day}/day per key</span>}
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
