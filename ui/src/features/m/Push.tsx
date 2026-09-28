// Settings > You: alerts as notifications in this browser (a phone with the app installed, or any
// browser that can). On and off, and a test once on. Shown only where it can work: a secure page,
// the Push API, the server on its https address, and a person to send to.

import { useEffect, useState } from "react";
import { pushApi, type PushView } from "../../api/m";
import { useMeUser } from "../../store/me";
import { Button } from "../../ui";
import { Check } from "../../ui/Check";
import { touchUI } from "./phone";
import { current, mineOf, pushSupported, turnOff, turnOn } from "./subscribe";

export function PushHere() {
  const user = useMeUser();
  const [v, setV] = useState<PushView>();
  const [sub, setSub] = useState<PushSubscription | null>(null);
  const [busy, setBusy] = useState(false);
  const [note, setNote] = useState<{ ok: boolean; text: string }>();
  const supported = pushSupported();
  const load = async () => {
    const [view, s] = await Promise.all([pushApi.get(), current()]);
    setV(view);
    setSub(s);
  };
  useEffect(() => {
    if (supported && user) load().catch(() => setV(undefined));
  }, [supported, user?.id]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!supported || !user || !v?.available) return null;
  const mine = mineOf(v, sub);
  const blocked = Notification.permission === "denied" && !mine;
  const run = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setNote(undefined);
    try {
      await f();
    } catch (e) {
      setNote({ ok: false, text: (e as Error).message });
    } finally {
      setBusy(false);
      await load().catch(() => {});
    }
  };
  return (
    <div className="line mpush">
      <Check checked={!!mine} disabled={busy || blocked} onChange={() => run(() => (mine ? turnOff(v) : turnOn(v)))}>
        {touchUI() ? "Alerts on this phone" : "Alerts in this browser"}
      </Check>
      {mine && <Button small kind="plain" disabled={busy} onClick={() => run(async () => {
        const r = await pushApi.test(mine.id);
        setNote({ ok: r.ok, text: r.detail });
      })}>Test</Button>}
      {blocked && <span className="mono dim">Notifications are blocked for this site.</span>}
      {note && <span className={"mono " + (note.ok ? "dim" : "err")}>{note.text}</span>}
    </div>
  );
}
