import { lazy, Suspense, useEffect, useState } from "react";
import { setToken } from "./api";
import { start, store, useStore } from "./store";
import { Input, Mark } from "./ui";
import { typing, useHash, useKey } from "./util";

// Map views pull in MapLibre and terra-draw; load them on demand.
const MapView = lazy(() => import("./views/MapView").then((m) => ({ default: m.MapView })));
const FirstRun = lazy(() => import("./views/FirstRun").then((m) => ({ default: m.FirstRun })));
const DataView = lazy(() => import("./views/Data").then((m) => ({ default: m.DataView })));
const SettingsView = lazy(() => import("./views/Settings").then((m) => ({ default: m.SettingsView })));

const VIEWS = [
  { id: "map", label: "Map", key: "m" },
  { id: "data", label: "Data", key: "d" },
  { id: "settings", label: "Settings", key: "s" },
] as const;

export function App() {
  useEffect(() => start(), []);
  const ready = useStore((s) => s.ready);
  const error = useStore((s) => s.error);
  const state = useStore((s) => s.state);
  const up = useStore((s) => s.up);
  const needToken = useStore((s) => s.needToken);
  const [view, go] = useHash();

  useKey((e) => {
    if (typing(e)) return;
    const v = VIEWS.find((v) => v.key === e.key);
    if (v) go(v.id);
  }, []);

  if (needToken) return <TokenPrompt />;
  if (!ready) return <div className="boot"><Mark size={22} /></div>;
  if (!state) return <div className="boot"><Mark size={22} /><p className="mono dim">{error}</p></div>;
  if (!state.farm || !state.paddocks.length || !state.herds.length) return <Suspense fallback={null}><FirstRun /></Suspense>;

  return (
    <div className="shell">
      <header className="topbar">
        <a className="brand" href="#/map"><Mark />{state.farm.name}</a>
        <nav aria-label="Views">
          {VIEWS.map((v) => (
            <a key={v.id} href={`#/${v.id}`} aria-current={view === v.id ? "page" : undefined}>{v.label}</a>
          ))}
        </nav>
        {!up && <span className="offline mono">offline</span>}
      </header>
      <main>
        <Suspense fallback={null}>
          {view === "data" ? <DataView /> : view === "settings" ? <SettingsView /> : <MapView />}
        </Suspense>
      </main>
    </div>
  );
}

function TokenPrompt() {
  const [v, setV] = useState("");
  return (
    <form className="boot" onSubmit={(e) => { e.preventDefault(); if (!v.trim()) return; setToken(v.trim()); store.tokenSaved(); }}>
      <Mark size={22} />
      <Input mono autoFocus type="password" placeholder="App token" aria-label="App token" value={v} onChange={(e) => setV(e.target.value)} className="token" />
    </form>
  );
}
