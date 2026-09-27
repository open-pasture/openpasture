import { lazy, Suspense, useEffect, useState } from "react";
import { setToken } from "./api";
import "./features";
import { guarded, shortcuts, topbar, views } from "./registry";
import { start, store, useStore } from "./store";
import { Input, Mark } from "./ui";
import { typing, useHash, useKey } from "./util";

const FirstRun = lazy(() => import("./views/FirstRun").then((m) => ({ default: m.FirstRun })));
const Print = lazy(() => import("./views/Print").then((m) => ({ default: m.Print })));

export function App() {
  useEffect(() => start(), []);
  const ready = useStore((s) => s.ready);
  const error = useStore((s) => s.error);
  const state = useStore((s) => s.state);
  const up = useStore((s) => s.up);
  const needToken = useStore((s) => s.needToken);
  const [view, rest, go] = useHash();
  const nav = views.use();
  const bar = topbar.use();
  const keys = shortcuts.use();

  useKey((e) => {
    if (typing(e)) return;
    const v = nav.find((v) => v.key === e.key);
    if (v) return go(v.id);
    const k = keys.find((k) => k.key === e.key && (!k.when || k.when()));
    if (k) {
      e.preventDefault();
      k.run();
    }
  }, [nav, keys]);

  if (needToken) return <TokenPrompt />;
  if (!ready) return <div className="boot"><Mark size={22} /></div>;
  if (!state) return <div className="boot"><Mark size={22} /><p className="mono dim">{error}</p></div>;
  if (view === "print") return <Suspense fallback={null}><Print rest={rest} /></Suspense>;
  if (!state.farm || !state.paddocks.length || !state.herds.length) return <Suspense fallback={null}><FirstRun /></Suspense>;

  // An unknown view shows the first one (the map).
  const cur = nav.find((v) => v.id === view) ?? nav[0];
  return (
    <div className="shell">
      <header className="topbar">
        <a className="brand" href="#/map"><Mark />{state.farm.name}</a>
        <nav aria-label="Views">
          {nav.map((v) => (
            <a key={v.id} href={`#/${v.id}`} aria-current={cur?.id === v.id ? "page" : undefined}>{v.label}</a>
          ))}
        </nav>
        <div className="tright">
          {bar.map((t) => guarded(t.id, <t.Item />))}
          {!up && <span className="offline mono">offline</span>}
        </div>
      </header>
      <main>
        <Suspense fallback={null}>
          {cur && guarded(cur.id, <cur.View rest={cur.id === view ? rest : ""} />)}
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
