import { lazy, Suspense, useCallback, useEffect, useState } from "react";
import { getToken, setToken } from "./api";
import "./features";
import { joinCode } from "./features/j/signin";
import { guarded, shortcuts, topbar, views } from "./registry";
import { start, store, useStore } from "./store";
import { useCan, useMe } from "./store/me";
import { Icon, Input, Mark } from "./ui";
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
      <header className="topbar side">
        <a className="brand" href="#/map"><Mark /><span className="bname">{state.farm.name}</span></a>
        <nav aria-label="Views">
          {nav.map((v) => (
            <a key={v.id} href={`#/${v.id}`} aria-current={cur?.id === v.id ? "page" : undefined}>
              {v.icon && <Icon name={v.icon} size={14} accent="currentColor" className="nicon" />}
              <span className="nlabel">{v.label}</span>
              <kbd>{v.key.toUpperCase()}</kbd>
            </a>
          ))}
        </nav>
        <Herds onPick={() => { if (cur && cur.id !== "map" && cur.id !== "herd") go("map"); }} />
        <div className="tright">
          {bar.map((t) => guarded(t.id, <t.Item />))}
          {!up && <span className="offline mono">offline</span>}
        </div>
        <div className="sfoot">
          <You />
          <UpdateButton />
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

// The farm's herds, as a list to switch between (the desktop sidebar only).
function Herds({ onPick }: { onPick: () => void }) {
  const herds = useStore((s) => s.state?.herds ?? []);
  const herdId = useStore((s) => s.herdId);
  if (herds.length < 2) return null;
  return (
    <ul className="herds" aria-label="Herds">
      {herds.map((h) => (
        <li key={h.id}>
          <button type="button" aria-current={h.id === herdId ? "true" : undefined}
            onClick={() => { store.setHerd(h.id); onPick(); }}>
            <span className="hn">{h.name}</span>
            <span className="mono hc">{h.count}</span>
          </button>
        </li>
      ))}
    </ul>
  );
}

// The desktop app's updater, over the two commands its capability allows the served UI.
// In a browser there is no updater, so nothing shows.
type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
interface UpdateStatus { version: string; available?: string | null; busy: boolean }
const tauriInvoke = (): Invoke | undefined =>
  (window as unknown as { __TAURI_INTERNALS__?: { invoke?: Invoke } }).__TAURI_INTERNALS__?.invoke;

function UpdateButton() {
  const [invoke] = useState(tauriInvoke);
  const [st, setSt] = useState<UpdateStatus>();
  const refresh = useCallback(() => {
    invoke?.("update_status").then((s) => setSt(s as UpdateStatus), () => setSt(undefined));
  }, [invoke]);
  const busy = !!st?.busy;
  useEffect(() => {
    if (!invoke) return;
    refresh();
    // Quick while a check or install runs; the app's own check runs every six hours.
    const t = setInterval(refresh, busy ? 1000 : 60_000);
    return () => clearInterval(t);
  }, [invoke, refresh, busy]);
  if (!invoke || !st) return null;
  const title = busy ? "Checking for updates" : st.available ? `Update to openpasture ${st.available}` : `openpasture ${st.version}. Check for updates`;
  return (
    <button type="button" className={"upd" + (st.available ? " on" : "")} disabled={busy} title={title} aria-label={title}
      onClick={() => invoke?.("update_check").then(() => setTimeout(refresh, 250), () => {})}>
      <Icon name="navup" size={12} accent="currentColor" />
      <span>{st.available ? "Update" : busy ? "Checking" : st.version}</span>
    </button>
  );
}

// Who is reading, at the foot of the sidebar.
function You() {
  const m = useMe();
  if (!m) return null;
  const role = m.role.slice(0, 1).toUpperCase() + m.role.slice(1);
  const name = m.user?.name ?? role;
  return (
    <a className="whoami" href="#/settings">
      <span className="av" aria-hidden="true">{name.slice(0, 1).toUpperCase()}</span>
      <span className="yn">{name}</span>
      {m.user && <span className="yr">{role}</span>}
    </a>
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
