// Pixel art for map features: a point icon per kind and the red hatch exclusions are filled
// with. Rows are strings, "#" a pixel, "." empty. Bitmaps are built here (no MapLibre), so the
// overlay adds them with map.addImage(id, image, { pixelRatio: PR }).

import type { FeatureKind } from "../../api";

// Device pixels per CSS pixel in every bitmap.
export const PR = 2;
// CSS pixels per grid pixel: chunky, like the rest of the pixel marks.
const CELL = 2;
// CSS pixels of dark edge around an icon, so it reads on bright imagery.
const EDGE = 1;

const INK = "#0C1606";

export type IconKind = Extract<FeatureKind, "water" | "gate" | "shade" | "hazard">;

export const ICONS: Record<IconKind, readonly string[]> = {
  water: [
    "...#...",
    "..###..",
    ".#####.",
    "#######",
    "#######",
    ".#####.",
    "..###..",
  ],
  gate: [
    "#.....#",
    "#######",
    "#.#.#.#",
    "#######",
    "#.....#",
  ],
  shade: [
    "..###..",
    ".#####.",
    "#######",
    "#######",
    ".#####.",
    "...#...",
    "...#...",
  ],
  hazard: [
    "...#...",
    "..###..",
    "..#.#..",
    ".##.##.",
    ".#####.",
    "###.###",
    "#######",
  ],
};

export interface Bitmap { width: number; height: number; data: Uint8Array }

const rgb = (hex: string) => [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16));

// A grid drawn in `fill`, each grid pixel CELL css pixels, with an EDGE-wide ink outline.
export function iconBitmap(rows: readonly string[], fill: string): Bitmap {
  const cell = CELL * PR, edge = EDGE * PR;
  const gw = rows[0].length, gh = rows.length;
  const width = gw * cell + 2 * edge, height = gh * cell + 2 * edge;
  const on = (x: number, y: number) => {
    const gx = Math.floor((x - edge) / cell), gy = Math.floor((y - edge) / cell);
    return x >= edge && y >= edge && gx < gw && gy < gh && rows[gy][gx] === "#";
  };
  const data = new Uint8Array(width * height * 4);
  const [r, g, b] = rgb(fill);
  const [ir, ig, ib] = rgb(INK);
  for (let y = 0; y < height; y++)
    for (let x = 0; x < width; x++) {
      const o = (y * width + x) * 4;
      if (on(x, y)) {
        data.set([r, g, b, 255], o);
        continue;
      }
      // Within `edge` of a lit pixel: outline.
      let near = false;
      for (let dy = -edge; dy <= edge && !near; dy++)
        for (let dx = -edge; dx <= edge && !near; dx++) near = on(x + dx, y + dy);
      if (near) data.set([ir, ig, ib, 210], o);
    }
  return { width, height, data };
}

// A repeating tile of diagonal stripes, one grid pixel thick, for fill-pattern.
export const HATCH = ["#...", ".#..", "..#.", "...#"];

export function hatchBitmap(color: string, alpha = 200): Bitmap {
  const cell = CELL * PR;
  const n = HATCH.length * cell;
  const data = new Uint8Array(n * n * 4);
  const [r, g, b] = rgb(color);
  for (let y = 0; y < n; y++)
    for (let x = 0; x < n; x++)
      if (HATCH[Math.floor(y / cell)][Math.floor(x / cell)] === "#") data.set([r, g, b, alpha], (y * n + x) * 4);
  return { width: n, height: n, data };
}
