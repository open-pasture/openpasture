import { useEffect, useState } from "react";
import { fleet, type PlanStep, type TestPlan } from "../../api/fleet";
import { useCan } from "../../store/me";
import { Button, Input } from "../../ui";
import { Check } from "../../ui/Check";

// Test plans: each class's steps and limits. The owner edits limits and which steps are
// required; saving makes a new version, and runs keep the version they were tested to.
export function Plans() {
  const [plans, setPlans] = useState<TestPlan[]>();
  const [err, setErr] = useState<string>();
  const load = () => fleet.plans().then(setPlans, (e) => setErr((e as Error).message));
  useEffect(() => void load(), []);
  if (err) return <p className="err">{err}</p>;
  if (!plans) return null;
  return <>{plans.map((p) => <Plan key={`${p.id}:${p.version}`} plan={p} onSaved={() => void load()} />)}</>;
}

function Plan({ plan, onSaved }: { plan: TestPlan; onSaved: () => void }) {
  const owner = useCan("owner");
  const [edit, setEdit] = useState<PlanStep[]>();
  const [err, setErr] = useState<string>();
  const steps = edit ?? plan.steps;
  const set = (i: number, patch: Partial<PlanStep>) => setEdit(steps.map((s, j) => (j === i ? { ...s, ...patch } : s)));
  const save = async () => {
    if (!edit) return;
    setErr(undefined);
    try {
      await fleet.savePlan(plan.device_class, edit);
      setEdit(undefined);
      onSaved();
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const limit = (v: string) => (v.trim() === "" ? undefined : Number(v));
  return (
    <section className="fplan">
      <div className="line">
        <h3 className="grow">{plan.device_class} <span className="dim mono">{plan.id} v{plan.version}{plan.shipped ? " as shipped" : ""}</span></h3>
        {owner && !edit && <Button small kind="plain" onClick={() => setEdit(plan.steps)}>Edit</Button>}
        {edit && <><Button small kind="plain" onClick={() => setEdit(undefined)}>Cancel</Button><Button small onClick={() => void save()}>Save as v{plan.version + 1}</Button></>}
      </div>
      {err && <p className="err">{err}</p>}
      <table className="tbl">
        <thead><tr><th>step</th><th>kind</th><th>low</th><th>high</th><th>unit</th><th>required</th><th>note</th></tr></thead>
        <tbody>
          {steps.map((s, i) => (
            <tr key={s.name}>
              <td>{s.name}</td><td>{s.kind}</td>
              {edit && s.kind === "measure" ? (
                <>
                  <td><Input mono className="sm lim" defaultValue={s.limit_lo ?? ""} aria-label={`${s.name} low`} onChange={(e) => set(i, { limit_lo: limit(e.target.value) })} /></td>
                  <td><Input mono className="sm lim" defaultValue={s.limit_hi ?? ""} aria-label={`${s.name} high`} onChange={(e) => set(i, { limit_hi: limit(e.target.value) })} /></td>
                </>
              ) : (
                <><td>{s.limit_lo ?? ""}</td><td>{s.limit_hi ?? ""}</td></>
              )}
              <td>{s.unit ?? ""}</td>
              <td>{edit ? <Check checked={s.required} label={`${s.name} required`} onChange={(v) => set(i, { required: v })} /> : s.required ? "yes" : ""}</td>
              <td className="wrap dim">{s.note ?? ""}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </section>
  );
}
