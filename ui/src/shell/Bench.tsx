// The desktop app: a rail of views down the left, the view's sidebar beside it (the farm's
// paddocks unless the view brings its own), the open views as tabs over the one showing, the herd
// panel on the right and a status line along the foot. Phones keep the one-column shell in App.

import { Suspense, useEffect, useRef, useState, type MouseEvent } from "react";
import { guarded, shortcuts, sidebars, topbar, type ViewItem } from "../registry";
import { store, useStore } from "../store";
import { ackLine } from "../store/boundary";
import { summarize } from "../store/live";
import { useMe } from "../store/me";
import { createSlice } from "../store/slice";
import { pageHash, pageKeys } from "../features/k-animals/herd";
import { isPhone } from "../features/m/phone";
import { age, useNow } from "../util";
import { Icon, Mark } from "../ui";
import { HerdPanel } from "../views/HerdPanel";
import { ask, mapState } from "./bus";
import { alerts, loadAlerts } from "../store/a-engine";
import { asksOf } from "./asks";
import { asksPing, Desk } from "./Desk";
import { FarmHead, FarmSidebar, HerdSwitch } from "./Farm";
import { close, hashOf, prune, tabs, visit, type Tab } from "./tabs";
import { UpdateButton } from "./UpdateButton";
import "../styles/bench.css";

// Either side shown or put away, kept across reloads: [ the sidebar, ] the herd panel.
function shown(key: string) {
  const s = createSlice(localStorage.getItem(key) !== "off");
  s.subscribe(() => localStorage.setItem(key, s.get() ? "on" : "off"));
  return s;
}
const leftOpen = shown("op.nav");
const side = shown("op.side");
shortcuts.register({ id: "bench-nav", key: "[", when: () => !isPhone(), run: () => leftOpen.set(!leftOpen.get()) });
shortcuts.register({ id: "bench-side", key: "]", when: () => !isPhone(), run: () => side.set(!side.get()) });

export function Bench({ nav, cur, view, rest }: { nav: ViewItem[]; cur: ViewItem | undefined; view: string; rest: string }) {
  const shownRest = cur?.id === view ? rest : "";
  const navKey = nav.map((v) => v.id).join();
  useEffect(() => {
    if (cur) visit(cur.id, shownRest);
  }, [cur?.id, shownRest]); // eslint-disable-line react-hooks/exhaustive-deps
  useEffect(() => {
    if (nav.length) prune((id) => nav.some((v) => v.id === id));
  }, [navKey]); // eslint-disable-line react-hooks/exhaustive-deps
  // Once the app has drawn, every view's and sidebar's code comes down while nothing else is
  // asked of it: a tab then opens at once, not behind slow requests the open view made.
  useEffect(() => {
    const idle = (f: () => void) => ("requestIdleCallback" in window ? requestIdleCallback(f, { timeout: 4000 }) : setTimeout(f, 1500));
    idle(() => [...nav, ...sidebars.list()].forEach((x) => void x.preload?.().catch(() => {})));
  }, [navKey]); // eslint-disable-line react-hooks/exhaustive-deps
  const open = side.use((v) => v);
  const navOpen = leftOpen.use((v) => v);
  const panel = !!cur && cur.side !== false;

  return (
    <div className="bench">
      <Rail nav={nav} cur={cur} />
      {navOpen && cur && <Sidebar view={cur} rest={shownRest} />}
      <div className="bmain">
        <Tabs nav={nav} cur={cur} panel={panel} />
        <main>
          <Suspense fallback={null}>
            {cur && <div key={cur.id} className="bview">{guarded(cur.id, <cur.View rest={shownRest} />)}</div>}
          </Suspense>
        </main>
      </div>
      {panel && open && <Side />}
      <Status />
    </div>
  );
}

// ---- sidebar ----------------------------------------------------------------------------

// The view's own sidebar, else the farm's, under the herd it is about (or the farm's name, over a
// view that isn't about one herd).
function Sidebar({ view, rest }: { view: ViewItem; rest: string }) {
  const own = sidebars.use().find((s) => s.id === view.id);
  return (
    <aside className="bside" aria-label={`${view.label} sidebar`}>
      {view.side === false ? <FarmHead /> : <HerdSwitch />}
      <div key={view.id} className="bsbody">
        <Suspense fallback={null}>
          {own ? guarded(`sidebar:${view.id}`, <own.Sidebar rest={rest} />) : guarded("sidebar:farm", <FarmSidebar />)}
        </Suspense>
      </div>
    </aside>
  );
}

// ---- rail -------------------------------------------------------------------------------

