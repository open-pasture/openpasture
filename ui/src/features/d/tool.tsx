// Draw a map feature of one kind and save it. Water, shade and hazards are a point or an area;
// an exclusion keeps to the paddock it is drawn in or covers the whole farm, optionally until a
// date; a hazard point gets a radius.

import { useEffect, useMemo, useState, type ComponentType } from "react";
import type { Map as MLMap } from "maplibre-gl";
import type { FeatureGeometry, FeatureKind, LonLat, Polygon } from "../../api";
import { featuresApi, type NewFeature } from "../../api/d";
import { useStore } from "../../store";
import { putFeature } from "../../store/d";
import { Button, Input, Segmented } from "../../ui";
import { NumberField } from "../../ui/NumberField";
import { useDrawing, type ToolProps } from "../../map/tools";
import { circle, endOfDay, HAZARD_RADIUS_M, localDate, modeFor, scopePaddock, spec, type Shape } from "./model";

const made = new Map<FeatureKind, ComponentType<ToolProps>>();

// One tool component per kind, for the tools registry.
export function toolFor(kind: FeatureKind): ComponentType<ToolProps> {
  let t = made.get(kind);
  if (!t) {
    t = (props: ToolProps) => <FeatureTool kind={kind} {...props} />;
    made.set(kind, t);
  }
  return t;
}

function FeatureTool({ kind, map, draw, done }: ToolProps & { kind: FeatureKind }) {
  const s = spec(kind);
  const [shape, setShape] = useState<Shape>(s.shapes[0]);
  const { drawn, geometry } = useDrawing(draw, modeFor(kind, shape));
  const state = useStore((st) => st.state);
  const tz = state?.farm?.timezone ?? "UTC";
  const [name, setName] = useState("");
  const [radius, setRadius] = useState(HAZARD_RADIUS_M);
  const [scope, setScope] = useState<"paddock" | "farm">("paddock");
  const [until, setUntil] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();

  const shaped = geometry?.type === shape ? (geometry as FeatureGeometry) : undefined;
  const home = useMemo(
    () => (kind === "exclusion" && shaped?.type === "Polygon" ? scopePaddock(shaped as Polygon, state?.paddocks ?? []) : undefined),
    [kind, shaped, state?.paddocks],
  );

  // A hazard point shows the ground its radius covers while it is drawn.
  const ring = useMemo(
    () => (kind === "hazard" && shaped?.type === "Point" ? circle(shaped.coordinates as LonLat, radius) : undefined),
    [kind, shaped, radius],
  );
  useRadius(map, ring);

  const save = async () => {
    if (!shaped) return;
    const body: NewFeature = { kind, geometry: closed(shaped) };
    if (kind === "exclusion") {
      if (home && scope === "paddock") body.paddock_id = home.id;
      if (until) body.active_until = endOfDay(until, tz);
    } else if (kind !== "farm_boundary" && name.trim()) body.name = name.trim();
    if (kind === "hazard" && shaped.type === "Point") body.props = { radius_m: radius };
    setBusy(true);
    setErr(undefined);
    try {
      putFeature(await featuresApi.create(body));
      done();
    } catch (e) {
      setErr((e as Error).message);
      setBusy(false);
    }
  };

  if (!drawn)
    return (
      <>
        <span className="toolhint">{s.label}</span>
        {s.shapes.length > 1 && (
          <Segmented label="Shape" value={shape} onChange={setShape}
            options={s.shapes.map((v) => ({ value: v, label: v === "Point" ? "Point" : "Area" }))} />
        )}
        <Button small kind="plain" onClick={done}>Cancel</Button>
      </>
    );
  return (
    <form className="toolform" onSubmit={(e) => { e.preventDefault(); void save(); }}>
      {kind === "exclusion" ? (
        <>
          {home && (
            <Segmented label="Scope" value={scope} onChange={setScope}
              options={[{ value: "paddock", label: "This paddock", title: home.name }, { value: "farm", label: "Whole farm" }]} />
          )}
          <span className="dim dword">until</span>
          <input type="date" className="input sm mono ddate" aria-label="Until" min={localDate(Date.now(), tz)} value={until} onChange={(e) => setUntil(e.target.value)} />
        </>
      ) : (
        kind !== "farm_boundary" && <Input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="Name" aria-label="Name" className="sm" />
      )}
      {kind === "hazard" && shaped?.type === "Point" && <NumberField label="Radius" quantity="len" value={radius} min={0.5} onChange={setRadius} />}
      <Button small kind="plain" onClick={done}>Cancel</Button>
      <Button small kind="primary" type="submit" disabled={busy || !shaped}>Save</Button>
      {err && <span className="err dmsg">{err}</span>}
    </form>
  );
}

const DRAFT = "d-draft";

function useRadius(map: MLMap, ring: Polygon | undefined) {
  useEffect(() => {
    const data: GeoJSON.FeatureCollection = { type: "FeatureCollection", features: ring ? [{ type: "Feature", properties: {}, geometry: ring }] : [] };
    const src = map.getSource(DRAFT) as { setData(d: GeoJSON.FeatureCollection): void } | undefined;
    if (src) src.setData(data);
    else if (ring) {
      map.addSource(DRAFT, { type: "geojson", data });
      map.addLayer({ id: DRAFT, type: "line", source: DRAFT, paint: { "line-color": "#F0936C", "line-width": 1, "line-dasharray": [2, 2] } }, "slot-points");
    }
  }, [map, ring]);
  useEffect(() => () => {
    try {
      if (map.getLayer(DRAFT)) map.removeLayer(DRAFT);
      if (map.getSource(DRAFT)) map.removeSource(DRAFT);
    } catch {
      /* the map went first */
    }
  }, [map]);
}

// terra-draw leaves polygon rings closed; make sure, since the server wants closed rings.
function closed(g: FeatureGeometry): FeatureGeometry {
  if (g.type !== "Polygon") return g;
  return {
    type: "Polygon",
    coordinates: g.coordinates.map((r) => {
      const [a, z] = [r[0], r[r.length - 1]];
      return a[0] === z[0] && a[1] === z[1] ? r : [...r, a as LonLat];
    }),
  };
}
