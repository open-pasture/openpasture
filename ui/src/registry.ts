// Where features plug into the app. A stream registers items from its own
// ui/src/features/<id>/index.ts; the views render whatever is registered, in `order`,
// hiding items below the reader's role (store/me.ts).
//
// Map-only registries live next to the map: overlays and layers in map/overlays.ts,
// tools in map/tools.ts.

import {
  Component,
  createElement,
  Fragment,
  useSyncExternalStore,
  type ComponentType,
  type ErrorInfo,
  type ReactElement,
  type ReactNode,
} from "react";
import type { Animal, Collar, Paddock, Polygon, Role, User } from "./api";
import type { IconName } from "./ui/icons";
import { atLeast, me } from "./store/me";

export type { Role } from "./api";

// Vite sets DEV false in production builds; bun test and the dev server leave it on.
const DEV = import.meta.env.DEV !== false;
// Under the dev server a feature module that is edited runs again and re-registers its items.
const HOT = !!import.meta.hot;

// ---- shortcut keys ------------------------------------------------------------------
// One key space for the whole app. Views, tools and shortcuts claim their key when they
// register; a second claim throws in dev (in production it is logged and the item keeps no key).

const keys = new Map<string, string>([["Escape", "core"]]);

export function claimKey(key: string, owner: string): boolean {
  const had = keys.get(key);
  if (had !== undefined && had !== owner) {
    const msg = `The ${key} key belongs to ${had}; ${owner} can't have it too.`;
    if (DEV) throw new Error(msg);
    console.error(msg);
    return false;
  }
  keys.set(key, owner);
  return true;
}

export const keyOwner = (key: string) => keys.get(key);

// ---- registries -----------------------------------------------------------------------

export interface Item {
  id: string;
  order?: number;
  minRole?: Role;
  key?: string;
}

export interface Registry<T extends Item> {
  readonly name: string;
  // Throws on a duplicate id; claims `key`.
  register(item: T): void;
  // Everything, by order (then registration).
  list(): T[];
  // What `role` may see, by order. Nothing before the role is known.
  visible(role: Role | undefined): T[];
  get(id: string): T | undefined;
  has(id: string): boolean;
  subscribe(fn: () => void): () => void;
  // Tell readers something they compute from items changed (e.g. a layer became available).
  changed(): void;
  // React: visible items, re-rendered on registration, changed() and role changes.
  use(): T[];
}

export function createRegistry<T extends Item>(name: string): Registry<T> {
  const items: T[] = [];
  const subs = new Set<() => void>();
  let version = 0;
  let cache: { version: number; role: Role | undefined; list: T[] } | undefined;
  const bump = () => {
    version++;
    subs.forEach((f) => f());
  };
  const sorted = () => items.slice().sort((a, b) => (a.order ?? 0) - (b.order ?? 0));
  const both = (fn: () => void) => {
    subs.add(fn);
    const off = me.subscribe(fn);
    return () => {
      subs.delete(fn);
      off();
    };
  };
  const snapshot = () => {
    const role = me.get()?.role;
    if (!cache || cache.version !== version || cache.role !== role) cache = { version, role, list: reg.visible(role) };
    return cache.list;
  };
  const reg: Registry<T> = {
    name,
    register(item) {
      const at = items.findIndex((i) => i.id === item.id);
      if (at >= 0 && !HOT) throw new Error(`${name}: "${item.id}" is registered twice.`);
      if (item.key !== undefined && !claimKey(item.key, `${name} ${item.id}`)) item = { ...item, key: undefined };
      if (at >= 0) items[at] = item;
      else items.push(item);
      bump();
    },
    list: sorted,
    visible: (role) => (role === undefined ? [] : sorted().filter((i) => atLeast(role, i.minRole ?? "viewer"))),
    get: (id) => items.find((i) => i.id === id),
    has: (id) => items.some((i) => i.id === id),
    subscribe(fn) {
      subs.add(fn);
      return () => void subs.delete(fn);
    },
    changed: bump,
    use: () => useSyncExternalStore(both, snapshot, snapshot),
  };
  return reg;
}

// A block inside a view, shown when the reader's role allows and `when` says so.
export interface Section<P> {
  id: string;
  order: number;
  minRole?: Role;
  when?: (p: P) => boolean;
  Section: ComponentType<P>;
}

// A view component, usually React.lazy(() => import(...)) so it loads when first shown.
export type Lazy<P> = ComponentType<P>;

// Top nav. `rest` is the hash after the view: "#/herd/214" gives the herd view "214".
export interface ViewItem { id: string; label: string; key: string; order: number; minRole?: Role; icon?: IconName; View: Lazy<{ rest: string }> }
export const views = createRegistry<ViewItem>("views");

// Right-hand herd panel. Built-in blocks sit at HERD_PANEL orders; sections go between them.
export const HERD_PANEL = { decision: 20, escapes: 40, collars: 60, addCollar: 70 } as const;
export const herdPanel = createRegistry<Section<{ herdId: string }>>("herdPanel");

// Entries under the herd name. The name becomes a Menu when this has items or there is more
// than one herd; choosing an entry opens its Item under the name.
export interface HerdMenuItem { id: string; label: string; order: number; minRole?: Role; Item: ComponentType<{ herdId: string }> }
export const herdMenu = createRegistry<HerdMenuItem>("herdMenu");

