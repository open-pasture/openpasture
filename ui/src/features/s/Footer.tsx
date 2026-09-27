// Under the strip tool: put the herd on a schedule of these strips. "Daily [07:00]
// [x] Back fence  Schedule", and one line saying when the first of them opens.

import { useEffect, useState } from "react";
import { sApi, type NewSchedule } from "../../api/s";
import type { ToolFooterProps } from "../../registry";
import { store, useStore } from "../../store";
import { loadSchedule, sState } from "../../store/s";
import { Button, Segmented } from "../../ui";
import { Check } from "../../ui/Check";
import { useNow } from "../../util";
import { atLabel } from "../b/when";
import { nextOpen } from "./model";

type Every = "1" | "2" | "3";

export function ScheduleFooter({ strips, layoutId, herdId }: ToolFooterProps) {
  const hs = sState.use((s) => s.byHerd[herdId]);
  const tz = useStore((s) => s.state?.farm?.timezone) ?? "UTC";
  const now = useNow(30_000);
  const [every, setEvery] = useState<Every>("1");
  const [at, setAt] = useState("07:00");
  const [fence, setFence] = useState(true);
  const [line, setLine] = useState<{ text: string; err?: boolean }>();
  const [busy, setBusy] = useState(false);
  useEffect(() => void loadSchedule(herdId), [herdId]);
  const running = hs?.schedule;

  const body = (): NewSchedule => ({
    herd_id: herdId, strips, layout_id: layoutId, cadence: { every_days: Number(every), at }, back_fence: { enabled: fence },
  });
  const key = JSON.stringify([strips?.length, strips?.[0]?.coordinates[0]?.[0], layoutId, every, at, fence, herdId]);
  // What would happen, asked a moment after the last change.
  useEffect(() => {
    if (running || !strips?.length) return;
    let live = true;
    const t = setTimeout(() => {
      sApi.preview(body()).then(
        (p) => {
          const first = nextOpen(p.moves);
          if (live && first) setLine({ text: `strip ${first.index + 1} opens ${atLabel(Date.parse(first.at), tz, Date.now())}` });
        },
        (e: Error) => live && setLine({ text: e.message, err: true }),
      );
    }, 250);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [key, !!running]); // eslint-disable-line react-hooks/exhaustive-deps

  if (running) {
    const next = nextOpen(hs.moves);
    return (
      <p className="mono sline">
        {running.status === "paused" ? "schedule paused" : next ? `strip ${next.index + 1} of ${running.strips.length} opens ${atLabel(Date.parse(next.at), tz, now)}` : "scheduled"}
      </p>
    );
  }
  if (!strips?.length) return null;
  const make = async () => {
    setBusy(true);
    try {
      await sApi.create(body());
      await loadSchedule(herdId);
      await store.refreshHerd();
    } catch (e) {
      setLine({ text: (e as Error).message, err: true });
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="sfoot">
      <div className="srowf">
        <Segmented<Every> label="Every" value={every} onChange={setEvery}
          options={[{ value: "1", label: "Daily" }, { value: "2", label: "2 d" }, { value: "3", label: "3 d" }]} />
        <input className="input sm mono" type="time" value={at} aria-label="Open time" onChange={(e) => e.target.value && setAt(e.target.value)} />
        <Check checked={fence} onChange={setFence}>Back fence</Check>
        <Button small disabled={busy || !!line?.err} onClick={() => void make()}>Schedule</Button>
      </div>
      {line && <p className={"mono " + (line.err ? "err" : "sline")}>{line.text}</p>}
    </div>
  );
}
