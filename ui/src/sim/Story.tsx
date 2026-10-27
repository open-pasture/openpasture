import { useEffect, useRef } from "react";
import { C } from "../map/base";
import { SIMULATED } from ".";

// Behind the first question: a paddock on real rangeland, cut into strips. The fence steps
// forward, the herd walks after it, and the ground they grazed regrows while it rests.
// A simulation in screen space, and it says so on screen for as long as it shows. Its
// animals are pixels on a canvas: nothing here is farm data and nothing is written.

type P = [number, number];
const POLY: P[] = [[0.12, 0.2], [0.46, 0.12], [0.86, 0.2], [0.9, 0.72], [0.66, 0.88], [0.2, 0.82], [0.08, 0.52]];
const STRIPS = 6;
const HEAD = 14;
const INTRO = 2.6; // seconds to trace the paddock
const STEP = 5; // seconds per strip

const lerp = (a: number, b: number, t: number) => a + (b - a) * t;
const clamp = (v: number, a: number, b: number) => Math.max(a, Math.min(b, v));

function inPoly(x: number, y: number, p: P[]) {
  let c = false;
  for (let i = 0, j = p.length - 1; i < p.length; j = i++) {
    const [xi, yi] = p[i], [xj, yj] = p[j];
    if (yi > y !== yj > y && x < ((xj - xi) * (y - yi)) / (yj - yi) + xi) c = !c;
  }
  return c;
}

function trace(ctx: CanvasRenderingContext2D, p: P[]) {
  ctx.beginPath();
  p.forEach(([x, y], i) => (i ? ctx.lineTo(x, y) : ctx.moveTo(x, y)));
  ctx.closePath();
}

type Cell = { x: number; y: number; g: number; n: number };
type Cow = { x: number; y: number; tx: number; ty: number; wait: number; blaze: boolean };

class Sim {
  p: P[];
  x0: number; x1: number; y0: number; y1: number;
  cols: number; rows: number;
  cells: (Cell | null)[] = [];
  herd: Cow[];
  k = 0;
  f = 0;

