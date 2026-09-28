// The install: the manifest's fields, its icons' real sizes, and the page's meta.

import { expect, test } from "bun:test";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

const UI = join(import.meta.dir, "../../..");
const pub = (p: string) => join(UI, "public", p.replace(/^\//, ""));

// Width and height from a PNG's IHDR.
function pngSize(file: string): [number, number] {
  const b = readFileSync(file);
  expect([...b.subarray(0, 8)]).toEqual([137, 80, 78, 71, 13, 10, 26, 10]);
  expect(b.subarray(12, 16).toString("latin1")).toBe("IHDR");
  return [b.readUInt32BE(16), b.readUInt32BE(20)];
}

test("the manifest names the app, starts on the map, installs standalone in the app's colours", () => {
  const m = JSON.parse(readFileSync(pub("manifest.webmanifest"), "utf8"));
  expect(m.name).toBe("openpasture");
  expect(m.short_name).toBe("openpasture");
  expect(m.start_url).toBe("/#/map");
  expect(m.scope).toBe("/");
  expect(m.display).toBe("standalone");
  expect(m.background_color).toBe("#0B0C09");
  expect(m.theme_color).toBe("#0B0C09");
});

test("icons are the sizes they say: 192 and 512, and maskable", () => {
  const m = JSON.parse(readFileSync(pub("manifest.webmanifest"), "utf8"));
  const icons: { src: string; sizes: string; type: string; purpose: string }[] = m.icons;
  for (const i of icons) {
    expect(i.type).toBe("image/png");
    const [w, h] = pngSize(pub(i.src));
    expect(`${w}x${h}`).toBe(i.sizes);
  }
  const has = (sizes: string, purpose: string) => icons.some((i) => i.sizes === sizes && i.purpose.split(" ").includes(purpose));
  expect([has("192x192", "any"), has("512x512", "any"), has("512x512", "maskable")]).toEqual([true, true, true]);
  // The service worker's notification icons, and the Home Screen icon for iPhones.
  expect(pngSize(pub("/icons/icon-192.png"))).toEqual([192, 192]);
  expect(pngSize(pub("/icons/badge-96.png"))).toEqual([96, 96]);
  expect(pngSize(pub("/icons/apple-touch-icon.png"))).toEqual([180, 180]);
});

test("the page links the manifest, sets the theme colour and fills the screen past the notch", () => {
  const html = readFileSync(join(UI, "index.html"), "utf8");
  expect(html).toContain('<link rel="manifest" href="/manifest.webmanifest" />');
  expect(html).toContain('<meta name="theme-color" content="#0B0C09" />');
  expect(html).toMatch(/name="viewport" content="[^"]*viewport-fit=cover/);
  expect(html).toContain('<link rel="apple-touch-icon" href="/icons/apple-touch-icon.png" />');
  expect(existsSync(pub("sw.js"))).toBe(true);
});
