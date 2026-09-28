import type { Map as MLMap } from "maplibre-gl";
import type { LonLat, Polygon } from "../api";
import { C, fc, setData } from "./base";
import { setBoundary } from "./layers";
import { type Ring, type XY, chevrons, dot, ll, rearRun, signedArea, xy } from "./sweep-model";

// The active boundary and, while a move sweeps, what explains it: the back line
// lit, chevrons drifting toward the target, and the ground behind the back line
// dimmed. A new step glides from the old shape to the new one over GLIDE ms:
// both outer rings are resampled to N points, aligned, and interpolated; holes go
// straight to their new place. A shape that gains or loses a hole switches at once.

const N = 160;
const GLIDE = 1500;
// Once a step has glided in, only the chevrons move: they drift slowly, so a
// frame this often is enough, and the boundary, back line and swept ground
// are drawn once, not every frame.
const CHEVRON_FRAME_MS = 66;
const smooth = (t: number) => t * t * (3 - 2 * t);


export interface SweepState {
  active?: Polygon;
  // Set while a move sweeps.
  target?: Polygon;
  // Unit east/north axis of travel; undefined when the herd closes in from all sides.
  axis?: XY;
  // Ground the herd held when the move began; what falls behind the back line dims.
  ground?: Polygon[];
}



// N points evenly spaced along the ring, counter-clockwise, open (no closing point).
function resample(g: Polygon): Ring {
  let r = g.coordinates[0];
  if (signedArea(r) < 0) r = r.slice().reverse();
  const k = Math.cos((r[0][1] * Math.PI) / 180);
  const seg: number[] = [];
  let total = 0;
  for (let i = 0; i < r.length - 1; i++) {
    const d = Math.hypot((r[i + 1][0] - r[i][0]) * k, r[i + 1][1] - r[i][1]);
    seg.push(d);
    total += d;
  }
  const out: Ring = [];
  let i = 0, acc = 0;
  for (let j = 0; j < N; j++) {
    const want = (j / N) * total;
    while (i < seg.length - 1 && acc + seg[i] < want) acc += seg[i++];
    const t = seg[i] ? (want - acc) / seg[i] : 0;
    out.push([r[i][0] + (r[i + 1][0] - r[i][0]) * t, r[i][1] + (r[i + 1][1] - r[i][1]) * t]);
  }
  // Pin each corner to its nearest sample, so corners stay sharp mid-glide.
  let at = 0;
  for (let v = 0; v < seg.length; v++) {
    out[Math.round((at / total) * N) % N] = r[v];
    at += seg[v];
  }
  return out;
}

// Rotate b so its points line up with a's (least total squared distance).
function align(a: Ring, b: Ring): Ring {
  let best = 0, bestD = Infinity;
  for (let s = 0; s < N; s++) {
    let d = 0;
    for (let i = 0; i < N && d < bestD; i++) {
      const p = b[(i + s) % N], q = a[i];
      d += (p[0] - q[0]) ** 2 + (p[1] - q[1]) ** 2;
    }
    if (d < bestD) (bestD = d), (best = s);
  }
  return b.map((_, i) => b[(i + best) % N]);
}

// The outer ring on screen with the destination's holes.
const closed = (r: Ring, exact?: Polygon): Polygon => ({ type: "Polygon", coordinates: [[...r, r[0]], ...(exact?.coordinates.slice(1) ?? [])] });
const ringCount = (g?: Polygon) => g?.coordinates.length ?? 0;

function centroidOf(r: Ring): LonLat {
  const s = r.reduce((a, p) => [a[0] + p[0], a[1] + p[1]], [0, 0]);
  return [s[0] / r.length, s[1] / r.length];
}

// The step's back edge faces straight away from the target, so the edge whose outward
// normal best opposes the rough axis (weighted by length) gives the exact one.
export function snapAxis(active: Polygon, rough: XY): XY {
  let r = active.coordinates[0];
  if (signedArea(r) < 0) r = r.slice().reverse();
  const lat0 = r[0][1];
  let best = rough, bestScore = 0;
  for (let i = 0; i < r.length - 1; i++) {
    const a = xy(r[i], lat0), b = xy(r[i + 1], lat0);
    const d: XY = [b[0] - a[0], b[1] - a[1]];
    const len = Math.hypot(d[0], d[1]);
    if (len < 1) continue;
    const out: XY = [d[1] / len, -d[0] / len]; // outward for a ccw ring
    const c = -dot(out, rough);
    if (c > 0.8 && c * len > bestScore) (bestScore = c * len), (best = [-out[0], -out[1]]);
  }
  return best;
}

