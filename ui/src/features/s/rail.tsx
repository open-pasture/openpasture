// The time rail along the bottom of the map while the herd has a schedule. Drag it forward to
// see the boundary the collars will hold then (dashed), the strips already grazed by then
// (dimmed like swept ground) and the back fence (a line). Day marks read "Wed".

import { useEffect, useRef, useState, type PointerEvent as RPointerEvent } from "react";
import { createRoot } from "react-dom/client";
import type { Map as MLMap } from "maplibre-gl";
import { C, fc, setData } from "../../map/base";
import type { OverlayCtx, OverlayHandle } from "../../map/overlays";
import { useStore } from "../../store";
import { sState } from "../../store/s";
import { useNow } from "../../util";
import { atLabel } from "../b/when";
import { backLine, boundaryAt, dayTicks, grazedAt, isOpen, pending, railSpan, stripAt } from "./model";

const SOURCES = ["s-grazed", "s-plan", "s-back"] as const;
// The track's width in px (s.css), and the least room a day label needs.
const TRACK_PX = 380;
const LABEL_PX = 34;

// Day labels that fit: every day while there's room, else every second, third, ... day.
function labelled(ticks: { at: number }[], i: number, a: number, b: number): boolean {
  const gap = ticks.length > 1 ? ((ticks[1].at - ticks[0].at) / (b - a)) * TRACK_PX : TRACK_PX;
  const every = Math.max(1, Math.ceil(LABEL_PX / Math.max(gap, 1)));
  return i % every === 0;
}
const LAYERS = ["s-grazed-fill", "s-plan-line", "s-back-line"] as const;

export function mountRail(ctx: OverlayCtx): OverlayHandle {
  const map = ctx.map;
  for (const id of SOURCES) setData(map, id, fc([]));
  const before = map.getLayer(ctx.beforeId("slot-plan")) ? ctx.beforeId("slot-plan") : undefined;
  map.addLayer({ id: "s-grazed-fill", type: "fill", source: "s-grazed", paint: { "fill-color": C.bg, "fill-opacity": 0.5 } }, before);
  map.addLayer({ id: "s-plan-line", type: "line", source: "s-plan", paint: { "line-color": C.grass2, "line-width": 1.5, "line-dasharray": [2, 2] } }, before);
  map.addLayer({ id: "s-back-line", type: "line", source: "s-back", layout: { "line-cap": "round" }, paint: { "line-color": C.grass2, "line-width": 3 } }, before);
  const el = document.createElement("div");
  el.className = "srail-host";
  map.getContainer().appendChild(el);
  const root = createRoot(el);
  root.render(<Rail map={map} />);
  return {
    destroy() {
      root.unmount();
      el.remove();
      try {
        for (const id of LAYERS) if (map.getLayer(id)) map.removeLayer(id);
        for (const id of SOURCES) if (map.getSource(id)) map.removeSource(id);
      } catch {
        /* the map went first */
      }
    },
  };
}

function Rail({ map }: { map: MLMap }) {
  const herdId = useStore((s) => s.herdId);
  const tz = useStore((s) => s.state?.farm?.timezone) ?? "UTC";
  const hs = sState.use((s) => (herdId ? s.byHerd[herdId] : undefined));
  const now = useNow(60_000);
  // How far along the rail the knob is, 0 = now.
  const [f, setF] = useState(0);
  const track = useRef<HTMLDivElement>(null);
  const s = hs?.schedule;
  const moves = hs?.moves ?? [];
  const live = s && s.status !== "done" && moves.some(pending);
  const [a, b] = railSpan(moves, now);
  const at = a + f * (b - a);

  useEffect(() => setF(0), [herdId, s?.id]);
  // Off "now" the plan line is the one dashed line: the map's next-move line steps aside.
  const scrubbed = !!s && !!live && f !== 0;
  useEffect(() => {
    const show = (v: boolean) => map.getLayer("pending-line") && map.setLayoutProperty("pending-line", "visibility", v ? "visible" : "none");
    show(!scrubbed);
    return () => {
      try {
        show(true);
      } catch {
        /* the map went first */
      }
    };
  }, [map, scrubbed]);
  // What the collars will hold at `at`.
  useEffect(() => {
    const draw = (id: (typeof SOURCES)[number], features: GeoJSON.Feature[]) => setData(map, id, fc(features));
    if (!s || !live || f === 0) {
      for (const id of SOURCES) draw(id, []);
      return;
    }
    const m = boundaryAt(moves, at);
    const k = stripAt(moves, at);
    draw("s-plan", m ? [{ type: "Feature", properties: {}, geometry: m.geometry }] : []);
    draw("s-grazed", grazedAt(s, moves, at).map((i) => ({ type: "Feature", properties: {}, geometry: s.strips[i] })));
    const back = m && k !== undefined && s.back_fence.enabled ? backLine(s.strips, m.geometry, k) : [];
    draw("s-back", back.map((line) => ({ type: "Feature", properties: {}, geometry: { type: "LineString", coordinates: line } })));
  }, [map, s, moves, at, f, live]);

  if (!s || !live) return null;
  const pos = (t: number) => `${(100 * (t - a)) / (b - a)}%`;
  const from = (e: RPointerEvent) => {
    const r = track.current?.getBoundingClientRect();
    if (r) setF(Math.min(1, Math.max(0, (e.clientX - r.left) / r.width)));
  };
  const ticks = dayTicks(a, b, tz);
  const marks = moves.filter((m) => pending(m) && Date.parse(m.at) >= a);
  return (
    <div className="srail">
      <div className="strack" ref={track} role="slider" aria-label="Time" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(f * 100)}
        aria-valuetext={f === 0 ? "now" : atLabel(at, tz, now)} tabIndex={0}
        onPointerDown={(e) => { e.currentTarget.setPointerCapture(e.pointerId); from(e); }}
        onPointerMove={(e) => e.buttons && from(e)}
        onKeyDown={(e) => {
          if (e.key === "ArrowRight") setF((x) => Math.min(1, x + 0.02));
          if (e.key === "ArrowLeft") setF((x) => Math.max(0, x - 0.02));
          if (e.key === "Escape" || e.key === "Home") setF(0);
        }}>
        {ticks.map((t, i) => (
          <i key={t.at} className="stick" style={{ left: pos(t.at) }}>{labelled(ticks, i, a, b) && <span className="mono">{t.label}</span>}</i>
        ))}
        {marks.map((m) => <b key={`${m.index}:${m.step}:${m.at}`} className={isOpen(m) ? "sopen" : "sstep"} style={{ left: pos(Date.parse(m.at)) }} />)}
        <span className="sknob" style={{ left: pos(at) }} />
      </div>
      <button type="button" className="mono sat" onClick={() => setF(0)} disabled={f === 0}>{f === 0 ? "now" : atLabel(at, tz, now)}</button>
    </div>
  );
}
