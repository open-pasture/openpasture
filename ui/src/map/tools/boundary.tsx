import { useEffect, useState } from "react";
import { api, type Polygon } from "../../api";
import { DEFAULT_WARN_M, SendOptions, useWarnBand } from "../../features/b/sendopts";
import type { ToolFooterProps } from "../../registry";
import { store } from "../../store";
import { useDefaultWarn } from "../../store/h";
import { Button } from "../../ui";
import { attempt } from "../../util";
import { current } from "../drawn";
import { ToolFooter, useDrawing, type ToolProps } from "../tools";

// Draw a boundary and send it to the herd's collars. The server sweeps the active
// boundary toward it; toolFooter sections (checks, schedules) show under the form.
export function BoundaryTool({ map, draw, herdId, ctx, done }: ToolProps) {
  const { drawn, geometry } = useDrawing(draw, "boundary");
  // warn_m and effective_at, when set, go with the boundary and to the footer.
  const [opts, setOpts] = useState<ToolFooterProps["opts"]>({});
  const [busy, setBusy] = useState(false);
  // A refused send says why beside Send (the footer's checks cover the shape; this is the rest).
  const [err, setErr] = useState<string>();
  useEffect(() => setErr(undefined), [geometry, opts]);
  const polygon = geometry?.type === "Polygon" ? (geometry as Polygon) : undefined;
  // A herd in training mode gets its training warn when the send names none (H).
  const defaultWarn = useDefaultWarn(herdId) ?? DEFAULT_WARN_M;
  useWarnBand(map, ctx, drawn ? polygon : undefined, opts.warn_m ?? defaultWarn);

  const send = () => {
    const g = current(draw);
    if (!g || !herdId) return;
    return attempt(async () => {
      await api.sendBoundary(herdId, { geometry: g, ...opts });
      done();
      await store.refresh();
    }, { busy: setBusy, failed: setErr });
  };

  if (!drawn)
    return (
      <>
        <span className="toolhint">Boundary</span>
        <Button small kind="plain" onClick={done}>Cancel</Button>
      </>
    );
  return (
    <>
      <form className="toolform" onSubmit={(e) => { e.preventDefault(); void send(); }}>
        <SendOptions opts={opts} onChange={setOpts} defaultWarn={defaultWarn} />
        <Button small kind="plain" onClick={done}>Cancel</Button>
        <Button small kind="primary" type="submit" disabled={busy}>Send</Button>
        {err && <span className="mono err ferr">{err}</span>}
      </form>
      {herdId && <ToolFooter tool="boundary" geometry={polygon} herdId={herdId} opts={opts} />}
    </>
  );
}