// Keep the part of a ring at or below `level` along `axis` (one Sutherland-Hodgman pass).
function behind(r: XY[], axis: XY, level: number): XY[] {
  const out: XY[] = [];
  for (let i = 0; i < r.length; i++) {
    const a = r[i], b = r[(i + 1) % r.length];
    const da = level - dot(a, axis), db = level - dot(b, axis);
    if (da >= 0) out.push(a);
    if (da >= 0 !== db >= 0) {
      const t = da / (da - db);
      out.push([a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
    }
  }
  return out;
}

// A pixel chevron pointing north, in grass.
function chevron() {
  const PR = 2, w = 13, h = 7;
  const rows = [
    "......#......",
    ".....###.....",
    "....##.##....",
    "...##...##...",
    "..##.....##..",
    ".##.......##.",
    "##.........##",
  ];
  const data = new Uint8Array(w * PR * h * PR * 4);
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(C.grass2.slice(i, i + 2), 16));
  for (let y = 0; y < h * PR; y++)
    for (let x = 0; x < w * PR; x++)
      if (rows[Math.floor(y / PR)][Math.floor(x / PR)] === "#") data.set([r, g, b, 255], (y * w * PR + x) * 4);
  return { image: { width: w * PR, height: h * PR, data }, pixelRatio: PR };
}

export class SweepView {
  private shown?: Ring; // what is on screen, resampled
  private from?: Ring;
  private to?: Ring;
  private exact?: Polygon;
  private t0 = 0;
  private raf = 0;
  private tick?: ReturnType<typeof setTimeout>;
  // The boundary, back line and swept ground as last drawn are current.
  private drawn = false;
  // Where the chevrons run: from the back line's middle toward the target.
  private run?: { from: XY; d: XY; lat0: number };
  private s: SweepState = {};

  constructor(private map: MLMap) {
    const c = chevron();
    map.addImage("chevron", c.image, { pixelRatio: c.pixelRatio });
  }

  set(next: SweepState, glide: boolean) {
    const g = next.active;
    const changed = JSON.stringify(g?.coordinates) !== JSON.stringify(this.exact?.coordinates);
    this.s = next;
    this.drawn = false;
    setBoundary(this.map, "target", next.target);
    if (changed) {
      const sameRings = ringCount(g) === ringCount(this.exact);
      this.exact = g;
      if (!g) this.shown = undefined;
      else {
        const to = resample(g);
        if (glide && this.shown && sameRings) {
          this.from = this.shown;
          this.to = align(this.from, to);
          this.t0 = performance.now();
        } else {
          this.from = this.to = this.shown = to;
          this.t0 = 0;
        }
      }
    }
    this.kick();
  }

  private kick() {
    clearTimeout(this.tick);
    this.tick = undefined;
    if (!this.raf) this.raf = requestAnimationFrame(this.frame);
  }

  private frame = () => {
    this.raf = 0;
    const now = performance.now();
    const t = this.t0 ? Math.min(1, (now - this.t0) / GLIDE) : 1;
    const gliding = t < 1;
    if (gliding || !this.drawn) {
      if (this.from && this.to) {
        const e = smooth(t), a = this.from, b = this.to;
        this.shown = gliding ? a.map((p, i) => [p[0] + (b[i][0] - p[0]) * e, p[1] + (b[i][1] - p[1]) * e] as LonLat) : b;
      }
      // The last frame draws the exact polygon so corners stay sharp.
      setBoundary(this.map, "active", this.shown ? (gliding ? closed(this.shown, this.exact) : this.exact) : undefined);
      this.drawCues();
      this.drawn = !gliding;
    }
    const live = this.drawChevrons(now);
    if (gliding) this.raf = requestAnimationFrame(this.frame);
    else if (live) this.tick = setTimeout(() => this.kick(), CHEVRON_FRAME_MS);
  };

  // Back line and swept ground from what is on screen, and where the chevrons run.
  private drawCues() {
    const { target, axis, ground } = this.s;
    const ring = this.shown;
    this.run = undefined;
    if (!target || !axis || !ring) {
      this.fade(false);
      return;
    }
    this.fade(true);
    const lat0 = ring[0][1];
    const pts = ring.map((p) => xy(p, lat0));
    const prog = pts.map((p) => dot(p, axis));
    const level = Math.min(...prog);

    // The back line: the run of the ring that faces back. A sweep's back edge
    // follows the animals at the back of the herd, so it zig-zags behind them.
    let line: LonLat[] = rearRun(pts, axis).map((i) => ring[i]);
    // A single point is a corner, not an edge: light a short span across the axis instead.
    if (line.length < 2) {
      const m = pts[prog.indexOf(level)];
      const n: XY = [-axis[1], axis[0]];
      line = [ll([m[0] - n[0] * 6, m[1] - n[1] * 6], lat0), ll([m[0] + n[0] * 6, m[1] + n[1] * 6], lat0)];
    }
    setData(this.map, "back", fc([{ type: "Feature", properties: {}, geometry: { type: "LineString", coordinates: line } }]));

    // Chevrons drift from the back line's middle toward the target (see drawChevrons).
    const a0 = xy(line[0], lat0), a1 = xy(line[line.length - 1], lat0);
    const lineMid: XY = [(a0[0] + a1[0]) / 2, (a0[1] + a1[1]) / 2];
    const goal = xy(centroidOf(target.coordinates[0].slice(0, -1)), lat0);
    this.run = { from: lineMid, d: [goal[0] - lineMid[0], goal[1] - lineMid[1]], lat0 };

    // The ground given up: what the herd held, behind the back line.
    // Only as wide as the fence is now, so it reads as the path the sweep took.
    const n: XY = [-axis[1], axis[0]];
    const side = pts.map((p) => dot(p, n));
    const [t0, t1] = [Math.min(...side), Math.max(...side)];
    const swept: GeoJSON.Feature[] = [];
    for (const g of ground ?? []) {
      const r = g.coordinates[0].slice(0, -1).map((p) => xy(p, lat0));
      const part = behind(behind(behind(r, axis, level), n, t1), [-n[0], -n[1]], -t0);
      if (part.length >= 3) {
        const c = part.map((q) => ll(q, lat0));
        swept.push({ type: "Feature", properties: {}, geometry: { type: "Polygon", coordinates: [[...c, c[0]]] } });
      }
    }
    setData(this.map, "swept", fc(swept));
  }

  // Chevrons along the run, fading in and out. Returns whether they need another frame.
  private drawChevrons(now: number) {
    const feats = chevrons(this.run, now);
    setData(this.map, "chevrons", fc(feats));
    return feats.length > 0;
  }

  // The swept ground and the back line fade in when a sweep starts and out when it ends.
  private cues = false;
  private clearTimer?: ReturnType<typeof setTimeout>;
  private fade(on: boolean) {
    if (on === this.cues) return;
    this.cues = on;
    clearTimeout(this.clearTimer);
    this.map.setPaintProperty("swept-fill", "fill-opacity", on ? 0.42 : 0);
    this.map.setPaintProperty("back-glow", "line-opacity", on ? 0.55 : 0);
    this.map.setPaintProperty("back-line", "line-opacity", on ? 1 : 0);
    if (!on) this.clearTimer = setTimeout(() => {
      setData(this.map, "back", fc([]));
      setData(this.map, "swept", fc([]));
    }, 800);
  }

  destroy() {
    cancelAnimationFrame(this.raf);
    clearTimeout(this.tick);
    clearTimeout(this.clearTimer);
  }
}

// Axis from a herd's centroid to the target's, or undefined when the herd is around it.
export function roughAxis(points: LonLat[], target: Polygon): XY | undefined {
  if (!points.length) return undefined;
  const tr = target.coordinates[0].slice(0, -1);
  const tc = centroidOf(tr);
  const hc = centroidOf(points);
  const lat0 = tc[1];
  const a = xy(hc, lat0), b = xy(tc, lat0);
  const d: XY = [b[0] - a[0], b[1] - a[1]];
  const len = Math.hypot(d[0], d[1]);
  if (len < 2) return undefined;
  return [d[0] / len, d[1] / len];
}
