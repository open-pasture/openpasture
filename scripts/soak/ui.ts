#!/usr/bin/env bun
// One browser connected for the whole soak (dev tool, scripts/soak.sh), in
// the installed Chrome through scripts/shotdiff's playwright-core. Map tiles
// get a blank image; everything else goes to the server. It opens the map,
// then keeps the Herd table open (the live socket and the store run the
// same; the map's WebGL in software rendering would take several cores for
// hours), and goes back to the map for a screenshot every hour. Every ten
// minutes it writes a JSON line: console errors, failed requests, and how
// long the page's own requests took (p50/p95).
//
//   bun scripts/soak/ui.ts <base url> <out dir> <seconds>

import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";
import { chromium } from "../shotdiff/node_modules/playwright-core/index.mjs";

const [base, out, secs] = process.argv.slice(2);
mkdirSync(out, { recursive: true });
const CHROME = process.env.CHROME ?? "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const BLANK = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=", "base64");

const browser = await chromium.launch({ executablePath: CHROME, headless: true, args: ["--enable-unsafe-swiftshader", "--use-angle=swiftshader"] });
const page = await browser.newPage({ viewport: { width: 1024, height: 700 } });
const origin = new URL(base).origin;
await page.route("**/*", (r) => (new URL(r.request().url()).origin === origin ? r.continue() : r.fulfill({ body: BLANK, contentType: "image/png" })));
let errors: string[] = [];
let failed: string[] = [];
let times: number[] = [];
const started = new Map<object, number>();
page.on("console", (m) => m.type() === "error" && errors.push(m.text()));
page.on("pageerror", (e) => errors.push(String(e)));
page.on("request", (r) => started.set(r, Date.now()));
page.on("requestfinished", (r) => {
  const t = started.get(r);
  if (t && new URL(r.url()).origin === origin) times.push(Date.now() - t);
  started.delete(r);
});
page.on("requestfailed", (r) => {
  if (new URL(r.url()).origin === origin) failed.push(`${r.method()} ${r.url()} ${r.failure()?.errorText ?? ""}`);
});
const map = async (n: number) => {
  await page.evaluate(() => (location.hash = "#/"));
  await page.waitForTimeout(15_000);
  await page.screenshot({ path: join(out, `map-${String(n).padStart(2, "0")}.png`) });
  await page.evaluate(() => (location.hash = "#/herd"));
};
await page.goto(base + "/#/", { waitUntil: "networkidle" });
await map(0);

const until = Date.now() + Number(secs) * 1000;
let n = 0;
while (Date.now() < until) {
  await page.waitForTimeout(Math.min(600_000, Math.max(1000, until - Date.now())));
  n += 1;
  if (n % 6 === 0) await map(n / 6);
  await page.screenshot({ path: join(out, `herd-${String(n).padStart(2, "0")}.png`) });
  const sorted = [...times].sort((a, b) => a - b);
  const pick = (q: number) => (sorted.length ? sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))] : null);
  appendFileSync(join(out, "ui.jsonl"), JSON.stringify({ t: Math.floor(Date.now() / 1000), shot: n, requests: times.length, ms_p50: pick(0.5), ms_p95: pick(0.95), errors, failed }) + "\n");
  errors = [];
  failed = [];
  times = [];
}
await browser.close();
