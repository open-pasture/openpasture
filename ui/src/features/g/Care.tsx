// Fleet care in the Herd table, on the animal page and in Settings > Collars (stream G).

import { useEffect, useState } from "react";
import type { Animal, Collar } from "../../api";
import { gApi, type FleetSettings } from "../../api/g";
import type { HerdRow } from "../../registry";
import { fleet, loadFleet, useFleetRow } from "../../store/g";
import { useCan } from "../../store/me";
import { Button } from "../../ui";
import { Spark } from "../../ui/Spark";
import { useNow } from "../../util";
import { daysText, dueText, percent, shortDay, trendText } from "./model";

// Herd table: the last two weeks of battery, and days left while it falls.
export function TrendCell({ row }: { row: HerdRow }) {
  const r = useFleetRow(row.collar?.id);
  if (!r) return null;
  return (
    <span className="gtrend">
      <Spark values={r.daily} domain={[0, 1]} width={48} height={14} label="Battery by day" />
      {r.days_left !== undefined && <span className="mono dim">{daysText(r.days_left)}</span>}
    </span>
  );
}

// Herd table: when the collar's fit is next due.
export function FitCell({ row }: { row: HerdRow }) {
  const r = useFleetRow(row.collar?.id);
  const now = useNow(60_000);
  if (!r) return null;
  const due = dueText(r.fit_due_at, now);
  return <span className={"mono" + (due.late ? " glate" : "")}>{due.text}</span>;
}

// Animal page: battery trend and the fit check.
export function Care({ collar }: { animal?: Animal; collar?: Collar }) {
  const r = useFleetRow(collar?.id);
  const canCheck = useCan("hand");
  const now = useNow(60_000);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  if (!collar || !r) return null;
  const check = async () => {
    setBusy(true);
    setErr(undefined);
    try {
      await gApi.checkFit(collar.id);
      await loadFleet();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const due = dueText(r.fit_due_at, now);
  const hasBattery = r.battery !== undefined || r.daily.some((v) => v !== null);
  return (
    <section className="gcare">
      <ul className="kv">
        {hasBattery && (
          <li>
            <span>battery</span>
            <b className="gline">
              <Spark values={r.daily} domain={[0, 1]} width={64} height={16} label="Battery by day" />
              {[r.battery !== undefined && percent(r.battery), r.trend_pct_day !== undefined && trendText(r.trend_pct_day), r.days_left !== undefined && `${daysText(r.days_left)} left`]
                .filter(Boolean)
                .join("  ")}
            </b>
          </li>
        )}
        <li>
          <span>fit check</span>
          <b className={due.late ? "glate" : undefined}>{(r.fit_checked_at ? shortDay(r.fit_checked_at) + "  " : "") + "due " + due.text}</b>
        </li>
      </ul>
      {canCheck && (
        <div className="acts">
          <Button small disabled={busy} onClick={() => void check()}>Checked fit</Button>
          {err && <span className="mono err">{err}</span>}
        </div>
      )}
    </section>
  );
}

// Settings > Collars: "Fit check every [30] days".
export function FitDays() {
  const [s, setS] = useState<FleetSettings>();
  const [text, setText] = useState("");
  const [err, setErr] = useState<string>();
  const canEdit = useCan("manager");
  useEffect(() => {
    gApi.settings().then((v) => {
      setS(v);
      setText(String(v.fit_check_days));
    }, (e: Error) => setErr(e.message));
  }, []);
  if (!s) return err ? <p className="mono err">{err}</p> : null;
  const commit = async () => {
    const n = Number(text.trim());
    if (!Number.isInteger(n) || n < 1 || n > 365) return setText(String(s.fit_check_days));
    if (n === s.fit_check_days) return;
    setErr(undefined);
    try {
      const saved = await gApi.saveSettings({ fit_check_days: n });
      setS(saved);
      setText(String(saved.fit_check_days));
      if (fleet.get().loaded) await loadFleet();
    } catch (e) {
      setErr((e as Error).message);
      setText(String(s.fit_check_days));
    }
  };
  return (
    <div className="line">
      <span>Fit check every</span>
      <input className="input sm mono gdays" inputMode="numeric" aria-label="Days between fit checks" value={text} disabled={!canEdit}
        onChange={(e) => setText(e.target.value)} onBlur={() => void commit()}
        onKeyDown={(e) => {
          if (e.key === "Enter") { e.preventDefault(); void commit(); }
          if (e.key === "Escape") { e.stopPropagation(); setText(String(s.fit_check_days)); }
        }} />
      <span>days</span>
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
