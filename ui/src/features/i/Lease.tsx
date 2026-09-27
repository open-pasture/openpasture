// Paddock sheet: the landowner of a rented paddock, inline with the rate and what it is per,
// and the lease season. Clearing the landowner ends the lease.

import { useEffect, useState, type KeyboardEvent } from "react";
import type { Paddock } from "../../api";
import { reportsApi, type Lease, type RatePer } from "../../api/i";
import { useStore } from "../../store";
import { useCan } from "../../store/me";
import { Input } from "../../ui/Input";
import { useUnits } from "../../units";
import { currencyFor, money, rateShown, rateStored } from "./format";

const PER: RatePer[] = ["acre_season", "head_day", "au_day", "aum", "pair_month"];

function perLabel(p: RatePer, area: string) {
  switch (p) {
    case "acre_season": return `per ${area}, season`;
    case "head_day": return "per head-day";
    case "au_day": return "per AU-day";
    case "aum": return "per AUM";
    case "pair_month": return "per pair-month";
  }
}

interface Draft { landowner: string; rate: string; rate_per: RatePer; currency: string; season_from: string; season_to: string }

export default function LeaseSection({ paddock }: { paddock: Paddock }) {
  const [lease, setLease] = useState<Lease | null>();
  const can = useCan("manager");
  useEffect(() => {
    let live = true;
    reportsApi.leases().then((all) => live && setLease(all.find((l) => l.paddock_id === paddock.id) ?? null), () => live && setLease(undefined));
    return () => {
      live = false;
    };
  }, [paddock.id]);
  if (lease === undefined) return null;
  if (!can) return lease ? <ReadOnly lease={lease} /> : null;
  return <LeaseForm paddockId={paddock.id} lease={lease} onSaved={setLease} />;
}

function ReadOnly({ lease }: { lease: Lease }) {
  const u = useUnits();
  const ha = u.parse("1", "area") ?? 1;
  return (
    <ul className="kv lease">
      <li><span>Landowner</span><b>{lease.landowner}</b></li>
      <li><span>Rate</span><b>{money(rateShown(lease.rate_amount, lease.rate_per, ha))} {lease.currency} {perLabel(lease.rate_per, u.unitLabel("area"))}</b></li>
      {(lease.season_from || lease.season_to) && <li><span>Season</span><b>{lease.season_from ?? ""} – {lease.season_to ?? ""}</b></li>}
    </ul>
  );
}

function LeaseForm({ paddockId, lease, onSaved }: { paddockId: string; lease: Lease | null; onSaved: (l: Lease | null) => void }) {
  const u = useUnits();
  const tz = useStore((s) => s.state?.farm?.timezone);
  // Hectares in one of the farm's area units: acre_season rates are stored per hectare.
  const ha = u.parse("1", "area") ?? 1;
  const from = (l: Lease | null): Draft => ({
    landowner: l?.landowner ?? "",
    rate: l ? money(rateShown(l.rate_amount, l.rate_per, ha)) : "",
    rate_per: l?.rate_per ?? "acre_season",
    currency: l?.currency ?? currencyFor(tz),
    season_from: l?.season_from ?? "",
    season_to: l?.season_to ?? "",
  });
  const [d, setD] = useState<Draft>(() => from(lease));
  const [err, setErr] = useState<string>();

  const save = async (next: Draft) => {
    setD(next);
    setErr(undefined);
    const landowner = next.landowner.trim();
    try {
      if (!landowner) {
        if (lease) {
          await reportsApi.deleteLease(paddockId);
          onSaved(null);
        }
        return;
      }
      const shown = Number(next.rate.replace(/,/g, "") || "0");
      if (!Number.isFinite(shown) || shown < 0) return setErr("The rate is a number.");
      const body = {
        landowner,
        rate_per: next.rate_per,
        rate_amount: rateStored(shown, next.rate_per, ha),
        currency: next.currency.trim().toUpperCase() || "USD",
        season_from: next.season_from || undefined,
        season_to: next.season_to || undefined,
        notes: lease?.notes,
      };
      const same = lease && lease.landowner === body.landowner && lease.rate_per === body.rate_per && money(rateShown(lease.rate_amount, lease.rate_per, ha)) === money(shown)
        && lease.currency === body.currency && (lease.season_from ?? undefined) === body.season_from && (lease.season_to ?? undefined) === body.season_to;
      if (!same) onSaved(await reportsApi.putLease(paddockId, body));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const enter = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") {
      e.preventDefault();
      (e.target as HTMLInputElement).blur();
    }
  };

  return (
    <div className="lease">
      <Input className="sm" value={d.landowner} placeholder="Landowner" aria-label="Landowner"
        onChange={(e) => setD({ ...d, landowner: e.target.value })} onBlur={() => save(d)} onKeyDown={enter} />
      {d.landowner.trim() && lease && <>
        <div className="lrow">
          <Input mono className="sm rate" inputMode="decimal" value={d.rate} placeholder="Rate" aria-label="Rate"
            onChange={(e) => setD({ ...d, rate: e.target.value })} onBlur={() => save(d)} onKeyDown={enter} />
          <Input mono className="sm cur" value={d.currency} maxLength={3} aria-label="Currency"
            onChange={(e) => setD({ ...d, currency: e.target.value.toUpperCase() })} onBlur={() => save(d)} onKeyDown={enter} />
          <select className="input sm" value={d.rate_per} aria-label="Rate per"
            onChange={(e) => {
              const per = e.target.value as RatePer;
              // The number stays what the person typed; only its meaning changes.
              void save({ ...d, rate_per: per });
            }}>
            {PER.map((p) => <option key={p} value={p}>{perLabel(p, u.unitLabel("area"))}</option>)}
          </select>
        </div>
        <div className="lrow">
          <Input type="date" mono className="sm" value={d.season_from} max={d.season_to || undefined} aria-label="Season from"
            onChange={(e) => void save({ ...d, season_from: e.target.value })} />
          <span className="dim">–</span>
          <Input type="date" mono className="sm" value={d.season_to} min={d.season_from || undefined} aria-label="Season to"
            onChange={(e) => void save({ ...d, season_to: e.target.value })} />
        </div>
      </>}
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
