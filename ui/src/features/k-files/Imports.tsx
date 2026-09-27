// Data > Imports: a position file waiting in its preview (tracks drawn, labels matched to
// animals, columns and time zone to fix), then Replay of each import, with undo.

import { useEffect, useMemo, useRef, useState } from "react";
import type { Map as MLMap } from "maplibre-gl";
import type { Animal, CollarState } from "../../api";
import { files, type ImportLabel, type ImportTrack, type Mapping, type PositionImport, type PositionPreview, type TrackPoint } from "../../api/k-files";
import { C, createMap, fc, fitPolys, onLoad, setData } from "../../map/base";
import { addFarmLayers, Labels, paddockLabels, setPaddocks } from "../../map/layers";
import { Animals } from "../../map/animals";
import { useStore } from "../../store";
import { useCan } from "../../store/me";
import { kfiles, loadImports } from "../../store/k-files";
import { Button, Icon, Segmented } from "../../ui";
import { MappingRow } from "../../ui/MappingRow";
import { Table, type Column } from "../../ui/Table";
import { num } from "../../units";
import { at, choices, mappingFields, span } from "./logic";

export function ImportsSection() {
  const pending = kfiles.use((s) => s.pending);
  const imports = kfiles.use((s) => s.imports);
  const selected = kfiles.use((s) => s.selected);
  useEffect(() => {
    void loadImports();
  }, []);
  if (pending) return <Preview key={pending.import_id} p={pending} />;
  if (!imports?.length) return null;
  const sel = imports.find((i) => i.id === selected) ?? imports[0];
  return (
    <>
      <ImportReplay key={sel.id} imp={sel} />
      <ImportList imports={imports} selected={sel.id} />
    </>
  );
}

const when = (t: number | string) =>
  new Date(t).toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", hour12: false });
const day = (t: string) => new Date(t).toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
// "Jul 12, 2025", or "Jun 14, 2025 – Jun 15, 2025" when it spans days.
const dates = (i: PositionImport) => {
  if (!i.from || !i.to) return "–";
  const [a, b] = [day(i.from), day(i.to)];
  return a === b ? a : `${a} – ${b}`;
};

// ---- the map ----------------------------------------------------------------------

interface Line { id: string; points: TrackPoint[]; matched: boolean }

