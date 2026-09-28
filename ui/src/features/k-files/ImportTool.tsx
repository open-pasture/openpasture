// Draw > Import file. A paddock file (GeoJSON, KML, KMZ, zipped shapefile) shows its polygons
// as dashed drafts: click one to drop it (again to bring it back), Save keeps the rest as
// paddocks. A position file (CSV, GPX, GeoJSON points) goes to Data, where it is previewed.

import { useCallback, useEffect, useRef, useState } from "react";
import type { GeoJSONSource, MapMouseEvent } from "maplibre-gl";
import { files, type PaddockPreview } from "../../api/k-files";
import { C, fc, fitPolys } from "../../map/base";
import { Labels } from "../../map/layers";
import type { ToolProps } from "../../map/tools";
import { interiorPoint } from "../../geo";
import { store } from "../../store";
import { kfiles } from "../../store/k-files";
import { Button } from "../../ui";
import { FilePick } from "../../ui/FilePick";
import { useUnits } from "../../units";
import { ACCEPT, fileKind, kept } from "./logic";

const SRC = "kfiles-drafts";
const FILL = "kfiles-drafts-fill";
const LINE = "kfiles-drafts-line";

export function ImportTool({ map, ctx, done }: ToolProps) {
  const u = useUnits();
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  const [prev, setPrev] = useState<PaddockPreview>();
  const [dropped, setDropped] = useState<Set<number>>(new Set());
  const labels = useRef<Labels>(null);
  const input = useRef<HTMLInputElement>(null);

  const pick = useCallback(async (file: File) => {
    setErr(undefined);
    setBusy(true);
    try {
      const kind = fileKind(file.name, await file.slice(0, 65536).text());
      const positions = async () => {
        kfiles.patch({ pending: await files.previewPositions(file) });
        done();
        location.hash = "/data";
      };
      if (kind === "positions") return await positions();
      let p;
      try {
        p = await files.previewPaddocks(file);
      } catch (e) {
        // A GeoJSON we couldn't place from its start: points are position history.
        if (kind === undefined && /no polygons/.test((e as Error).message)) return await positions();
        throw e;
      }
      setDropped(new Set());
      setPrev(p);
      if (!p.drafts.length) setErr(p.errors[0] ?? "The file has no polygons.");
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  }, [done]);

  // Open the file chooser at once; the button stays for a second try or a drop.
  useEffect(() => {
    input.current?.click();
  }, []);

  // Drafts on the map: dashed outlines in the plan slot, dimmed once dropped.
  useEffect(() => {
    if (!prev?.drafts.length) return;
    const features = prev.drafts.map((d, i) => ({ type: "Feature" as const, id: i + 1, properties: { i }, geometry: d.geometry }));
    map.addSource(SRC, { type: "geojson", data: fc(features) });
    map.addLayer({ id: FILL, type: "fill", source: SRC, paint: { "fill-color": C.fg, "fill-opacity": ["case", ["boolean", ["feature-state", "dropped"], false], 0, 0.06] } }, ctx.beforeId("slot-plan"));
    map.addLayer({
      id: LINE, type: "line", source: SRC,
      paint: { "line-color": C.fg, "line-width": 1.5, "line-dasharray": [3, 2.5], "line-opacity": ["case", ["boolean", ["feature-state", "dropped"], false], 0.25, 0.9] },
    }, ctx.beforeId("slot-plan"));
    labels.current = new Labels(map);
    fitPolys(map, prev.drafts.map((d) => d.geometry), 96, true);
    const click = (e: MapMouseEvent & { features?: GeoJSON.Feature[] }) => {
      const i = e.features?.[0]?.properties?.i as number | undefined;
      if (i === undefined) return;
      setDropped((s) => {
        const next = new Set(s);
        if (next.has(i)) next.delete(i);
        else next.add(i);
        return next;
      });
    };
    const enter = () => (map.getCanvas().style.cursor = "pointer");
    const leave = () => (map.getCanvas().style.cursor = "");
    map.on("click", FILL, click);
    map.on("mouseenter", FILL, enter);
    map.on("mouseleave", FILL, leave);
    return () => {
      try {
        map.off("click", FILL, click);
        map.off("mouseenter", FILL, enter);
        map.off("mouseleave", FILL, leave);
        map.getCanvas().style.cursor = "";
        labels.current?.clear();
        if (map.getLayer(LINE)) map.removeLayer(LINE);
        if (map.getLayer(FILL)) map.removeLayer(FILL);
        if (map.getSource(SRC)) map.removeSource(SRC);
      } catch {
        /* the map went first (leaving the view) */
      }
    };
  }, [map, ctx, prev]);

  useEffect(() => {
    if (!prev?.drafts.length || !(map.getSource(SRC) as GeoJSONSource | undefined)) return;
    prev.drafts.forEach((_, i) => map.setFeatureState({ source: SRC, id: i + 1 }, { dropped: dropped.has(i) }));
    labels.current?.set(prev.drafts.flatMap((d, i) => (dropped.has(i) ? [] : [{ id: `kf${i}`, at: interiorPoint(d.geometry), text: d.name }])));
  }, [map, prev, dropped]);

  const save = async () => {
    if (!prev) return;
    const { keep } = kept(prev.drafts, dropped);
    if (!keep.length) return;
    setBusy(true);
    try {
      await files.commitPaddocks({ import_id: prev.import_id, keep });
      done();
      await store.refresh();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const foot = err || (prev?.errors.length ?? 0) > 0;
  if (!prev?.drafts.length)
    return (
      <>
        <FilePick accept={ACCEPT} onPick={pick} disabled={busy}>{busy ? "Reading" : "Choose file"}</FilePick>
        <input ref={input} type="file" accept={ACCEPT} hidden onChange={(e) => {
          const f = e.target.files?.[0];
          e.target.value = "";
          if (f) void pick(f);
        }} />
        <Button small kind="plain" onClick={done}>Cancel</Button>
        {err && <div className="toolfoot kfiles-foot"><p className="mono err">{err}</p></div>}
      </>
    );
  const k = kept(prev.drafts, dropped);
  return (
    <>
      <form className="toolform" onSubmit={(e) => { e.preventDefault(); void save(); }}>
        <span className="mono kfiles-count">{k.keep.length}/{prev.drafts.length}  {u.area(k.areaHa)}</span>
        <Button small kind="plain" onClick={done}>Cancel</Button>
        <Button small kind="primary" type="submit" disabled={busy || !k.keep.length}>Save</Button>
      </form>
      {foot && (
        <div className="toolfoot kfiles-foot">
          {err && <p className="mono err">{err}</p>}
          {prev.errors.map((e, i) => <p key={i} className="mono dim">{e}</p>)}
        </div>
      )}
    </>
  );
}
