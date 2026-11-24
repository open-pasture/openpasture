// app-shell: open the UI the server serves in Chromium, sign in with the app
// token at the Token prompt like a person would, and see the farm the launch
// made. Screenshots and a JSON result go to $OP_VERIFY_RUN/evidence/.
// Run through `bin/verify drive app-shell`, which has checked OP_BASE_URL.
import { spawnSync } from "node:child_process";
import { mkdirSync, writeFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";

// The same guard as every entry point, before the browser is even loaded.
const guard = spawnSync("python3", [join(dirname(import.meta.path), "local_only.py"), process.env.OP_BASE_URL ?? ""], { encoding: "utf8" });
if (guard.status !== 0) {
  process.stderr.write(guard.stderr);
  process.exit(3);
}
const base = guard.stdout.trim();
const token = process.env.OP_TOKEN ?? "";
const out = join(process.env.OP_VERIFY_RUN ?? ".", "evidence");
mkdirSync(out, { recursive: true });

const { chromium } = await import("playwright-core");
const exe = process.env.CHROME ?? ["/usr/bin/chromium", "/usr/bin/google-chrome", "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"].find(existsSync);
if (!exe) {
  console.error("no Chromium or Chrome found; set CHROME");
  process.exit(2);
}
const browser = await chromium.launch({ executablePath: exe, headless: true });
const fails: string[] = [];
const check = (ok: boolean, what: string) => {
  console.log((ok ? "PASS " : "FAIL ") + what);
  if (!ok) fails.push(what);
};
try {
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(String(e)));
  await page.goto(base + "/", { waitUntil: "domcontentloaded" });
  const prompt = page.getByLabel("Token");
  await prompt.waitFor({ timeout: 20_000 });
  await page.screenshot({ path: join(out, "app-shell-1-token.png") });
  check(true, "a fresh browser is asked for a token");
  await prompt.fill(token);
  await prompt.press("Enter");
  // The desktop shell: the herd collar-sim made, its paddock, and the status bar's
  // "Simulated collars" (every collar on the farm is simulated).
  const herd = page.getByText("Simulated herd").first();
  await herd.waitFor({ timeout: 30_000 }).catch(() => {});
  check(await herd.isVisible(), "after signing in the shell shows the herd (Simulated herd)");
  const paddock = page.getByText("Sim 1", { exact: true }).first();
  check(await paddock.isVisible().catch(() => false), "and its paddock (Sim 1)");
  const sim = page.getByText("Simulated collars", { exact: true }).first();
  await sim.waitFor({ timeout: 15_000 }).catch(() => {});
  check(await sim.isVisible(), "the status bar says Simulated collars");
  await page.screenshot({ path: join(out, "app-shell-2-farm.png") });
  check(errors.length === 0, `no uncaught page errors (${errors.length})`);
  writeFileSync(join(out, "app-shell.json"), JSON.stringify({ base, url: page.url(), errors, fails }, null, 2));
} finally {
  await browser.close();
}
console.log(`app-shell: ${fails.length ? "FAIL" : "PASS"} (evidence ${out})`);
process.exit(fails.length ? 1 : 0);
