import { useEffect, useMemo, useState } from "react";
import { fleet, type TestPlan, type Throughput, type Yield } from "../../api/fleet";
import { store } from "../../store";
import { Segmented } from "../../ui";
import { pct } from "./model";

type Range = "1d" | "7d" | "30d";
const SPAN: Record<Range, number> = { "1d": 86400e3, "7d": 7 * 86400e3, "30d": 30 * 86400e3 };

// Build: yield per test step, the steps failing most, and each station's pace.
export function Build() {
  const [plans, setPlans] = useState<TestPlan[]>();
  const [plan, setPlan] = useState<string>();
  const [version, setVersion] = useState<number>();
  const [range, setRange] = useState<Range>("7d");
  const [y, setY] = useState<Yield>();
  const [t, setT] = useState<Throughput>();
  const [err, setErr] = useState<string>();
  const [tick, setTick] = useState(0);
  useEffect(() => {
    fleet.plans().then((p) => {
      setPlans(p);
      setPlan((cur) => cur ?? p[0]?.id);
    }, (e) => setErr((e as Error).message));
    return store.on("fleet_test", () => setTick((n) => n + 1));
  }, []);
  const { from, to } = useMemo(() => {
    const now = Date.now();
    return { from: new Date(now - SPAN[range]).toISOString(), to: new Date(now + 60e3).toISOString() };
  }, [range, tick]); // eslint-disable-line react-hooks/exhaustive-deps
  // Every version of the chosen plan, newest first: yield is per version, never mixed.
  const versions = useMemo(() => (plans ?? []).filter((p) => p.id === plan).map((p) => p.version).sort((a, b) => b - a), [plans, plan]);
  const ids = useMemo(() => [...new Set((plans ?? []).map((p) => p.id))], [plans]);
  useEffect(() => {
    if (!plan) return;
    fleet.yield({ plan_id: plan, plan_version: version, from, to }).then(setY, (e) => setErr((e as Error).message));
    fleet.throughput({ from, to, bucket: range === "1d" ? "hour" : "day" }).then(setT, (e) => setErr((e as Error).message));
  }, [plan, version, from, to, range]);
  if (err) return <p className="err">{err}</p>;
  if (!plans) return null;
  return (
    <>
      <div className="line fline">
        <select className="input sm" value={plan ?? ""} onChange={(e) => { setPlan(e.target.value); setVersion(undefined); }} aria-label="Test plan">
          {ids.map((id) => <option key={id} value={id}>{id}</option>)}
        </select>
        {versions.length > 1 && (
          <select className="input sm" value={version ?? versions[0]} onChange={(e) => setVersion(Number(e.target.value))} aria-label="Plan version">
            {versions.map((v) => <option key={v} value={v}>v{v}</option>)}
          </select>
        )}
        <Segmented label="Range" value={range} onChange={setRange} options={[{ value: "1d", label: "24 h" }, { value: "7d", label: "7 d" }, { value: "30d", label: "30 d" }]} />
      </div>
      {y && y.other_versions.length > 0 && (
        <p className="dim">{y.other_versions.map((o) => `${o.runs} run${o.runs === 1 ? "" : "s"} judged by v${o.plan_version}`).join(", ")} in this range {y.other_versions.length === 1 ? "isn't" : "aren't"} in these numbers.</p>
      )}
      {y && (y.runs === 0 ? <p className="dim">No runs of {y.plan_id} v{y.plan_version} in this range.</p> : (
        <>
          <ul className="kv">
            <li><span>units, runs</span><b>{y.units}, {y.runs}</b></li>
            <li><span>first-pass yield</span><b>{pct(y.first_pass.rate)}  {y.first_pass.passed}/{y.first_pass.units}</b></li>
            <li><span>final yield</span><b>{pct(y.final.rate)}  {y.final.passed}/{y.final.units}</b></li>
            {y.failures.length > 0 && <li><span>failing most</span><b>{y.failures.slice(0, 3).map((f) => `${f.name} ${f.fail}`).join("  ")}</b></li>}
          </ul>
          <table className="tbl">
            <thead><tr><th>step</th><th>pass</th><th>fail</th><th>skip</th><th>missing</th><th>yield</th><th>min</th><th>median</th><th>max</th></tr></thead>
            <tbody>
              {y.steps.map((s) => (
                <tr key={s.name}>
                  <td>{s.name}{s.required ? "" : " (optional)"}</td><td>{s.pass}</td><td className={s.fail ? "err" : ""}>{s.fail}</td><td>{s.skip}</td><td>{s.missing}</td><td>{pct(s.rate)}</td>
                  <td>{s.values ? `${num(s.values.min)}` : ""}</td><td>{s.values ? `${num(s.values.p50)}` : ""}</td><td>{s.values ? `${num(s.values.max)} ${s.values.unit ?? ""}` : ""}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      ))}
      {t && t.stations.length > 0 && (
        <>
          <h3>Stations</h3>
          <table className="tbl">
            <thead><tr><th>station</th><th>units</th><th>runs</th><th>pass</th><th>fail</th><th>cycle</th><th>per {t.bucket}</th></tr></thead>
            <tbody>
              {t.stations.map((s) => (
                <tr key={s.station}>
                  <td>{s.station}</td><td>{s.units}</td><td>{s.runs}</td><td>{s.pass}</td><td className={s.fail ? "err" : ""}>{s.fail}</td>
                  <td>{s.cycle_s_p50 !== undefined ? `${Math.round(s.cycle_s_p50)} s` : ""}</td>
                  <td className="wrap">{s.buckets.map((b) => b.runs).join(" ")}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
    </>
  );
}

function num(v: number) {
  return Math.abs(v) >= 100 ? v.toFixed(0) : Math.abs(v) >= 10 ? v.toFixed(1) : v.toFixed(2);
}
