// What the map's animal squares are doing, without MapLibre (so it is tested on its own).
// Each frame yields only what changed as a source diff: new squares, squares that moved or
// changed colour, squares that went. 250 animals easing is 250 point updates, not a rebuild.

import type { CollarState, LonLat } from "../api";

export const EASE_MS = 950;
// At most this often the map redraws animals (30 fps).
export const FRAME_MS = 1000 / 30;
// A move draws where each animal walked in the last TRAIL_MS.
export const TRAIL_MS = 75_000;
// Up to this many animals every one has a trail; above it only the ones that matter now.
export const TRAIL_ALL_UP_TO = 50;
// Below this zoom squares jump to a new fix instead of easing.
export const EASE_MIN_ZOOM = 15;
// Below this zoom the herd also shows as an outline.
export const OUTLINE_MAX_ZOOM = 14;
// Square size by zoom (MapLibre expression); full size from z16, where the map opens.
export const ANIMAL_SIZE = ["interpolate", ["linear"], ["zoom"], 12, 0.45, 14, 0.7, 16, 1] as const;

export interface AnimalIn { id: string; point: LonLat; state: CollarState; herd?: string }

type Props = { id: string; state: CollarState; lag: boolean };
export type Feature = { type: "Feature"; properties: Props; geometry: { type: "Point"; coordinates: LonLat } };
export type FeatureUpdate = { id: string; newGeometry?: { type: "Point"; coordinates: LonLat }; addOrUpdateProperties?: { key: string; value: unknown }[] };
export interface Diff { add: Feature[]; remove: string[]; update: FeatureUpdate[] }

interface Anim {
  from: LonLat; to: LonLat; t0: number; state: CollarState; herd?: string;
  trail: { p: LonLat; t: number }[];
  // What the source holds now.
  shown?: { at: LonLat; state: CollarState; lag: boolean };
}

const ease = (t: number) => 1 - Math.pow(1 - t, 2);
const same = (a: LonLat, b: LonLat) => a[0] === b[0] && a[1] === b[1];

export class AnimalsModel {
  private a = new Map<string, Anim>();
  private gone = new Set<string>();
  // Squares that need a look this frame: new, easing, restyled.
  private dirty = new Set<string>();
  private lag = new Set<string>();
  private focus = new Set<string>();
  private trailsOn = false;
  private trailsDirty = false;
  private outlineDirty = true;

  // Who is drawn and where they are. New animals appear at their point; known ones ease there.
  set(items: readonly AnimalIn[], now: number, easing: boolean) {
    const keep = new Set(items.map((i) => i.id));
    for (const id of this.a.keys()) if (!keep.has(id)) this.drop(id);
    for (const it of items) this.move(it.id, it.point, it.state, now, easing, it.herd);
  }

  // A new position. Without easing the square is there at once.
  move(id: string, to: LonLat, state: CollarState, now: number, easing: boolean, herd?: string) {
    const cur = this.a.get(id);
    if (!cur) {
      this.a.set(id, { from: to, to, t0: now - EASE_MS, state, herd, trail: [] });
      this.gone.delete(id);
      this.dirty.add(id);
      this.outlineDirty = true;
      return;
    }
    if (herd !== undefined) cur.herd = herd;
    const moved = !same(cur.to, to);
    if (moved) {
      cur.trail = cur.trail.filter((x) => now - x.t < TRAIL_MS);
      cur.trail.push({ p: cur.to, t: now });
      cur.from = easing ? this.at(cur, now) : to;
      cur.to = to;
      cur.t0 = easing ? now : now - EASE_MS;
      this.outlineDirty = true;
      if (this.trailed(id)) this.trailsDirty = true;
    }
    if (moved || cur.state !== state) {
      cur.state = state;
      this.dirty.add(id);
    }
  }

  private drop(id: string) {
    if (!this.a.delete(id)) return;
    this.dirty.delete(id);
    this.gone.add(id);
    this.outlineDirty = true;
    if (this.trailed(id)) this.trailsDirty = true;
  }

  // Stragglers: drawn soft orange with a ring, whatever their fence state.
  setLag(ids: readonly string[]) {
    const next = new Set(ids);
    for (const id of new Set([...this.lag, ...next])) if (this.lag.has(id) !== next.has(id) && this.a.has(id)) this.dirty.add(id);
    this.lag = next;
    this.trailsDirty = true;
  }

  // Besides stragglers, whose trails show in a big herd: rung by an alert, hovered, selected.
  setFocus(ids: readonly string[]) {
    const next = new Set(ids);
    if (next.size === this.focus.size && ids.every((i) => this.focus.has(i))) return;
    this.focus = next;
    this.trailsDirty = true;
  }

  showTrails(on: boolean) {
    if (on === this.trailsOn) return;
    this.trailsOn = on;
    this.trailsDirty = true;
  }

  where(id: string): LonLat | undefined {
    return this.a.get(id)?.to;
  }

