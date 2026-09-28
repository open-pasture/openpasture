#!/usr/bin/env bun
// One browser connected for the whole soak (dev tool, scripts/soak.sh), in
// the installed Chrome through scripts/shotdiff's playwright-core. Map tiles
// get a blank image; everything else goes to the server. It opens the map,
// then keeps the Herd table open (the live socket and the store run the
// same; the map's WebGL in software rendering would take several cores for
// hours), and goes back to the map for a screenshot every hour. Every ten
// minutes it writes a JSON line: console errors, failed requests, how long
// the page's own requests took (p50/p95), the page's JS heap, and how often
// the browser had to be started again (a crashed or closed browser is
// relaunched at once, so the soak keeps its client).
//
//   bun scripts/soak/ui.ts <base url> <out dir> <seconds>

import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { chromium } from "../shotdiff/node_modules/playwright-core/index.mjs";

const [base, out, secs] = process.argv.slice(2);
mkdirSync(out, { recursive: true });
const CHROME = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const BLANK = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=", "base64");
const origin = new URL(base).origin;
const until = Date.now() + Number(secs) * 1000;
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

let errors: string[] = [];
let failed: string[] = [];
let times: number[] = [];
let restarts = 0;
let dead = "";
let browser: any;
let page: any;

const open = async (hash: string) => {
  dead = "";
  browser = await chromium.launch({
    executablePath: CHROME,
    headless: true,
    args: ["--enable-unsafe-swiftshader", "--use-angle=swiftshader", "--enable-precise-memory-info"],
  });
  browser.on("disconnected", () => (dead ||= "browser disconnected"));
  page = await browser.newPage({ viewport: { width: 1024, height: 700 } });
  await page.route("**/*", (r: any) => (new URL(r.request().url()).origin === origin ? r.continue() : r.fulfill({ body: BLANK, contentType: "image/png" })));
  const started = new Map<object, number>();
  page.on("crash", () => (dead ||= "page crashed"));
  page.on("close", () => (dead ||= "page closed"));
  page.on("console", (m: any) => m.type() === "error" && errors.push(m.text()));
  page.on("pageerror", (e: any) => errors.push(String(e)));
  page.on("request", (r: any) => started.set(r, Date.now()));
  page.on("requestfinished", (r: any) => {
    const t = started.get(r);
    if (t && new URL(r.url()).origin === origin) times.push(Date.now() - t);
    started.delete(r);
  });
  page.on("requestfailed", (r: any) => {
    if (new URL(r.url()).origin === origin) failed.push(`${r.method()} ${r.url()} ${r.failure()?.errorText ?? ""}`);
  });
  await page.goto(`${base}/${hash}`, { waitUntil: "networkidle" });
};

const map = async (n: number) => {
  await page.evaluate(() => (location.hash = "#/"));
  await page.waitForTimeout(15_000);
  await page.screenshot({ path: join(out, `map-${String(n).padStart(2, "0")}.png`) });
  await page.evaluate(() => (location.hash = "#/herd"));
};

/// Wait `ms`, or less if the browser goes away.
const wait = async (ms: number) => {
  const end = Date.now() + ms;
  while (Date.now() < end && !dead) await sleep(Math.min(5_000, end - Date.now()));
};

/// Start the browser again, on the Herd table.
const reopen = async (why: string) => {
  errors.push(`client restarted: ${why}`);
  restarts += 1;
  for (;;) {
    try {
      await browser.close();
    } catch {}
    try {
      await open("#/herd");
      return;
    } catch (e) {
      errors.push(`client start failed: ${String(e).split("\n")[0]}`);
      if (Date.now() > until) return;
      await sleep(30_000);
    }
  }
};

await open("#/");
await map(0);
let n = 0;
while (Date.now() < until) {
  await wait(Math.min(600_000, Math.max(1000, until - Date.now())));
  if (dead) {
    await reopen(dead);
    continue;
  }
  n += 1;
  let heap: number | null = null;
  try {
    if (n % 6 === 0) await map(n / 6);
    await page.screenshot({ path: join(out, `herd-${String(n).padStart(2, "0")}.png`) });
    heap = await page.evaluate(() => (performance as any).memory?.usedJSHeapSize ?? null);
  } catch (e) {
    await reopen(String(e).split("\n")[0]);
  }
  const sorted = [...times].sort((a, b) => a - b);
  const pick = (q: number) => (sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))] : null);
  const line = {
    t: Math.floor(Date.now() / 1000),
    shot: n,
    requests: times.length,
    ms_p50: pick(0.5),
    ms_p95: pick(0.95),
    heap_mb: heap === null ? null : Math.round(heap / 1048576),
    restarts,
    errors,
    failed,
  };
  appendFileSync(join(out, "ui.jsonl"), JSON.stringify(line) + "\n");
  errors = [];
  failed = [];
  times = [];
}
await browser.close();