function Rail({ nav, cur }: { nav: ViewItem[]; cur: ViewItem | undefined }) {
  const list = tabs.use((v) => v);
  const items = topbar.use().filter((t) => !t.at);
  // A view goes back to where its tab was; the view showing goes back to its start.
  const href = (v: ViewItem) => {
    const t = list.find((t) => t.view === v.id);
    return t && v.id !== cur?.id ? hashOf(t) : `#/${v.id}`;
  };
  const at = nav.findIndex((v) => v.id === cur?.id);
  return (
    <nav className="rail" aria-label="Views">
      <a className="rmark" href="#/map" title="openpasture"><Mark size={16} /></a>
      {/* The marker glides to the view showing. */}
      {at >= 0 && <i className="rind" style={{ transform: `translateY(${at * 40}px)` }} aria-hidden="true" />}
      {nav.map((v) => (
        <a key={v.id} href={href(v)} className="ritem" aria-current={cur?.id === v.id ? "page" : undefined}
          title={`${v.label} (${v.key.toUpperCase()})`} aria-label={v.label}>
          {v.icon ? <Icon name={v.icon} size={16} accent="currentColor" /> : <span className="rletter">{v.label.slice(0, 1)}</span>}
        </a>
      ))}
      <div className="rtools">
        {items.map((t) => guarded(t.id, <t.Item />))}
        <AsksButton />
        <You />
      </div>
    </nav>
  );
}

// What openpasture needs from the farmer: how many asks, and a click brings them into view in the
// herd panel (opening it if it was put away). A new ask makes it beat once.
function AsksButton() {
  const decisions = useStore((s) => s.decisions);
  const herdId = useStore((s) => s.herdId);
  const list = alerts.use((s) => s.list);
  const up = useStore((s) => s.up);
  // Alerts load when the app shows and again whenever the live feed comes back (events were
  // missed), as the phone's alert list does.
  useEffect(() => {
    void loadAlerts();
  }, [up]);
  const asks = asksOf(decisions, list, herdId);
  const high = asks.some((a) => a.rank >= 4);
  const [beat, setBeat] = useState(0);
  const seen = useRef(asks.length);
  useEffect(() => {
    if (asks.length > seen.current) setBeat((b) => b + 1);
    seen.current = asks.length;
  }, [asks.length]);
  if (!asks.length) return null;
  const title = `${asks.length} ${asks.length === 1 ? "thing needs" : "things need"} you`;
  return (
    <button type="button" key={beat} className={"rasks" + (beat ? " beat" : "")} data-high={high || undefined} title={title} aria-label={title}
      onClick={() => {
        side.set(true);
        asksPing.set((n) => n + 1);
      }}>
      <i aria-hidden="true" /><span className="mono">{asks.length}</span>
    </button>
  );
}

// Who is reading: their initial, opening Settings.
function You() {
  const m = useMe();
  if (!m) return null;
  const role = m.role.slice(0, 1).toUpperCase() + m.role.slice(1);
  const name = m.user?.name ?? role;
  return (
    <a className="ryou" href="#/settings" title={m.user ? `${name}, ${role.toLowerCase()}` : name} aria-label={name}>
      {name.slice(0, 1).toUpperCase()}
    </a>
  );
}

// ---- tabs -------------------------------------------------------------------------------

function Tabs({ nav, cur, panel }: { nav: ViewItem[]; cur: ViewItem | undefined; panel: boolean }) {
  const list = tabs.use((v) => v);
  const open = side.use((v) => v);
  const left = leftOpen.use((v) => v);
  const label = useTabLabel(nav);
  const shut = (e: MouseEvent, t: Tab) => {
    e.preventDefault();
    e.stopPropagation();
    close(t.view, cur?.id ?? "");
  };
  return (
    <div className="btabs">
      <button type="button" className="tnav" aria-pressed={left} title={left ? "Hide the sidebar ([)" : "Show the sidebar ([)"}
        aria-label="Sidebar" onClick={() => leftOpen.set(!left)}>
        <Icon name="navside" size={14} accent="currentColor" style={{ transform: "scaleX(-1)" }} />
      </button>
      <div className="tlist" role="tablist" aria-label="Open views">
        {list.map((t) => {
          const v = nav.find((v) => v.id === t.view);
          if (!v) return null;
          const on = cur?.id === t.view;
          return (
            <a key={t.view} href={hashOf(t)} role="tab" aria-selected={on} className="tab"
              onAuxClick={(e) => e.button === 1 && shut(e, t)}>
              {v.icon && <Icon name={v.icon} size={11} accent="currentColor" />}
              <span className="tl">{label(t, v)}</span>
              {list.length > 1 && (
                <button type="button" className="tx" aria-label={`Close ${v.label}`} title="Close" onClick={(e) => shut(e, t)}>
                  <Icon name="none" size={7} accent="currentColor" />
                </button>
              )}
            </a>
          );
        })}
      </div>
      {panel && (
        <button type="button" className="tside" aria-pressed={open} title={open ? "Hide the herd panel (])" : "Show the herd panel (])"}
          aria-label="Herd panel" onClick={() => side.set(!open)}>
          <Icon name="navside" size={14} accent="currentColor" />
        </button>
      )}
    </div>
  );
}

