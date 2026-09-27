// What streams draw on the map. An overlay mounts once the map is up and draws into
// its slot (map/layers.ts); a layer is an overlay the Layers menu turns on and off.

import type { ExpressionSpecification, Map as MLMap } from "maplibre-gl";
import type { ReactNode } from "react";
import type { LonLat, PositionItem } from "../api";
import { createRegistry } from "../registry";
import { store } from "../store";
import type { Slot } from "./layers";
import { ANIMAL_SIZE } from "./animals-model";

export type { Slot } from "./layers";
export type Tone = "fg" | "warn" | "red" | "grass";
export type BBox = [number, number, number, number]; // west, south, east, north

export interface OverlayCtx {
  map: MLMap;
  herdId(): string | undefined;
  // Latest position per collar id, kept current by the live feed.
  positions(): ReadonlyMap<string, PositionItem>;
  // Called at most once a frame with the collar ids whose position changed. Returns unsubscribe.
  onPositions(fn: (changed: string[]) => void): () => void;
  // Ring these animals in a tone; each overlay has its own set, [] clears it.
  highlight(ids: string[], tone?: Tone): void;
  flyTo(to: LonLat | BBox): void;
  // Open the map's side sheet with this content; null closes it.
  openSheet(node: ReactNode | null): void;
  // Id to pass as addLayer's beforeId to draw in a slot.
  beforeId(slot: Slot): string;
}

export interface OverlayHandle {
  // Called when the herd or the farm record changes.
  update?(): void;
  destroy(): void;
}

export interface Overlay {
  id: string;
  slot: Slot;
  mount(ctx: OverlayCtx): OverlayHandle;
}

export const overlays = createRegistry<Overlay>("overlays");

// Shown in the Layers menu only while available(); call layers.changed() when that may have changed.
export interface LayerItem { id: string; label: string; order: number; available(): boolean; overlay: Overlay }
export const layers = createRegistry<LayerItem>("layers");

// ---- positions ------------------------------------------------------------------------

type PosListener = (changed: string[]) => void;

// Built from the collar list and moved by each positions batch, so overlays never scan the store.
class Positions {
  private map = new Map<string, PositionItem>();
  private listeners = new Set<PosListener>();
  private pending = new Set<string>();
  private queued = false;
  private collars = store.get().collars;
  private started = false;

  private start() {
    if (this.started) return;
    this.started = true;
    this.rebuild();
    store.subscribe(() => {
      if (store.get().collars !== this.collars) this.rebuild();
    });
    store.onPositions((items) => {
      for (const it of items) {
        const cur = this.map.get(it.collar_id);
        if (cur && Date.parse(cur.fix.at) > Date.parse(it.fix.at)) continue;
        this.map.set(it.collar_id, { ...cur, ...it, animal_id: it.animal_id ?? cur?.animal_id, last_seen: it.last_seen ?? it.fix.at });
        this.touch(it.collar_id);
      }
    });
  }

  private rebuild() {
    this.collars = store.get().collars;
    const next = new Map<string, PositionItem>();
    for (const c of this.collars) {
      if (!c.last_fix) continue;
      next.set(c.id, { collar_id: c.id, animal_id: c.animal_id, fix: c.last_fix, state: c.state, battery: c.battery, last_seen: c.last_seen });
    }
    for (const id of new Set([...this.map.keys(), ...next.keys()])) {
      const a = this.map.get(id), b = next.get(id);
      if (!a || !b || a.fix.at !== b.fix.at || a.state !== b.state || a.battery !== b.battery) this.touch(id);
    }
    this.map = next;
  }

  private touch(id: string) {
    this.pending.add(id);
    if (this.queued) return;
    this.queued = true;
    requestAnimationFrame(() => {
      this.queued = false;
      const ids = [...this.pending];
      this.pending.clear();
      this.listeners.forEach((f) => f(ids));
    });
  }

  all(): ReadonlyMap<string, PositionItem> {
    this.start();
    return this.map;
  }

  listen(fn: PosListener) {
    this.start();
    this.listeners.add(fn);
    return () => void this.listeners.delete(fn);
  }
}

