import { lazy, Suspense, useEffect, useState } from "react";
import { getToken, setToken } from "./api";
import "./features";
import { joinCode } from "./features/j/signin";
import { guarded, shortcuts, topbar, views } from "./registry";
import { start, store, useStore } from "./store";
import { useCan } from "./store/me";
import { Input, Mark } from "./ui";
import { typing, useHash, useKey } from "./util";

const FirstRun = lazy(() => import("./views/FirstRun").then((m) => ({ default: m.FirstRun })));
const Print = lazy(() => import("./views/Print").then((m) => ({ default: m.Print })));
const Join = lazy(() => import("./features/j/Join").then((m) => ({ default: m.Join })));

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
  // Setting up the farm writes it, which managers and owners do.
  const setsUp = useCan("manager");

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

  // A sign-in link opens before any sign-in: it is one.
  if (view === "join") return <Suspense fallback={null}><Join code={rest} /></Suspense>;
  if (needToken) return <TokenPrompt />;
  if (!ready) return <div className="boot"><Mark size={22} /></div>;
  if (!state) return <div className="boot"><Mark size={22} /><p className="mono dim">{error}</p></div>;
  if (view === "print") return <Suspense fallback={null}><Print rest={rest} /></Suspense>;
  if (!state.farm || !state.paddocks.length || !state.herds.length) {
    return setsUp ? <Suspense fallback={null}><FirstRun /></Suspense> : <div className="boot"><Mark size={22} /></div>;
  }

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

// The app token, a person's own token, or a sign-in link pasted whole. Empty tries again
// without one (this machine needs none).
function TokenPrompt() {
  const [v, setV] = useState("");
  // A person's token that was just refused was revoked: forget it.
  useEffect(() => {
    if (getToken().startsWith("opu_")) setToken("");
  }, []);
  const submit = () => {
    const code = joinCode(v);
    if (code) return location.replace(`#/join/${code}`);
    setToken(v.trim());
    store.tokenSaved();
  };
  return (
    <form className="boot" onSubmit={(e) => { e.preventDefault(); submit(); }}>
      <Mark size={22} />
      <Input mono autoFocus type="password" placeholder="Token" aria-label="Token" value={v} onChange={(e) => setV(e.target.value)} className="token" />
    </form>
  );
}