// A tab is its view's name, or what it shows inside the view: an animal's tag or name.
function useTabLabel(nav: ViewItem[]) {
  const animals = useStore((s) => s.animals);
  const collars = useStore((s) => s.collars);
  return (t: Tab, v: ViewItem) => {
    if (t.view !== "herd" || !t.rest || t.rest.startsWith("?")) return v.label;
    const key = decodeURIComponent(t.rest.split("/")[0]);
    const a = animals.find((a) => a.tag === key || a.id === key);
    const c = a ? undefined : collars.find((c) => c.id === key);
    return a ? (a.name ? `${a.tag} ${a.name}` : a.tag) : c?.name ?? nav.find((n) => n.id === "herd")?.label ?? v.label;
  };
}

// ---- herd panel -------------------------------------------------------------------------

// The herd panel beside the view. On the map it points the map; elsewhere an animal opens its
// page and a group opens the Herd table with them selected.
function Side() {
  const changing = mapState.use((v) => v.changing);
  const herdId = useStore((s) => s.herdId);
  const change = () => {
    if (!mapState.get().on) location.hash = "#/map";
    ask({ k: "change" });
  };
  return (
    <div className="bright">
      <HerdPanel
        desk={herdId && <Desk herdId={herdId} changing={changing} onChange={change} onFocusCollar={focusOne} onFocusCollars={focus} />}
        changing={changing}
        onChange={change}
        onFocusCollar={focusOne}
        onFocusCollars={focus}
        onHoverCollar={(id) => mapState.get().on && ask({ k: "hover", id })}
      />
    </div>
  );
}

// A group of collars: on the map, the map goes to them; elsewhere the Herd table opens with them
// selected (one opens its animal's page).
function focus(ids: string[]) {
  if (mapState.get().on) return ask({ k: "collars", ids });
  if (ids.length === 1) return focusOne(ids[0]);
  location.hash = `#/herd?select=${ids.map(encodeURIComponent).join(",")}`;
}

function focusOne(id: string) {
  if (mapState.get().on) return ask({ k: "collar", id });
  const { animals, collars } = store.get();
  const c = collars.find((x) => x.id === id);
  const a = animals.find((x) => x.collar_id === id || (c?.animal_id && x.id === c.animal_id));
  location.hash = pageHash(pageKeys(animals)(a, c));
}

// ---- status -----------------------------------------------------------------------------

function Status() {
  const up = useStore((s) => s.up);
  const farm = useStore((s) => s.state?.farm);
  const herdId = useStore((s) => s.herdId);
  const collars = useStore((s) => s.collars);
  const bstat = useStore((s) => (s.herdId ? s.boundary[s.herdId] : undefined));
  const items = topbar.use().filter((t) => t.at === "status");
  const now = useNow(15_000);
  const mine = collars.filter((c) => c.herd_id === herdId);
  const newest = mine.reduce<string | undefined>((m, c) => (c.last_seen && (!m || c.last_seen > m) ? c.last_seen : m), undefined);
  const line = ackLine(bstat, new Set(mine.map((c) => c.id)));
  const sum = summarize(mine.filter((c) => !c.parked_at));
  // The dot blinks as reports land, at most about once a second.
  const [beat, setBeat] = useState(0);
  const last = useRef(0);
  useEffect(() => store.onPositions(() => {
    const t = Date.now();
    if (t - last.current < 900) return;
    last.current = t;
    setBeat((b) => b + 1);
  }), []);
  const time = farm?.timezone
    ? new Date(now).toLocaleTimeString(undefined, { timeZone: farm.timezone, hour: "2-digit", minute: "2-digit", timeZoneName: "short" })
    : undefined;
  return (
    <footer className="bstatus mono">
      <span className={"bconn" + (up ? " up" : "")}><i key={beat} className={beat ? "beat" : undefined} />{up ? "connected" : "offline"}</span>
      {items.map((t) => guarded(t.id, <t.Item />))}
      {mine.length > 0 && <span>{mine.length} collars{newest && `, last report ${age(newest, now)} ago`}</span>}
      {sum.outside.length > 0 && <button type="button" className="bwarn" onClick={() => focus(sum.outside)}>{sum.outside.length} out</button>}
      {sum.warning.length > 0 && <button type="button" className="bwarn" onClick={() => focus(sum.warning)}>{sum.warning.length} near</button>}
      {sum.low.length > 0 && <button type="button" className="bwarn" onClick={() => focus(sum.low)}>{sum.low.length} low battery</button>}
      {line && <span title={`${line.n} of ${line.of} collars ${line.held ? "hold" : "applied"} boundary v${line.version}`}>
        boundary v{line.version} <b className={line.n >= line.of ? "ok" : undefined}>{line.n}/{line.of}</b>
      </span>}
      <span className="bgap" />
      {time && <span title={farm?.timezone}>{time}</span>}
      <UpdateButton />
    </footer>
  );
}
