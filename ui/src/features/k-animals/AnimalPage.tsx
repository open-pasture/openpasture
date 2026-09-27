import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { Map as MLMap } from "maplibre-gl";
import { api, type Animal, type Collar, type LonLat, type ParkReason, type Polygon, type RemovedReason } from "../../api";
import { inside } from "../../geo";
import { kAnimals, type LinkedCollar } from "../../api/k-animals";
import { C, createMap, fc, fitPolys, onLoad, setData } from "../../map/base";
import { addFarmLayers, Labels, paddockLabels, setPaddocks } from "../../map/layers";
import { animalPage, interleave, sectionNodes, useSections } from "../../registry";
import { store, useStore } from "../../store";
import { useCan } from "../../store/me";
import { keepBatch } from "../../store/k-animals";
import { Button, Copy, Input, Menu, Segmented } from "../../ui";
import { useUnits } from "../../units";
import { age, useNow } from "../../util";
import { Edit, type EditField } from "./Edit";
import { battery, cardsPossible, day, reasonWord, REASONS, resolve, spareCollars } from "./herd";

const PARK: { value: ParkReason; label: string }[] = [{ value: "charging", label: "Charging" }, { value: "shelf", label: "Shelf" }, { value: "repair", label: "Repair" }];

// #/herd/<tag>: what we know about one animal, where it has been, and what can be done
// with it and its collar. Other streams add sections (animalPage).
export function AnimalPage({ rest }: { rest: string }) {
  const state = useStore((s) => s.state)!;
  const herdId = useStore((s) => s.herdId);
  const animals = useStore((s) => s.animals);
  const collars = useStore((s) => s.collars);
  const { animal, collar } = useMemo(() => resolve(rest, animals, collars, herdId), [rest, animals, collars, herdId]);
  const props = { animal, collar };
  const sections = sectionNodes(useSections(animalPage, props), props);
  const key = decodeURIComponent(rest.split(/[/?]/)[0]);

  if (!animal && !collar)
    return (
      <div className="apage">
        <div className="aside"><a className="back mono" href="#/herd">Herd</a><p className="dim">Nothing here is tagged {key}.</p></div>
      </div>
    );
  return (
    <div className="apage">
      <div className="aside">
        <a className="back mono" href="#/herd">Herd</a>
        {interleave([
          { key: "facts", order: 10, node: <Facts animal={animal} collar={collar} herdName={state.herds.find((h) => h.id === (animal?.herd_id ?? collar?.herd_id))?.name} /> },
          { key: "actions", order: 90, node: <Actions animal={animal} collar={collar} /> },
        ], sections)}
      </div>
      <TrackMap key={(animal?.id ?? "") + (collar?.id ?? "")} animal={animal} collar={collar} />
    </div>
  );
}

function Facts({ animal, collar, herdName }: { animal?: Animal; collar?: Collar; herdName?: string }) {
  const canEdit = useCan("manager");
  const units = useUnits();
  const now = useNow(5000);
  const [err, setErr] = useState<string>();
  // Empty facts show only to someone who can fill them in.
  const editable = canEdit && !animal?.removed_at;
  const row = (label: string, field: EditField) =>
    animal && (editable || animal[field]) ? <li key={field}><span>{label}</span><b><Edit a={animal} field={field} can={canEdit} onError={setErr} /></b></li> : null;
  const fix = collar?.last_fix;
  return (
    <section className="afacts">
      <h1 className="atag">
        {animal ? <Edit a={animal} field="tag" can={canEdit} onError={setErr} /> : collar?.name}
        {animal?.name && <span className="aname"><Edit a={animal} field="name" can={canEdit} onError={setErr} /></span>}
      </h1>
      {animal?.removed_at && <p className="mono dim">{reasonWord(animal.removed_reason)} {day(animal.removed_at)}</p>}
      <ul className="kv">
        {!animal?.name && row("name", "name")}
        {row("EID", "eid")}
        {row("breed", "breed")}
        {row("sex", "sex")}
        {row("born", "born")}
        {herdName && <li><span>herd</span><b>{herdName}</b></li>}
        {collar && (
          <li><span>collar</span><b>{collar.name}{collar.parked_reason ? `  ${collar.parked_reason}` : `  ${battery(collar.battery)}  ${age(collar.last_seen, now)}`}</b></li>
        )}
        {fix && !collar?.parked_at && <li><span>fix</span><b>{age(fix.at, now)}  ±{units.len(fix.accuracy_m)}</b></li>}
        {row("notes", "notes")}
      </ul>
      {err && <p className="mono err">{err}</p>}
    </section>
  );
}