// The paddock sheet on the map. Built-in parts sit at PADDOCK_SHEET orders.
export const PADDOCK_SHEET = { name: 0, facts: 10, notes: 40, actions: 90 } as const;
export const paddockSheet = createRegistry<Section<{ paddock: Paddock; herdId?: string }>>("paddockSheet");

// The animal page (#/herd/<tag>), rendered by the Herd view.
export const animalPage = createRegistry<Section<{ animal?: Animal; collar?: Collar }>>("animalPage");

// Data view blocks, each under its label. Built-in blocks sit at DATA orders.
export const DATA = { health: 10, replay: 20, pasture: 30, sql: 90 } as const;
export interface DataSectionProps { herdId?: string; from: string; to: string }
export interface DataSection { id: string; label: string; order: number; minRole?: Role; Section: ComponentType<DataSectionProps> }
export const dataSections = createRegistry<DataSection>("dataSections");

// Settings rows. Sections sharing a group render under one label, in order; a group named
// like a built-in row (SETTINGS) joins that row after its own content.
export const SETTINGS = { Brain: 10, Daily: 20, Hosting: 30, Land: 40, Server: 90 } as const;
export interface SettingsSection { id: string; group: string; label: string; order: number; minRole?: Role; Section: ComponentType }
export const settingsSections = createRegistry<SettingsSection>("settingsSections");

// Inside each person's row in Settings > People.
export const peopleRow = createRegistry<Section<{ user: User }>>("peopleRow");

// A row of the Herd table: an animal with its collar, or a collar with no animal.
export interface HerdRow { id: string; animal?: Animal; collar?: Collar }
export interface HerdColumn {
  id: string; label: string; order: number; width: number; minRole?: Role;
  sort?: (a: HerdRow, b: HerdRow) => number;
  Cell: ComponentType<{ row: HerdRow }>;
}
export const herdColumns = createRegistry<HerdColumn>("herdColumns");
// Actions on the Herd table's selection.
export interface HerdBulk { id: string; label: string; order: number; minRole: Role; run(rows: HerdRow[]): Promise<void> }
export const herdBulk = createRegistry<HerdBulk>("herdBulk");

// Below a drawing tool's form, while there is a shape.
export interface ToolFooterProps {
  tool: "boundary" | "strip";
  geometry?: Polygon;
  strips?: Polygon[];
  layoutId?: string;
  herdId: string;
  opts: { warn_m?: number; effective_at?: string };
}
export const toolFooter = createRegistry<Section<ToolFooterProps>>("toolFooter");

// Right end of the top bar, before "offline".
export interface TopbarItem { id: string; order: number; minRole?: Role; Item: ComponentType }
export const topbar = createRegistry<TopbarItem>("topbar");

// #/print/<id>/<rest>: white paper, no app chrome.
export interface PrintPage { id: string; Page: ComponentType<{ rest: string }> }
export const printPages = createRegistry<PrintPage>("printPages");

// What "/" finds. Hits from every finder, in order.
export interface SearchHit { label: string; kind: string; run(): void }
export interface Finder { id: string; order: number; minRole?: Role; find(q: string): Promise<SearchHit[]> }
export const search = createRegistry<Finder>("search");

export async function searchAll(q: string, role: Role | undefined = me.get()?.role): Promise<SearchHit[]> {
  const found = await Promise.all(search.visible(role).map((f) => f.find(q).catch(() => [] as SearchHit[])));
  return found.flat();
}

// App-wide keys that aren't a view or a tool (e.g. "a" opens the alert list). Not while typing.
export interface Shortcut { id: string; key: string; order?: number; minRole?: Role; when?: () => boolean; run(): void }
export const shortcuts = createRegistry<Shortcut>("shortcuts");

// ---- rendering --------------------------------------------------------------------------

// One section failing to render leaves the rest of the view standing.
class Guard extends Component<{ id: string; children?: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() {
    return { failed: true };
  }
  componentDidCatch(e: Error, info: ErrorInfo) {
    console.error(`section ${this.props.id} failed`, e, info.componentStack);
  }
  render() {
    return this.state.failed ? null : this.props.children;
  }
}

export function guarded(id: string, node: ReactNode): ReactElement {
  return createElement(Guard, { key: id, id }, node);
}

// Sections of `reg` for these props: visible to the role and passing `when`.
export function useSections<P>(reg: Registry<Section<P>>, props: P): Section<P>[] {
  return reg.use().filter((s) => !s.when || s.when(props));
}

export interface Placed { key: string; order: number; node: ReactNode }

export function sectionNodes<P extends object>(list: Section<P>[], props: P): Placed[] {
  return list.map((s) => ({ key: `section:${s.id}`, order: s.order, node: guarded(s.id, createElement(s.Section, props)) }));
}

// Built-in blocks and registered ones in one order; built-ins first on a tie. Keys stay
// stable, so a section appearing doesn't remount the blocks after it.
export function interleave(core: Placed[], added: Placed[]): ReactNode {
  const all = [...core, ...added].map((x, i) => ({ ...x, i }));
  all.sort((a, b) => a.order - b.order || a.i - b.i);
  return createElement(Fragment, null, ...all.map((x) => createElement(Fragment, { key: x.key }, x.node)));
}

// Sections alone, in order (null when there are none, so nothing wraps an empty list).
export function Sections<P extends object>({ of, props }: { of: Registry<Section<P>>; props: P }) {
  const list = useSections(of, props);
  return list.length ? interleave([], sectionNodes(list, props)) : null;
}
