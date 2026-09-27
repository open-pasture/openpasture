#!/usr/bin/env bun
// Dev tool: screenshot diffs and frame times of the openpasture UI, in the installed Chrome.
//
//   bun scripts/shotdiff/shotdiff.ts diff <urlA> <urlB>     shoot both, compare, exit 1 over --max
//   bun scripts/shotdiff/shotdiff.ts shoot <url> <dir>      screenshots only
//   bun scripts/shotdiff/shotdiff.ts compare <dirA> <dirB>  compare two shoot folders
//   bun scripts/shotdiff/shotdiff.ts frames <url>           rAF frame times while panning the map
//                                   (also: --frames <url>)
// scripts/shotdiff/shotdiff runs the same and installs the dependencies on first use.
//
// Options:
//   --routes "#/,#/data,#/settings"  hash routes to shoot (default)
//   --size 1440x900                  viewport
//   --max 0.5                        largest allowed share of differing pixels, in %
//   --out <dir>                      where diff images go (default /tmp/shotdiff)
//   --wait 2500                      ms to let the map and fades settle after the network is idle
//   --tiles                          load real imagery (by default every tile is one blank image, so
//                                    shots don't change with what the imagery server sends)
//   --token <t>                      app token, for a server that isn't on this machine
//   --seconds 10 --route "#/"        frames: how long to pan, and where
//   --headed                         a visible window (frames on the real GPU)
//   --chrome <path>                  Chrome binary (default: $CHROME or /Applications/Google Chrome.app)
//
// Both pages must hold the same data and no sim may run. Two servers can't open one data dir at
// once: give the second a copy (cp -R), or keep one debug server (it reads ui/dist from disk),
// `shoot` one build, put the other build in ui/dist, `shoot` again, then `compare`.

import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import pixelmatch from "pixelmatch";
import { chromium, type Browser, type Page } from "playwright-core";
import { PNG } from "pngjs";

type Opts = Record<string, string | boolean>;

function parse(argv: string[]): { cmd: string; args: string[]; opts: Opts } {
  const args: string[] = [];
  const opts: Opts = {};
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) args.push(a);
    else if (["tiles", "headed", "help", "frames"].includes(a.slice(2))) opts[a.slice(2)] = true;
    else opts[a.slice(2)] = argv[++i] ?? "";
  }
  if (opts.frames) return { cmd: "frames", args, opts };
  return { cmd: args.shift() ?? "help", args, opts };
}

const str = (o: Opts, k: string, d: string) => (typeof o[k] === "string" ? (o[k] as string) : d);
const numOpt = (o: Opts, k: string, d: number) => {
  const v = Number(str(o, k, String(d)));
  if (!Number.isFinite(v)) throw new Error(`--${k} wants a number`);
  return v;
};

const CHROME = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

// A 256 px tile of the app's background colour.
const BLANK = (() => {
  const png = new PNG({ width: 256, height: 256 });
  for (let i = 0; i < png.data.length; i += 4) png.data.set([0x0b, 0x0c, 0x09, 255], i);
  return PNG.sync.write(png);
})();

async function launch(o: Opts): Promise<Browser> {
  return chromium.launch({
    executablePath: str(o, "chrome", process.env.CHROME ?? CHROME),
    headless: !o.headed,
    // WebGL without a GPU in headless Chrome.
    args: o.headed ? [] : ["--enable-unsafe-swiftshader", "--use-angle=swiftshader"],
  });
}

