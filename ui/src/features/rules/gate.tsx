// The safety gate in the send tools. A send the gate holds (a road crossed, the farm boundary,
// a hard hazard, no water where the farm wants it) comes back with its findings: they show
// here, each flying to its place on the map, where the pre-send check draws them, and the
// farmer may send it anyway, naming each one. A no-go zone can't be sent into by anyone.

import { useCallback, useState, type ReactNode } from "react";
import { flyToCause } from "../f/state";
import { Button } from "../../ui";
import { confirmable, heldBy, type Held } from "./model";

// `run(send)`: send with nothing confirmed; if the gate holds it, `panel` shows why and
// sends again confirming what it found when the farmer says so.
export function useGate(): { run: (send: (confirm: string[]) => Promise<void>) => Promise<void>; panel: ReactNode; held?: Held } {
  const [held, setHeld] = useState<Held>();
  const [again, setAgain] = useState<() => Promise<void>>();
  const run = useCallback(async (send: (confirm: string[]) => Promise<void>) => {
    setHeld(undefined);
    try {
      await send([]);
    } catch (e) {
      const h = heldBy(e);
      if (!h) throw e;
      setHeld(h);
      setAgain(() => () => send(h.confirm).then(() => setHeld(undefined)));
      if (h.findings[0]) flyToCause(h.findings[0]);
    }
  }, []);
  const panel = held ? <GateHold held={held} onSend={again} onCancel={() => setHeld(undefined)} /> : null;
  return { run, panel, held };
}

function GateHold({ held, onSend, onCancel }: { held: Held; onSend?: () => Promise<void>; onCancel: () => void }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  const send = async () => {
    if (!onSend) return;
    setBusy(true);
    try {
      await onSend();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="gatehold" role="alert">
      {held.findings.map((f) => (
        <button key={f.code + f.text} type="button" className="fsent" data-sev="critical" onClick={() => flyToCause(f)}>{f.text}</button>
      ))}
      <div className="gline">
        {confirmable(held) ? (
          <Button small kind="primary" disabled={busy} onClick={() => void send()}>Send anyway</Button>
        ) : (
          <span className="mono dim">No boundary may take in a no-go zone.</span>
        )}
        <Button small kind="plain" onClick={onCancel}>Back</Button>
        {err && <span className="mono err">{err}</span>}
      </div>
    </div>
  );
}

// Why a proposal waits for a person instead of going on its own: what the safety gate and the
// rule review found (`inputs.review.reasons`).
export function Held({ inputs }: { inputs: unknown }) {
  const review = (inputs as { review?: { reasons?: unknown } } | null)?.review;
  const reasons = Array.isArray(review?.reasons) ? review.reasons.filter((r): r is string => typeof r === "string") : [];
  if (!reasons.length) return null;
  return (
    <ul className="held">
      {reasons.map((r) => <li key={r}>{r}</li>)}
    </ul>
  );
}
