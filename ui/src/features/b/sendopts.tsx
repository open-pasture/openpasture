// The boundary tool's two options, as text buttons that turn into inputs: "warn 16 ft" (how far
// inside the line collars start to warn) and "now" (when the collars switch to it; a later time
// stages it). The warn band shows on the drawn shape as an inner dashed line.

import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import type { Map as MLMap } from "maplibre-gl";
import type { Polygon } from "../../api";
import type { OverlayCtx } from "../../map/overlays";
import type { ToolFooterProps } from "../../registry";
import { useStore } from "../../store";
import { Button } from "../../ui";
import { NumberField } from "../../ui/NumberField";
import { useUnits } from "../../units";
import { ccw, offsetExpr } from "./band";
import { TONE } from "./ramp";
import { atLabel, clockIn, nextAt, soon } from "./when";

type Opts = ToolFooterProps["opts"];

// The collars' own warning distance when a boundary names none (firmware default).
export const DEFAULT_WARN_M = 5;

const browserTz = () => Intl.DateTimeFormat().resolvedOptions().timeZone;

export function SendOptions({ opts, onChange, defaultWarn = DEFAULT_WARN_M }: { opts: Opts; onChange: (o: Opts) => void; defaultWarn?: number }) {
  const u = useUnits();
  const tz = useStore((s) => s.state?.farm?.timezone) ?? browserTz();
  const [edit, setEdit] = useState<"warn" | "at">();
  const warnBox = useRef<HTMLSpanElement>(null);
  // The herd's training warn while its training mode is on (H), else the collars' default.
  const warn = opts.warn_m ?? defaultWarn;
  useEffect(() => {
    if (edit === "warn") warnBox.current?.querySelector("input")?.select();
  }, [edit]);

  const setAt = (hhmm: string) => onChange({ ...opts, effective_at: /^\d\d:\d\d$/.test(hhmm) ? new Date(nextAt(hhmm, tz)).toISOString() : undefined });
  const atKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape") {
      // Back to now; the tool stays open.
      e.stopPropagation();
      onChange({ ...opts, effective_at: undefined });
      setEdit(undefined);
    } else if (e.key === "Enter") {
      e.preventDefault();
      setAt(e.currentTarget.value);
      setEdit(undefined);
    }
  };

  return (
    <>
      {edit === "warn" ? (
        <span className="bopt" ref={warnBox} onBlur={() => setEdit(undefined)}>
          <span className="dim">warn</span>
          <NumberField value={warn} quantity="len" min={1} max={100} label="Warn distance" width={3}
            onChange={(m) => onChange({ ...opts, warn_m: m })} />
        </span>
      ) : (
        <Button small kind="plain" className="bopt" onClick={() => setEdit("warn")} title="How far inside the line collars start to warn">
          warn {u.len(warn)}
        </Button>
      )}
      {edit === "at" ? (
        <input type="time" className="input sm mono btime" autoFocus aria-label="When collars switch to it"
          defaultValue={opts.effective_at ? clockIn(Date.parse(opts.effective_at), tz) : soon(tz)}
          onChange={(e) => setAt(e.target.value)} onBlur={(e) => { setAt(e.target.value); setEdit(undefined); }} onKeyDown={atKey} />
      ) : (
        <Button small kind="plain" className="bopt" onClick={() => setEdit("at")} title="When collars switch to it">
          {opts.effective_at ? atLabel(Date.parse(opts.effective_at), tz) : "now"}
        </Button>
      )}
    </>
  );
}

const SRC = "b-warnband";
const LAYER = "b-warnband-line";
// The warn band of the shape being drawn: a dashed line `warnM` inside its edge.
export function useWarnBand(map: MLMap, ctx: OverlayCtx, polygon: Polygon | undefined, warnM: number) {
  useEffect(() => {
    map.addSource(SRC, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
    map.addLayer({
      id: LAYER, type: "line", source: SRC,
      layout: { "line-join": "round" },
      paint: { "line-color": TONE.grass, "line-width": 1, "line-opacity": 0.7, "line-dasharray": [2, 3] },
    }, ctx.beforeId("slot-plan"));
    return () => {
      try {
        if (map.getLayer(LAYER)) map.removeLayer(LAYER);
        if (map.getSource(SRC)) map.removeSource(SRC);
      } catch {
        /* the map went first */
      }
    };
  }, [map, ctx]);

  useEffect(() => {
    const src = map.getSource(SRC) as { setData(d: GeoJSON.FeatureCollection): void } | undefined;
    if (!src) return;
    if (!polygon || polygon.coordinates[0].length < 4) {
      src.setData({ type: "FeatureCollection", features: [] });
      return;
    }
    const ring = ccw(polygon);
    src.setData({ type: "FeatureCollection", features: [{ type: "Feature", properties: {}, geometry: { type: "LineString", coordinates: ring } }] });
    const lat = ring.reduce((a, p) => a + p[1], 0) / ring.length;
    map.setPaintProperty(LAYER, "line-offset", offsetExpr(warnM, lat) as never);
  }, [map, polygon, warnM]);
}
