// The pre-send check under the boundary and strip tools: asked again 250 ms after the shape,
// warn distance or time last changed. At most three warning sentences, critical first, each
// flying to its cause; then, for the boundary tool, one mono facts line. It never blocks Send.

import { useEffect, useState } from "react";
import { fApi, type CheckResult } from "../../api/f";
import type { ToolFooterProps } from "../../registry";
import { useUnits } from "../../units";
import { factsLine, sentences } from "./model";
import { flyToCause, presend } from "./state";

export const DEBOUNCE_MS = 250;

export function PresendCheck({ tool, geometry, herdId, opts }: ToolFooterProps) {
  const u = useUnits();
  const [result, setResult] = useState<CheckResult>();
  const [error, setError] = useState<string>();
  const key = geometry ? JSON.stringify([herdId, geometry.coordinates, opts.warn_m ?? null, opts.effective_at ?? null]) : "";

  useEffect(() => {
    if (!geometry) {
      setResult(undefined);
      setError(undefined);
      presend.set({});
      return;
    }
    let live = true;
    const t = setTimeout(() => {
      fApi.check(herdId, { geometry, warn_m: opts.warn_m, effective_at: opts.effective_at, sweep: true }).then(
        (r) => {
          if (!live) return;
          setResult(r);
          setError(undefined);
          presend.set({ result: r });
        },
        (e: Error) => {
          if (!live) return;
          setResult(undefined);
          setError(e.message);
          presend.set({});
        },
      );
    }, DEBOUNCE_MS);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [key]); // eslint-disable-line react-hooks/exhaustive-deps

  // Leaving the tool clears the map.
  useEffect(() => () => presend.set({}), []);

  if (error) return <p className="mono err fcheck-err">{error}</p>;
  if (!result) return null;
  const said = sentences(result.findings);
  const facts = tool === "boundary" ? factsLine(u, result.facts) : "";
  if (!said.length && !facts) return null;
  return (
    <div className="fcheck">
      {said.map((f) => (
        <button key={f.code + f.text} type="button" className="fsent" data-sev={f.severity} onClick={() => flyToCause(f)}>
          {f.text}
        </button>
      ))}
      {facts && <p className="mono ffacts">{facts}</p>}
    </div>
  );
}