export const positions = new Positions();

// ---- highlight rings ------------------------------------------------------------------

const PR = 2;
// C.fg, C.warn, C.red, C.grass (map/base.ts). Not imported: base.ts loads MapLibre, and
// registries stay out of the first bundle.
const TONES: Record<Tone, string> = { fg: "#F3F2EA", warn: "#F0936C", red: "#E5484D", grass: "#9FD760" };

// A 13 px square ring, like the hover ring on an animal.
function ringImage(hex: string) {
  const n = 13 * PR;
  const data = new Uint8Array(n * n * 4);
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));
  for (let y = 0; y < n; y++)
    for (let x = 0; x < n; x++)
      if (x < PR || y < PR || x >= n - PR || y >= n - PR) data.set([r, g, b, 255], (y * n + x) * 4);
  return { width: n, height: n, data };
}

// Rings on the animals source, one layer per tone in slot-top; each overlay owns a set.
export class Rings {
  private sets = new Map<string, { ids: string[]; tone: Tone }>();
  // Told whenever the rung animals change (the map draws their trails).
  onChange?: () => void;
  constructor(private map: MLMap) {}

  // Every animal some overlay rings now (alerts, tools): the map draws their trails.
  ids(): Set<string> {
    return new Set([...this.sets.values()].flatMap((s) => s.ids));
  }

  set(owner: string, ids: string[], tone: Tone = "fg") {
    if (ids.length) this.sets.set(owner, { ids, tone });
    else this.sets.delete(owner);
    for (const t of Object.keys(TONES) as Tone[]) this.draw(t);
    this.onChange?.();
  }

  private draw(tone: Tone) {
    const ids = [...new Set([...this.sets.values()].filter((s) => s.tone === tone).flatMap((s) => s.ids))];
    const id = `rings-${tone}`;
    if (!this.map.getLayer(id)) {
      if (!ids.length || !this.map.getSource("animals")) return;
      if (!this.map.hasImage(id)) this.map.addImage(id, ringImage(TONES[tone]), { pixelRatio: PR });
      this.map.addLayer(
        { id, type: "symbol", source: "animals", layout: { "icon-image": id, "icon-size": ANIMAL_SIZE as unknown as ExpressionSpecification, "icon-allow-overlap": true, "icon-ignore-placement": true } },
        this.map.getLayer("slot-top") ? "slot-top" : undefined,
      );
    }
    this.map.setFilter(id, ["in", ["get", "id"], ["literal", ids]]);
  }
}

// ---- mounting ---------------------------------------------------------------------------

export interface MapHost {
  map: MLMap;
  rings: Rings;
  herdId(): string | undefined;
  openSheet(node: ReactNode | null): void;
}

export function overlayCtx(host: MapHost, owner: string): OverlayCtx {
  const { map } = host;
  return {
    map,
    herdId: host.herdId,
    positions: () => positions.all(),
    onPositions: (fn) => positions.listen(fn),
    highlight: (ids, tone) => host.rings.set(owner, ids, tone),
    flyTo(to) {
      if (to.length === 2) map.easeTo({ center: to, duration: 700 });
      else map.fitBounds([[to[0], to[1]], [to[2], to[3]]], { padding: 120, maxZoom: 18, duration: 700 });
    },
    openSheet: host.openSheet,
    beforeId: (slot) => slot,
  };
}

// Mount an overlay, keeping one broken overlay from taking the map down.
export function mountOverlay(host: MapHost, o: Overlay): OverlayHandle {
  try {
    const h = o.mount(overlayCtx(host, o.id));
    return {
      update: () => {
        try {
          h.update?.();
        } catch (e) {
          console.error(`overlay ${o.id} update failed`, e);
        }
      },
      destroy: () => {
        host.rings.set(o.id, []);
        try {
          h.destroy();
        } catch (e) {
          console.error(`overlay ${o.id} destroy failed`, e);
        }
      },
    };
  } catch (e) {
    console.error(`overlay ${o.id} failed to mount`, e);
    return { destroy: () => host.rings.set(o.id, []) };
  }
}
