// The one check every bun or node dev script runs on a server address before
// it connects: plain http/ws on this machine (127.0.0.1, localhost, [::1]).
// Staging and production are refused before any lookup. scripts/local_only.py
// is the same check for Python; scripts/check-local-only.py fails CI for a
// script that connects without one.
//
//   import { localOnly } from "../local-only.mjs";
//   const base = localOnly(process.argv[2]);   // throws, naming why, or returns it without a trailing slash
const LOCAL_HOSTS = new Set(["127.0.0.1", "localhost", "[::1]"]);

export function localOnly(url) {
  let u;
  try {
    u = new URL(url);
  } catch {
    throw new Error(`${JSON.stringify(url)} is not a URL`);
  }
  if (u.protocol !== "http:" && u.protocol !== "ws:") {
    throw new Error(`${url}: dev scripts drive plain http/ws on this machine, never ${u.protocol}`);
  }
  if (!LOCAL_HOSTS.has(u.hostname)) {
    throw new Error(`${url}: ${u.hostname} is not this machine. Dev scripts never drive staging or production; run a local server (CONTRIBUTING.md)`);
  }
  return url.replace(/\/+$/, "");
}
