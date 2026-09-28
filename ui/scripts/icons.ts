#!/usr/bin/env bun
// Dev tool: the app's install icons, drawn from the pixel mark (ui/src/ui/Mark.tsx) on the bg token.
//   bun scripts/icons.ts     writes ui/public/icons/*.png
// "any" icons hold the mark at 62 % of the square; maskable ones keep it inside the 80 % safe
// circle (a phone crops them round or squircle); the badge is the mark alone in white (Android's
// status bar tints it). No dependencies: PNG by hand over node:zlib.

import { deflateSync } from "node:zlib";
import { writeFileSync } from "node:fs";
import { join } from "node:path";

const ROWS = ["###..", "#.#..", "###..", "...##", "...#b"];
const BG = [0x0b, 0x0c, 0x09, 255];
const FG = [0xf3, 0xf2, 0xea, 255];
const BLAZE = [0xff, 0x6a, 0x2b, 255];

const CRC = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();
function crc32(buf: Uint8Array) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type: string, data: Uint8Array) {
  const out = new Uint8Array(12 + data.length);
  const v = new DataView(out.buffer);
  v.setUint32(0, data.length);
  out.set(new TextEncoder().encode(type), 4);
  out.set(data, 8);
  v.setUint32(8 + data.length, crc32(out.subarray(4, 8 + data.length)));
  return out;
}
function png(size: number, px: (x: number, y: number) => number[]) {
  const raw = new Uint8Array(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    for (let x = 0; x < size; x++) raw.set(px(x, y), y * (size * 4 + 1) + 1 + x * 4);
  }
  const ihdr = new Uint8Array(13);
  const v = new DataView(ihdr.buffer);
  v.setUint32(0, size);
  v.setUint32(4, size);
  ihdr.set([8, 6, 0, 0, 0], 8);
  const parts = [new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10]), chunk("IHDR", ihdr), chunk("IDAT", deflateSync(raw, { level: 9 })), chunk("IEND", new Uint8Array())];
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let i = 0;
  for (const p of parts) (out.set(p, i), (i += p.length));
  return out;
}

// The mark `share` of the square wide, centred; cells with a gap of 0.3 cell, as the app draws it.
function mark(size: number, share: number, bg: number[] | null, fg = FG, blaze = BLAZE) {
  const cell = (size * share) / 6.2;
  const gap = cell * 0.3;
  const x0 = (size - (5 * cell + 4 * gap)) / 2;
  return (x: number, y: number) => {
    const cx = (x + 0.5 - x0) / (cell + gap), cy = (y + 0.5 - x0) / (cell + gap);
    const i = Math.floor(cx), j = Math.floor(cy);
    const inCell = i >= 0 && j >= 0 && i < 5 && j < 5 && cx - i < cell / (cell + gap) && cy - j < cell / (cell + gap);
    const c = inCell ? ROWS[j][i] : ".";
    if (c === "#") return fg;
    if (c === "b") return blaze;
    return bg ?? [0, 0, 0, 0];
  };
}

const dir = join(import.meta.dir, "..", "public", "icons");
const write = (name: string, size: number, px: (x: number, y: number) => number[]) => {
  writeFileSync(join(dir, name), png(size, px));
  console.log(`${name} ${size}x${size}`);
};
write("icon-192.png", 192, mark(192, 0.62, BG));
write("icon-512.png", 512, mark(512, 0.62, BG));
write("maskable-192.png", 192, mark(192, 0.5, BG));
write("maskable-512.png", 512, mark(512, 0.5, BG));
write("apple-touch-icon.png", 180, mark(180, 0.6, BG));
write("badge-96.png", 96, mark(96, 0.8, null, [255, 255, 255, 255], [255, 255, 255, 255]));
