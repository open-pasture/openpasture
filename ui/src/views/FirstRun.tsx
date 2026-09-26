import { useEffect, useRef, useState } from "react";
import * as maplibregl from "maplibre-gl";
import type { Map as MLMap } from "maplibre-gl";
import { api, type LonLat, type Species } from "../api";
import { store, useStore } from "../store";
import { createMap, fitPolys, onLoad } from "../map/base";
import { addFarmLayers, Labels, paddockLabels, setPaddocks } from "../map/layers";
import { createDraw, current, type Draw } from "../map/draw";
import { Button, Icon, Input, Mark, Segmented } from "../ui";

type Step = "name" | "place" | "paddock" | "herd";
const STEPS: Step[] = ["name", "place", "paddock", "herd"];

// One question per screen: name, place, first paddock, herd.
export function FirstRun() {
  const state = useStore((s) => s.state)!;
  const [step, setStep] = useState<Step>(() => (!state.farm ? "name" : !state.paddocks.length ? "paddock" : "herd"));
  const [name, setName] = useState("");
  const [center, setCenter] = useState<LonLat>();
  const [drawn, setDrawn] = useState(false);
  const [pname, setPname] = useState("P1");
  const [herd, setHerd] = useState({ name: "Herd 1", count: "12", species: "cattle" as Species });
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  const [zoom, setZoom] = useState(0);

  const el = useRef<HTMLDivElement>(null);
  const map = useRef<MLMap>(null);
  const draw = useRef<Draw>(null);
  const pin = useRef<maplibregl.Marker>(null);
  const labels = useRef<Labels>(null);
  const stepRef = useRef(step);
  stepRef.current = step;

  useEffect(() => {
    const m = createMap(el.current!, state.farm ? { center: state.farm.center, zoom: 16 } : { zoom: 3.2, center: [-96, 38] });
    map.current = m;
    onLoad(m, () => {
      addFarmLayers(m);
      labels.current = new Labels(m);
      draw.current = createDraw(m);
      draw.current.on("finish", (id, ctx) => {
        if (ctx.action !== "draw") return;
        draw.current!.setMode("edit");
        draw.current!.selectFeature(id);
        setDrawn(true);
      });
      if (stepRef.current === "paddock") draw.current.setMode("paddock");
      const ps = store.get().state?.paddocks ?? [];
      setPaddocks(m, ps);
      labels.current.set(paddockLabels(ps));
      if (ps.length) fitPolys(m, ps.map((p) => p.geometry), 120);
    });
    m.on("zoomend", () => setZoom(m.getZoom()));
    m.on("click", (e) => {
      if (stepRef.current !== "place") return;
      const c: LonLat = [e.lngLat.lng, e.lngLat.lat];
      setCenter(c);
      if (!pin.current) {
        const d = document.createElement("div");
        d.className = "pin";
        pin.current = new maplibregl.Marker({ element: d, anchor: "bottom" });
      }
      pin.current.setLngLat(c).addTo(m);
    });
    return () => m.remove();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const run = async (f: () => Promise<void>) => {
    setBusy(true);
    setErr(undefined);
    try {
      await f();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const search = (q: string) => run(async () => {
    const r = await fetch(`https://nominatim.openstreetmap.org/search?format=json&limit=1&q=${encodeURIComponent(q)}`, {
      headers: { "Accept-Language": navigator.language },
    });
    const [hit] = (await r.json()) as { lat: string; lon: string; boundingbox: [string, string, string, string] }[];
    if (!hit) throw new Error("Not found");
    const [s, n, w, e] = hit.boundingbox.map(Number);
    map.current?.fitBounds([[w, s], [e, n]], { maxZoom: 16, duration: 1200, padding: 40 });
  });

  const toPlace = () => {
    if (!name.trim()) return;
    setStep("place");
  };

  const createFarm = () => run(async () => {
    const c = center ?? (map.current ? (map.current.getCenter().toArray() as LonLat) : undefined);
    if (!c) return;
    await api.createFarm({ name: name.trim(), timezone: Intl.DateTimeFormat().resolvedOptions().timeZone, center: c });
    pin.current?.remove();
    if (map.current && map.current.getZoom() < 15) map.current.easeTo({ center: c, zoom: 16 });
    draw.current?.setMode("paddock");
    setStep("paddock");
  });

  const savePaddock = () => run(async () => {
    const g = draw.current && current(draw.current);
    if (!g) return;
    const p = await api.createPaddock({ name: pname.trim() || "P1", geometry: g });
    draw.current!.clear();
    draw.current!.setMode("static");
    if (map.current) {
      setPaddocks(map.current, [p]);
      labels.current?.set(paddockLabels([p]));
    }
    setStep("herd");
  });

  const saveHerd = () => run(async () => {
    const count = Math.max(1, Math.min(500, parseInt(herd.count, 10) || 1));
    const paddock_id = (await api.paddocks())[0]?.id;
    await api.createHerd({ name: herd.name.trim() || "Herd 1", species: herd.species, count, paddock_id });
    await store.refresh();
  });

  return (
    <div className="first" data-step={step}>
      <div ref={el} className="map" />
      <div className="veil" />
      <div className="corner">
        <Mark size={18} />
        <span className="dots" aria-label={`Step ${STEPS.indexOf(step) + 1} of 4`}>
          {STEPS.map((s) => <i key={s} className={s === step ? "on" : STEPS.indexOf(s) < STEPS.indexOf(step) ? "done" : undefined} />)}
        </span>
      </div>

      {step === "name" && (
        <form className="q center" onSubmit={(e) => { e.preventDefault(); toPlace(); }}>
          <Input big autoFocus placeholder="Farm name" aria-label="Farm name" value={name} onChange={(e) => setName(e.target.value)} />
          <button className="go" type="submit" aria-label="Next" disabled={!name.trim()}><Icon name="arrow" size={15} /></button>
        </form>
      )}

      {step === "place" && (
        <>
          <form className="q top" onSubmit={(e) => { e.preventDefault(); const v = new FormData(e.currentTarget).get("q"); if (v) void search(String(v)); }}>
            <Input name="q" autoFocus placeholder="Find the farm" aria-label="Find the farm" />
          </form>
          {(center || zoom >= 13) && (
            <div className="q bottom">
              <Button kind="primary" disabled={busy} onClick={createFarm}>Here</Button>
            </div>
          )}
        </>
      )}

      {step === "paddock" && (
        <div className="q bottom">
          {!drawn ? (
            <p className="ask">Draw the first paddock</p>
          ) : (
            <form className="row" onSubmit={(e) => { e.preventDefault(); void savePaddock(); }}>
              <Input autoFocus value={pname} onChange={(e) => setPname(e.target.value)} aria-label="Paddock name" />
              <button className="go" type="submit" aria-label="Save" disabled={busy}><Icon name="arrow" size={15} /></button>
            </form>
          )}
        </div>
      )}

      {step === "herd" && (
        <form className="q bottom herd row" onSubmit={(e) => { e.preventDefault(); void saveHerd(); }}>
          <Input autoFocus value={herd.name} onChange={(e) => setHerd({ ...herd, name: e.target.value })} aria-label="Herd name" />
          <Input mono className="count" inputMode="numeric" value={herd.count} onChange={(e) => setHerd({ ...herd, count: e.target.value.replace(/\D/g, "") })} aria-label="Head" />
          <Segmented label="Species" value={herd.species} onChange={(species) => setHerd({ ...herd, species })}
            options={[{ value: "cattle", label: "Cattle" }, { value: "sheep", label: "Sheep" }, { value: "goats", label: "Goats" }]} />
          <button className="go" type="submit" aria-label="Done" disabled={busy}><Icon name="arrow" size={15} /></button>
        </form>
      )}

      {err && <p className="err mono">{err}</p>}
    </div>
  );
}