function Actions({ animal, collar }: { animal?: Animal; collar?: Collar }) {
  const canManage = useCan("manager");
  const canHand = useCan("hand");
  const isOwner = useCan("owner");
  const animals = useStore((s) => s.animals);
  const collars = useStore((s) => s.collars);
  const publicUrl = useStore((s) => s.state?.settings.server.public_url);
  const [open, setOpen] = useState<"remove" | "rekey">();
  const [reason, setReason] = useState<RemovedReason>("sold");
  const [date, setDate] = useState(() => new Date().toLocaleDateString("en-CA"));
  const [keyed, setKeyed] = useState<LinkedCollar>();
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  const herdId = animal?.herd_id ?? collar?.herd_id ?? "";
  const spares = animal && !animal.removed_at ? spareCollars(collars, animals, herdId) : [];

  const act = async (f: () => Promise<unknown>) => {
    setBusy(true);
    setErr(undefined);
    try {
      await f();
      setOpen(undefined);
      await store.refresh();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const remove = () => act(async () => {
    // Today means now; another day means that day, midday.
    const today = new Date().toLocaleDateString("en-CA");
    await kAnimals.remove(animal!.id, reason, date && date !== today ? new Date(`${date}T12:00`).toISOString() : undefined);
  });
  const rekey = () => act(async () => {
    const k = await kAnimals.rekey(collar!.id);
    if (cardsPossible(publicUrl)) {
      const id = `key-${collar!.id}-${Date.now()}`;
      keepBatch(id, [k]);
      location.hash = `/print/cards/${id}`;
    } else setKeyed(k);
  });

  if (keyed)
    return (
      <section className="aacts keyed">
        <Copy label="endpoint" value={keyed.endpoint} />
        <Copy label="key" value={keyed.key} />
        <Copy label="public key" value={keyed.public_key} />
        <Button small kind="plain" onClick={() => setKeyed(undefined)}>Done</Button>
      </section>
    );

  const buttons: ReactNode[] = [];
  if (canManage && spares.length)
    buttons.push(
      <Menu key="swap" trigger={<span className="btn quiet sm">{animal?.collar_id ? "Swap collar" : "Link collar"}</span>}
        items={spares.map((c) => ({ label: <span className="mono">{c.name}</span>, onSelect: () => act(() => kAnimals.swap(animal!.id, c.id)) }))} />,
    );
  if (canHand && collar && !collar.parked_at)
    buttons.push(<Menu key="park" trigger={<span className="btn quiet sm">Park</span>} items={PARK.map((p) => ({ label: p.label, onSelect: () => act(() => kAnimals.park(collar.id, p.value)) }))} />);
  if (canHand && collar?.parked_at)
    buttons.push(<Button key="unpark" small disabled={busy} onClick={() => act(() => kAnimals.unpark(collar.id))}>Unpark</Button>);
  if (isOwner && collar) buttons.push(<Button key="rekey" small kind="plain" disabled={busy} onClick={() => setOpen(open === "rekey" ? undefined : "rekey")}>New key</Button>);
  if (canManage && animal && !animal.removed_at)
    buttons.push(<Button key="remove" small kind="plain" className="danger" disabled={busy} onClick={() => setOpen(open === "remove" ? undefined : "remove")}>Remove</Button>);
  if (!buttons.length && !err) return null;

  return (
    <section className="aacts">
      <div className="acts">{buttons}</div>
      {open === "remove" && (
        <form className="aform" onSubmit={(e) => { e.preventDefault(); void remove(); }}>
          <Segmented label="Why" value={reason} onChange={setReason} options={REASONS} />
          <div className="acts">
            <Input type="date" className="sm mono" aria-label="When" value={date} max={new Date().toLocaleDateString("en-CA")} onChange={(e) => setDate(e.target.value)} />
            <Button small kind="plain" onClick={() => setOpen(undefined)}>Cancel</Button>
            <Button small kind="plain" className="danger" type="submit" disabled={busy}>Remove {animal?.tag}</Button>
          </div>
        </form>
      )}
      {open === "rekey" && (
        <div className="aform">
          <p className="mono dim">The old key stops working until the collar is set up again.</p>
          <div className="acts">
            <Button small kind="plain" onClick={() => setOpen(undefined)}>Cancel</Button>
            <Button small kind="plain" className="danger" disabled={busy} onClick={rekey}>New key</Button>
          </div>
        </div>
      )}
      {err && <p className="mono err">{err}</p>}
    </section>
  );
}

type RangeKey = "24h" | "7d" | "30d";
const SPAN: Record<RangeKey, number> = { "24h": 86400e3, "7d": 7 * 86400e3, "30d": 30 * 86400e3 };

// The animal's latest fix as a square, the collar's track over the range, and imported
// history as a fainter dashed line. Nothing to show, no map.
function TrackMap({ animal, collar }: { animal?: Animal; collar?: Collar }) {
  const state = useStore((s) => s.state)!;
  const el = useRef<HTMLDivElement>(null);
  const [map, setMap] = useState<MLMap>();
  const [range, setRange] = useState<RangeKey>("24h");
  const [track, setTrack] = useState<LonLat[]>([]);
  const [old, setOld] = useState<LonLat[][]>([]);
  const [fix, setFix] = useState<LonLat | undefined>(collar?.parked_at ? undefined : collar?.last_fix?.point);
  const fitted = useRef(false);
  const has = track.length > 0 || old.some((t) => t.length > 0) || !!fix;

  useEffect(() => {
    const m = createMap(el.current!, { center: state.farm!.center, zoom: 16, dim: 0.6 });
    onLoad(m, () => {
      addFarmLayers(m);
      setPaddocks(m, state.paddocks);
      new Labels(m).set(paddockLabels(state.paddocks));
      for (const id of ["a-old", "a-track", "a-fix"]) setData(m, id, fc([]));
      m.addImage("a-sq", square(C.grass, 9), { pixelRatio: 2 });
      m.addLayer({ id: "a-old", type: "line", source: "a-old", paint: { "line-color": C.fg2, "line-opacity": 0.45, "line-width": 1, "line-dasharray": [2, 2] } });
      m.addLayer({ id: "a-track", type: "line", source: "a-track", layout: { "line-join": "round", "line-cap": "round" }, paint: { "line-color": C.grass, "line-opacity": 0.7, "line-width": 1.5 } });
      m.addLayer({ id: "a-fix", type: "symbol", source: "a-fix", layout: { "icon-image": "a-sq", "icon-allow-overlap": true, "icon-ignore-placement": true } });
      setMap(m);
    });
    return () => m.remove();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!collar || collar.parked_at) return void setTrack([]);
    const to = Date.now();
    api.tracks({ collar_id: collar.id, from: new Date(to - SPAN[range]).toISOString(), to: new Date(to).toISOString(), max_points: 600 })
      .then((t) => setTrack(t.flatMap((x) => x.points.map((p) => [p[0], p[1]] as LonLat))), () => setTrack([]));
  }, [collar?.id, collar?.parked_at, range]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!animal) return;
    // Imported history (position files) comes from its own endpoint; none is fine.
    kAnimals.importedTracks(animal.id).then((t) => setOld(t.map((x) => x.points.map((p) => [p[0], p[1]] as LonLat))), () => setOld([]));
  }, [animal?.id]); // eslint-disable-line react-hooks/exhaustive-deps

  // The square follows the collar's live fixes.
  useEffect(() => {
    if (!collar || collar.parked_at) return;
    return store.onFix((e) => e.collar_id === collar.id && setFix(e.fix.point));
  }, [collar?.id, collar?.parked_at]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!map) return;
    const line = (pts: LonLat[]): GeoJSON.Feature => ({ type: "Feature", properties: {}, geometry: { type: "LineString", coordinates: pts } });
    setData(map, "a-old", fc(old.filter((t) => t.length > 1).map(line)));
    setData(map, "a-track", fc(track.length > 1 ? [line(fix ? [...track, fix] : track)] : []));
    setData(map, "a-fix", fc(fix ? [{ type: "Feature", properties: {}, geometry: { type: "Point", coordinates: fix } }] : []));
    map.resize();
    const pts = [...track, ...old.flat(), ...(fix ? [fix] : [])];
    if (!fitted.current && pts.length) {
      fitted.current = true;
      // Where it has been, and the paddock it is in now.
      const lon = pts.map((p) => p[0]), lat = pts.map((p) => p[1]);
      const [w, s, e, n] = [Math.min(...lon), Math.min(...lat), Math.max(...lon), Math.max(...lat)];
      const around: Polygon = { type: "Polygon", coordinates: [[[w, s], [e, s], [e, n], [w, n], [w, s]]] };
      const here = pts[pts.length - 1];
      const pad = state.paddocks.find((p) => inside(here, p.geometry));
      fitPolys(map, pad ? [around, pad.geometry] : [around], 48);
    }
  }, [map, track, old, fix]);

  return (
    <div className="amap" hidden={!has}>
      <div ref={el} className="map" />
      {collar && !collar.parked_at && (
        <div className="over">
          <Segmented label="Range" value={range} onChange={setRange} options={[{ value: "24h", label: "24 h" }, { value: "7d", label: "7 d" }, { value: "30d", label: "30 d" }]} />
        </div>
      )}
    </div>
  );
}

// A filled pixel square with a dark edge, like the animals on the main map.
function square(fill: string, size: number) {
  const n = size * 2;
  const data = new Uint8Array(n * n * 4);
  const rgb = [1, 3, 5].map((i) => parseInt(fill.slice(i, i + 2), 16));
  const ink = [1, 3, 5].map((i) => parseInt(C.ink.slice(i, i + 2), 16));
  for (let y = 0; y < n; y++)
    for (let x = 0; x < n; x++) {
      const edge = x < 2 || y < 2 || x >= n - 2 || y >= n - 2;
      data.set(edge ? [...ink, 220] : [...rgb, 255], (y * n + x) * 4);
    }
  return { width: n, height: n, data };
}