  // Where the square is drawn now (mid-ease or arrived).
  drawnAt(id: string, now: number): LonLat | undefined {
    const a = this.a.get(id);
    return a && this.at(a, now);
  }

  get size() {
    return this.a.size;
  }

  private at(a: Anim, now: number): LonLat {
    const t = ease(Math.min(1, Math.max(0, (now - a.t0) / EASE_MS)));
    return [a.from[0] + (a.to[0] - a.from[0]) * t, a.from[1] + (a.to[1] - a.from[1]) * t];
  }

  private trailed(id: string) {
    return this.trailsOn && (this.a.size <= TRAIL_ALL_UP_TO || this.lag.has(id) || this.focus.has(id));
  }

  // Changes since the last frame, and whether any square is still easing.
  frame(now: number): { diff: Diff | null; moving: boolean } {
    const diff: Diff = { add: [], remove: [...this.gone], update: [] };
    this.gone.clear();
    let moving = false;
    for (const id of this.dirty) {
      const a = this.a.get(id);
      if (!a) continue;
      const at = this.at(a, now);
      const lag = this.lag.has(id);
      if (!a.shown) {
        diff.add.push({ type: "Feature", properties: { id, state: a.state, lag }, geometry: { type: "Point", coordinates: at } });
      } else {
        const u: FeatureUpdate = { id };
        if (!same(a.shown.at, at)) u.newGeometry = { type: "Point", coordinates: at };
        const props: { key: string; value: unknown }[] = [];
        if (a.shown.state !== a.state) props.push({ key: "state", value: a.state });
        if (a.shown.lag !== lag) props.push({ key: "lag", value: lag });
        if (props.length) u.addOrUpdateProperties = props;
        if (u.newGeometry || u.addOrUpdateProperties) diff.update.push(u);
      }
      a.shown = { at, state: a.state, lag };
      if (now - a.t0 < EASE_MS) moving = true;
    }
    // Keep only the ones still easing for next frame.
    for (const id of [...this.dirty]) {
      const a = this.a.get(id);
      if (!a || now - a.t0 >= EASE_MS) this.dirty.delete(id);
    }
    const empty = !diff.add.length && !diff.remove.length && !diff.update.length;
    return { diff: empty ? null : diff, moving };
  }

  // Trails to draw, when they changed since last asked (else null). Rebuilt only on a new
  // fix or a change of who has one; the newest point is the latest fix.
  trails(now: number): { lag: boolean; path: LonLat[] }[] | null {
    if (!this.trailsDirty) return null;
    this.trailsDirty = false;
    if (!this.trailsOn) return [];
    const out: { lag: boolean; path: LonLat[] }[] = [];
    for (const [id, a] of this.a) {
      if (!this.trailed(id)) continue;
      // Averaged over three fixes, so GPS jitter and grazing steps don't scribble.
      const raw = [...a.trail.filter((x) => now - x.t < TRAIL_MS).map((x) => x.p), a.to];
      if (raw.length < 2) continue;
      const path = raw.map((p, i) => {
        if (i === raw.length - 1) return p;
        const w = raw.slice(Math.max(0, i - 1), i + 2);
        return [w.reduce((s, q) => s + q[0], 0) / w.length, w.reduce((s, q) => s + q[1], 0) / w.length] as LonLat;
      });
      out.push({ lag: this.lag.has(id), path });
    }
    return out;
  }

  // Each herd's outline (convex hull of its animals), when positions changed since last asked.
  outlines(): { herd: string; ring: LonLat[] }[] | null {
    if (!this.outlineDirty) return null;
    this.outlineDirty = false;
    const by = new Map<string, LonLat[]>();
    for (const a of this.a.values()) {
      const k = a.herd ?? "";
      let pts = by.get(k);
      if (!pts) by.set(k, (pts = []));
      pts.push(a.to);
    }
    const out: { herd: string; ring: LonLat[] }[] = [];
    for (const [herd, pts] of by) {
      const ring = hull(pts);
      if (ring.length >= 3) out.push({ herd, ring: [...ring, ring[0]] });
    }
    return out;
  }
}

// Convex hull, counter-clockwise, no repeated first point (Andrew's monotone chain).
export function hull(points: readonly LonLat[]): LonLat[] {
  const p = [...points].sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  if (p.length < 3) return p;
  const cross = (o: LonLat, a: LonLat, b: LonLat) => (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
  const lower: LonLat[] = [];
  for (const q of p) {
    while (lower.length >= 2 && cross(lower[lower.length - 2], lower[lower.length - 1], q) <= 0) lower.pop();
    lower.push(q);
  }
  const upper: LonLat[] = [];
  for (let i = p.length - 1; i >= 0; i--) {
    const q = p[i];
    while (upper.length >= 2 && cross(upper[upper.length - 2], upper[upper.length - 1], q) <= 0) upper.pop();
    upper.push(q);
  }
  return [...lower.slice(0, -1), ...upper.slice(0, -1)];
}

// Whether a frame is due: at most one per FRAME_MS.
export function frameDue(last: number, now: number) {
  return now - last >= FRAME_MS - 1;
}