async function open(browser: Browser, base: string, o: Opts): Promise<Page> {
  const [w, h] = str(o, "size", "1440x900").split("x").map(Number);
  const ctx = await browser.newContext({ viewport: { width: w, height: h }, deviceScaleFactor: 1, reducedMotion: "reduce" });
  const token = str(o, "token", "");
  if (token) await ctx.addInitScript((t) => localStorage.setItem("openpasture.token", t), token);
  const origin = new URL(base).origin;
  // Only the server itself answers. Images from elsewhere (map tiles) get a blank tile so the
  // map still loads; anything else from elsewhere (geocoder) fails.
  await ctx.route("**/*", (route) => {
    const req = route.request();
    const u = new URL(req.url());
    if (u.origin === origin || u.protocol === "data:" || u.protocol === "blob:" || o.tiles) return route.continue();
    if (req.resourceType() === "image" || /\/tile\//.test(u.pathname)) return route.fulfill({ status: 200, contentType: "image/png", body: BLANK });
    return route.abort();
  });
  return ctx.newPage();
}

async function settle(page: Page, o: Opts) {
  await page.waitForLoadState("networkidle").catch(() => {});
  await page.evaluate(() => document.fonts.ready.then(() => undefined));
  await page.waitForTimeout(numOpt(o, "wait", 2500));
}

const fileFor = (route: string) => (route.replace(/^#\/?/, "").replace(/[^a-z0-9]+/gi, "_") || "map") + ".png";

async function shoot(base: string, dir: string, o: Opts) {
  mkdirSync(dir, { recursive: true });
  const routes = str(o, "routes", "#/,#/data,#/settings").split(",").map((r) => r.trim()).filter(Boolean);
  const browser = await launch(o);
  try {
    for (const route of routes) {
      const page = await open(browser, base, o);
      await page.goto(new URL(route, base.endsWith("/") ? base : base + "/").toString());
      await settle(page, o);
      await page.screenshot({ path: join(dir, fileFor(route)), animations: "disabled", caret: "hide" });
      await page.context().close();
      console.log(`shot ${route} → ${join(dir, fileFor(route))}`);
    }
  } finally {
    await browser.close();
  }
}

function compare(dirA: string, dirB: string, o: Opts): boolean {
  const out = str(o, "out", "/tmp/shotdiff");
  const max = numOpt(o, "max", 0.5);
  mkdirSync(out, { recursive: true });
  let ok = true;
  const names = readdirSync(dirA).filter((f) => f.endsWith(".png")).sort();
  if (!names.length) throw new Error(`no screenshots in ${dirA}`);
  for (const name of names) {
    const a = PNG.sync.read(readFileSync(join(dirA, name)));
    let b: PNG;
    try {
      b = PNG.sync.read(readFileSync(join(dirB, name)));
    } catch {
      console.log(`${name.padEnd(16)} missing in ${dirB}`);
      ok = false;
      continue;
    }
    if (a.width !== b.width || a.height !== b.height) {
      console.log(`${name.padEnd(16)} sizes differ: ${a.width}x${a.height} vs ${b.width}x${b.height}`);
      ok = false;
      continue;
    }
    const diff = new PNG({ width: a.width, height: a.height });
    const n = pixelmatch(a.data, b.data, diff.data, a.width, a.height, { threshold: 0.1 });
    const pct = (n / (a.width * a.height)) * 100;
    writeFileSync(join(out, name), PNG.sync.write(diff));
    const pass = pct <= max;
    ok &&= pass;
    console.log(`${name.padEnd(16)} ${pct.toFixed(3).padStart(7)}%  ${String(n).padStart(7)} px  ${pass ? "ok" : `over ${max}%`}  diff ${join(out, name)}`);
  }
  return ok;
}

// p50 / p95 / max of requestAnimationFrame gaps while the map is dragged around a loop.
async function frames(base: string, o: Opts) {
  const seconds = numOpt(o, "seconds", 10);
  const browser = await launch(o);
  try {
    const page = await open(browser, base, o);
    await page.goto(new URL(str(o, "route", "#/"), base.endsWith("/") ? base : base + "/").toString());
    await page.waitForSelector(".maplibregl-canvas");
    await settle(page, o);
    const box = await page.locator(".maplibregl-canvas").first().boundingBox();
    if (!box) throw new Error("no map on the page");
    await page.evaluate(() => {
      const w = window as unknown as { __gaps: number[]; __stop: boolean };
      w.__gaps = [];
      w.__stop = false;
      let last = 0;
      const tick = (t: number) => {
        if (last) w.__gaps.push(t - last);
        last = t;
        if (!w.__stop) requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    });
    const cx = box.x + box.width / 2, cy = box.y + box.height / 2;
    const r = Math.min(box.width, box.height) / 4;
    const t0 = Date.now();
    await page.mouse.move(cx, cy);
    await page.mouse.down();
    // A slow circle, a quarter of the map across, round and round.
    while (Date.now() - t0 < seconds * 1000) {
      const a = ((Date.now() - t0) / 2000) * Math.PI * 2;
      await page.mouse.move(cx + Math.cos(a) * r - r, cy + Math.sin(a) * r, { steps: 2 });
    }
    await page.mouse.up();
    const gaps = await page.evaluate(() => {
      const w = window as unknown as { __gaps: number[]; __stop: boolean };
      w.__stop = true;
      return w.__gaps;
    });
    gaps.sort((a, b) => a - b);
    const q = (p: number) => gaps[Math.min(gaps.length - 1, Math.floor(p * gaps.length))] ?? NaN;
    console.log(`frames ${gaps.length}  p50 ${q(0.5).toFixed(1)} ms  p95 ${q(0.95).toFixed(1)} ms  max ${(gaps[gaps.length - 1] ?? NaN).toFixed(1)} ms  (${seconds} s panning, ${o.headed ? "headed" : "headless"})`);
  } finally {
    await browser.close();
  }
}

async function main() {
  const { cmd, args, opts } = parse(process.argv.slice(2));
  if (cmd === "shoot" && args.length === 2) return void (await shoot(args[0], args[1], opts));
  if (cmd === "compare" && args.length === 2) return void process.exit(compare(args[0], args[1], opts) ? 0 : 1);
  if (cmd === "diff" && args.length === 2) {
    const root = str(opts, "out", "/tmp/shotdiff");
    const [a, b] = [join(root, "a"), join(root, "b")];
    await shoot(args[0], a, opts);
    await shoot(args[1], b, opts);
    return void process.exit(compare(a, b, { ...opts, out: join(root, "diff") }) ? 0 : 1);
  }
  if (cmd === "frames" && args.length === 1) return void (await frames(args[0], opts));
  const usage = readFileSync(new URL(import.meta.url), "utf8").split("\n").slice(1);
  console.log(usage.slice(0, usage.findIndex((l) => !l.startsWith("//"))).map((l) => l.replace(/^\/\/ ?/, "")).join("\n"));
  process.exit(cmd === "help" || opts.help ? 0 : 2);
}

main().catch((e) => {
  console.error(e instanceof Error ? e.message : e);
  process.exit(2);
});