// Tracks over the paddocks with a time scrubber, like Replay: grass for tracks that belong to
// an animal, grey for those that don't (yet).
function TrackMap({ lines }: { lines: Line[] }) {
  const state = useStore((s) => s.state)!;
  const el = useRef<HTMLDivElement>(null);
  const [map, setMap] = useState<MLMap>();
  const animals = useRef<Animals>(null);
  const [pos, setPos] = useState(0);
  const [playing, setPlaying] = useState(false);

  useEffect(() => {
    const m = createMap(el.current!, { center: state.farm!.center, zoom: 15, dim: 0.6 });
    onLoad(m, () => {
      addFarmLayers(m);
      setPaddocks(m, state.paddocks);
      new Labels(m).set(paddockLabels(state.paddocks));
      setData(m, "ktracks", fc([]));
      m.addLayer({
        id: "ktracks", type: "line", source: "ktracks",
        paint: { "line-color": ["case", ["get", "matched"], C.grass, C.fg3], "line-opacity": ["case", ["get", "matched"], 0.45, 0.35], "line-width": 1 },
      });
      animals.current = new Animals(m);
      setMap(m);
    });
    return () => {
      animals.current?.destroy();
      m.remove();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const trackKey = lines.map((l) => `${l.id}:${l.points.length}:${l.matched}`).join();
  useEffect(() => {
    if (!map) return;
    setData(map, "ktracks", fc(lines.filter((l) => l.points.length > 1).map((l) => ({
      type: "Feature", properties: { matched: l.matched }, geometry: { type: "LineString", coordinates: l.points.map((p) => [p[0], p[1]]) },
    }))));
    const pts = lines.flatMap((l) => l.points.map((p) => [p[0], p[1]] as [number, number]));
    if (pts.length) fitPolys(map, [{ type: "Polygon", coordinates: [pts] }], 48);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [map, trackKey]);

  const [t0, t1] = useMemo(() => span(lines) ?? [0, 0], [lines]);
  const now = t0 + ((t1 - t0) * pos) / 1000;
  useEffect(() => {
    if (!animals.current) return;
    const s = now / 1000;
    animals.current.set(lines.flatMap((l) => {
      const p = at(l.points, s);
      return p ? [{ id: l.id, point: [p[0], p[1]] as [number, number], state: (l.matched ? "inside" : "unknown") as CollarState }] : [];
    }));
  }, [now, lines, map]);

  useEffect(() => {
    if (!playing) return;
    const t = setInterval(() => setPos((p) => (p >= 1000 ? (setPlaying(false), 1000) : p + 4)), 40);
    return () => clearInterval(t);
  }, [playing]);

  return (
    <div className="replay kfiles-map">
      <div ref={el} className="map" />
      <div className="scrub">
        <button type="button" className="play" aria-label={playing ? "Pause" : "Play"} onClick={() => { if (pos >= 1000) setPos(0); setPlaying(!playing); }}>
          {playing ? <span className="pause" /> : <Icon name="chev" size={7} />}
        </button>
        <input type="range" min={0} max={1000} value={pos} onChange={(e) => { setPlaying(false); setPos(Number(e.target.value)); }} aria-label="Time" />
        <span className="mono dim">{t1 > 0 ? when(now) : ""}</span>
      </div>
    </div>
  );
}

// ---- preview ----------------------------------------------------------------------

function Preview({ p }: { p: PositionPreview }) {
  const farmZone = useStore((s) => s.state?.farm?.timezone) ?? "UTC";
  const animals = useStore((s) => s.animals);
  const pickable = useMemo(() => choices(animals), [animals]);
  const [mapping, setMapping] = useState<Mapping>(p.mapping);
  const [zone, setZone] = useState(p.zone);
  // Label → animal id chosen here (null = leave the label out); others keep their match.
  const [assign, setAssign] = useState<Record<string, string | null>>({});
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();

  const reread = async (next: { mapping?: Mapping; zone?: string }) => {
    setErr(undefined);
    try {
      kfiles.patch({ pending: { ...(await files.rereadPositions(p.import_id, { mapping: next.mapping ?? mapping, zone: next.zone ?? zone })) } });
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  const animalOf = (l: ImportLabel) => (l.label in assign ? assign[l.label] ?? undefined : l.animal_id);
  const matched = p.labels.filter((l) => animalOf(l)).length;
  const lines = useMemo<Line[]>(
    () => p.tracks.map((t) => ({ id: t.label, points: t.points, matched: !!animalOf(p.labels.find((l) => l.label === t.label) ?? { label: t.label, points: 0, from: "", to: "" }) })),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [p, assign],
  );

  const commit = async () => {
    setBusy(true);
    setErr(undefined);
    try {
      const done = await files.commitPositions(p.import_id, { mapping, zone, animals: assign });
      kfiles.patch({ pending: undefined, selected: done.import.id });
      await loadImports();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };

  const columns: Column<ImportLabel>[] = [
    { id: "label", label: "tag", width: 140, sort: (a, b) => a.label.localeCompare(b.label, undefined, { numeric: true }), cell: (l) => l.label },
    {
      id: "animal", label: "animal", width: 150,
      cell: (l) => <AnimalPick value={animalOf(l)} animals={pickable} onChange={(id) => setAssign((a) => ({ ...a, [l.label]: id }))} />,
    },
    { id: "points", label: "points", width: 100, sort: (a, b) => a.points - b.points, cell: (l) => num(l.points) },
    { id: "from", label: "from", width: 150, cell: (l) => when(l.from) },
    { id: "to", label: "to", cell: (l) => when(l.to) },
  ];
  const zones = [...new Set([farmZone, "UTC"])];
  return (
    <div className="kfiles-preview">
      <TrackMap lines={lines} />
      <div className="kfiles-body">
        <p className="mono kfiles-head"><span>{p.file}</span><span className="dim">{num(p.points)} points  {matched}/{p.labels.length} tracks</span></p>
        {p.columns && p.columns.length > 0 && p.source !== "gpx" && (
          <MappingRow fields={mappingFields(p.source)} columns={p.columns} value={mapping} onChange={(m) => { setMapping(m); void reread({ mapping: m }); }} />
        )}
        {p.needs_zone && (
          <Segmented label="Time zone" value={zone} onChange={(z) => { setZone(z); void reread({ zone: z }); }} options={zones.map((z) => ({ value: z, label: <span className="mono">{z}</span> }))} />
        )}
        {p.labels.length > 0 && (
          <Table rows={p.labels} columns={columns} rowKey={(l) => l.label} height={Math.min(8, p.labels.length) * 37 + 34} className="kfiles-labels" />
        )}
        {p.errors.length > 0 && <div className="kfiles-errs">{p.errors.map((e, i) => <p key={i} className="mono dim">{e}</p>)}</div>}
        {err && <p className="mono err">{err}</p>}
        <div className="acts">
          <Button small kind="plain" onClick={() => kfiles.patch({ pending: undefined })}>Cancel</Button>
          <Button small kind="primary" disabled={busy || !matched} onClick={commit}>Import</Button>
        </div>
      </div>
    </div>
  );
}

function AnimalPick({ value, animals, onChange }: { value?: string; animals: Animal[]; onChange: (id: string | null) => void }) {
  return (
    <select className="input sm mono kfiles-pick" value={value ?? ""} aria-label="Animal" onChange={(e) => onChange(e.target.value || null)}>
      <option value="">–</option>
      {animals.map((a) => <option key={a.id} value={a.id}>{a.tag}</option>)}
    </select>
  );
}

// ---- imports ------------------------------------------------------------------------

function ImportReplay({ imp }: { imp: PositionImport }) {
  const [tracks, setTracks] = useState<ImportTrack[]>([]);
  useEffect(() => {
    void files.tracks({ import_id: imp.id, max_points: 400 }).then(setTracks).catch(() => setTracks([]));
  }, [imp.id]);
  const lines = useMemo(() => tracks.map((t) => ({ id: t.animal_id, points: t.points, matched: true })), [tracks]);
  return <TrackMap lines={lines} />;
}

function ImportList({ imports, selected }: { imports: PositionImport[]; selected: string }) {
  const manager = useCan("manager");
  const remove = async (id: string) => {
    await files.removeImport(id);
    await loadImports();
  };
  return (
    <table className="tbl kfiles-list">
      <thead><tr><th>file</th><th>animals</th><th>points</th><th>dates</th>{manager && <th />}</tr></thead>
      <tbody>
        {imports.map((i) => (
          <tr key={i.id} className={i.id === selected ? "on" : undefined} onClick={() => kfiles.patch({ selected: i.id })}>
            <td>{i.file_name}</td>
            <td>{num(i.animals)}</td>
            <td>{num(i.fixes)}</td>
            <td>{dates(i)}</td>
            {manager && <td className="kfiles-del"><Button small kind="plain" className="danger" onClick={(e) => { e.stopPropagation(); void remove(i.id); }}>Delete</Button></td>}
          </tr>
        ))}
      </tbody>
    </table>
  );
}