  constructor(w: number, h: number, readonly cell: number) {
    this.p = POLY.map(([x, y]) => [x * w, y * h]);
    const xs = this.p.map((q) => q[0]), ys = this.p.map((q) => q[1]);
    [this.x0, this.x1, this.y0, this.y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
    this.cols = Math.ceil(w / cell);
    this.rows = Math.ceil(h / cell);
    for (let r = 0; r < this.rows; r++)
      for (let c = 0; c < this.cols; c++) {
        const x = (c + 0.5) * cell, y = (r + 0.5) * cell;
        this.cells.push(inPoly(x, y, this.p) ? { x, y, g: 0.72 + Math.random() * 0.28, n: Math.random() } : null);
      }
    this.herd = Array.from({ length: HEAD }, (_, i) => {
      const [x, y] = this.pick();
      return { x, y, tx: x, ty: y, wait: Math.random() * 2, blaze: i === 2 };
    });
  }

  sx(k: number) {
    return this.x0 + ((this.x1 - this.x0) * k) / STRIPS;
  }

  pick(): P {
    for (let i = 0; i < 80; i++) {
      const x = lerp(this.sx(this.k), this.sx(this.k + 1), 0.12 + 0.76 * Math.random());
      const y = lerp(this.y0, this.y1, Math.random());
      if (inPoly(x, y, this.p)) return [x, y];
    }
    return [(this.sx(this.k) + this.sx(this.k + 1)) / 2, (this.y0 + this.y1) / 2];
  }

  advance() {
    this.k = (this.k + 1) % STRIPS;
    if (!this.k) this.f = 0;
    for (const a of this.herd) a.wait = Math.random() * 0.7;
  }

  step(dt: number) {
    this.f += (this.k - this.f) * Math.min(1, dt * 2.6);
    for (const a of this.herd) {
      a.wait -= dt;
      if (a.wait <= 0) {
        [a.tx, a.ty] = this.pick();
        a.wait = 2 + Math.random() * 3;
      }
      const dx = a.tx - a.x, dy = a.ty - a.y, d = Math.hypot(dx, dy), sp = Math.max(9, d * 1.1) * dt;
      if (d > sp) {
        a.x += (dx / d) * sp;
        a.y += (dy / d) * sp;
      } else {
        a.x = a.tx;
        a.y = a.ty;
      }
    }
    // Rest: everything outside the open strip regrows.
    const a0 = this.sx(this.k), a1 = this.sx(this.k + 1), R = this.cell * 2.3;
    for (const c of this.cells) if (c && !(c.x >= a0 && c.x < a1)) c.g = Math.min(1, c.g + dt * 0.028);
    for (const a of this.herd)
      for (let r = Math.floor((a.y - R) / this.cell); r <= Math.floor((a.y + R) / this.cell); r++)
        for (let c = Math.floor((a.x - R) / this.cell); c <= Math.floor((a.x + R) / this.cell); c++) {
          if (r < 0 || c < 0 || r >= this.rows || c >= this.cols) continue;
          const cl = this.cells[r * this.cols + c];
          if (!cl) continue;
          const d = Math.hypot(cl.x - a.x, cl.y - a.y);
          if (d < R) cl.g = Math.max(0.06, cl.g - dt * 0.1 * (1 - d / R));
        }
  }

  draw(ctx: CanvasRenderingContext2D, w: number, h: number, t: number) {
    ctx.clearRect(0, 0, w, h);
    const s = this.cell, scan = lerp(this.x0 - 80, this.x1 + 80, clamp(t / INTRO, 0, 1));
    for (const c of this.cells) {
      if (!c) continue;
      const x = c.x - s / 2 + 1, y = c.y - s / 2 + 1;
      if (c.x < scan) {
        ctx.fillStyle = "rgba(237,236,232,.045)";
        ctx.fillRect(x, y, s - 2, s - 2);
      }
      const near = (scan - c.x) / 80;
      if (near > 0 && near < 1) {
        ctx.fillStyle = `rgba(159,215,96,${(1 - near) * 0.4 * (0.5 + c.n * 0.5)})`;
        ctx.fillRect(x, y, s - 2, s - 2);
      }
      if (c.g < 0.95) {
        ctx.fillStyle = `rgba(42,30,16,${(1 - c.g) * 0.62})`;
        ctx.fillRect(x, y, s - 2, s - 2);
      }
    }

    const show = clamp((t - INTRO + 0.4) / 0.6, 0, 1);
    ctx.save();
    trace(ctx, this.p);
    ctx.clip();
    ctx.globalAlpha = show;
    ctx.fillStyle = "rgba(159,215,96,.09)";
    ctx.fillRect(this.sx(this.f), this.y0, this.sx(this.f + 1) - this.sx(this.f), this.y1 - this.y0);
    ctx.strokeStyle = "rgba(237,236,232,.16)";
    ctx.lineWidth = 1;
    for (let k = 1; k < STRIPS; k++) this.line(ctx, this.sx(k));
    // Where the fence goes next, where it is, and the one behind.
    if (this.f + 2 <= STRIPS + 0.01) this.fence(ctx, this.sx(this.f + 2), "rgba(159,215,96,.5)", 1.5, [4, 5]);
    this.fence(ctx, this.sx(this.f), "rgba(159,215,96,.55)", 2);
    this.fence(ctx, this.sx(this.f + 1), C.grass, 2.5);
    ctx.restore();

    const L = this.p.reduce((n, q, i) => n + Math.hypot(this.p[(i + 1) % this.p.length][0] - q[0], this.p[(i + 1) % this.p.length][1] - q[1]), 0);
    ctx.save();
    trace(ctx, this.p);
    ctx.setLineDash([L, L]);
    ctx.lineDashOffset = L * (1 - clamp(t / (INTRO * 0.8), 0, 1));
    ctx.strokeStyle = "rgba(237,236,232,.85)";
    ctx.lineWidth = 1.5;
    ctx.stroke();
    ctx.restore();
    if (t > INTRO * 0.8) {
      ctx.fillStyle = C.fg;
      for (const [x, y] of this.p) ctx.fillRect(x - 3, y - 3, 6, 6);
    }

    for (const a of this.herd) {
      const x = Math.round(a.x - 3), y = Math.round(a.y - 3);
      ctx.fillStyle = "rgba(0,0,0,.55)";
      ctx.fillRect(x + 1, y + 1, 6, 6);
      ctx.fillStyle = a.blaze ? C.blaze : C.fg;
      ctx.fillRect(x, y, 6, 6);
    }
  }

  line(ctx: CanvasRenderingContext2D, x: number) {
    ctx.beginPath();
    ctx.moveTo(x, this.y0 - 2);
    ctx.lineTo(x, this.y1 + 2);
    ctx.stroke();
  }

  fence(ctx: CanvasRenderingContext2D, x: number, color: string, width: number, dash: number[] = []) {
    ctx.strokeStyle = color;
    ctx.lineWidth = width;
    ctx.setLineDash(dash);
    this.line(ctx, x);
    ctx.setLineDash([]);
  }
}

export function Story({ active }: { active: boolean }) {
  const el = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    if (!active) return;
    const cv = el.current!, ctx = cv.getContext("2d")!;
    const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
    let sim: Sim, w = 0, h = 0, t = 0, next = INTRO + STEP, raf = 0, last = performance.now();

    const tick = (dt: number) => {
      t += dt;
      if (t > next) {
        sim.advance();
        next += STEP;
      }
      sim.step(dt);
    };
    const size = () => {
      const dpr = Math.min(2, devicePixelRatio || 1);
      ({ width: w, height: h } = cv.getBoundingClientRect());
      cv.width = w * dpr;
      cv.height = h * dpr;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      sim = new Sim(w, h, Math.max(9, Math.round(w / 96)));
      t = 0;
      next = INTRO + STEP;
      if (still) {
        for (let i = 0; i < 240; i++) tick(1 / 20);
        sim.draw(ctx, w, h, t);
      }
    };
    const ro = new ResizeObserver(size);
    ro.observe(cv);
    size();

    if (!still) {
      const loop = (now: number) => {
        tick(Math.min(0.05, (now - last) / 1000));
        last = now;
        sim.draw(ctx, w, h, t);
        raf = requestAnimationFrame(loop);
      };
      raf = requestAnimationFrame(loop);
    }
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, [active]);

  return (
    <div className={active ? "story" : "story off"}>
      <canvas ref={el} aria-hidden="true" />
      <p className="sw mono">{SIMULATED}</p>
    </div>
  );
}
