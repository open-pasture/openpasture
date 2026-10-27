// The /api/live socket: reconnects on its own and notices when it has gone quiet.
// - The server sends {"type":"ping"} every 15 s, so 45 s with no message means the socket is
//   dead even if it still says open (a laptop waking, a Wi-Fi change): it is closed and opened again.
// - Reconnects back off from 0.5 s to 8 s with jitter, so a farm's phones don't all knock at once
//   after a restart, and go at once when the browser comes back online or the page is shown again.
// - The token travels as a subprotocol, never in the URL (URLs end up in logs).

import type { LiveEvent } from "../api";
import { getToken } from "./http";
import { liveEvents } from "./x1";

export const SILENT_MS = 45_000;
// A socket heard from within this long is trusted when the page comes back (pings are 15 s apart).
const FRESH_MS = 20_000;
export const BACKOFF_MIN_MS = 500;
export const BACKOFF_MAX_MS = 8_000;
export const PROTOCOL = "openpasture";

// The wait before reconnect attempt `n` (0 first): half the doubling step plus up to the other half.
export function backoff(n: number, rand: () => number = Math.random): number {
  const step = Math.min(BACKOFF_MAX_MS, BACKOFF_MIN_MS * 2 ** Math.min(n, 10));
  return Math.round(step / 2 + (step / 2) * rand());
}

// What the socket offers: the plain protocol the server answers with, and the token as a second.
export function protocols(token: string): string[] {
  return token ? [PROTOCOL, `${PROTOCOL}.token.${token}`] : [PROTOCOL];
}

function open(path: string): WebSocket {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  return new WebSocket(`${proto}//${location.host}${path}`, protocols(getToken()));
}

// Live feed with reconnect. Returns an unsubscribe function.
export function live(onEvent: (e: LiveEvent) => void, onStatus?: (up: boolean) => void): () => void {
  let ws: WebSocket | null = null;
  let closed = false;
  let attempt = 0;
  let retry: ReturnType<typeof setTimeout> | undefined;
  let watchdog: ReturnType<typeof setTimeout> | undefined;

  let lastHeard = 0;
  // Any message, pings included, proves the socket alive for another SILENT_MS.
  const heard = () => {
    lastHeard = Date.now();
    clearTimeout(watchdog);
    watchdog = setTimeout(() => {
      // Dead without saying so: drop it and start again.
      const dead = ws;
      ws = null;
      dead?.close();
      if (dead) onStatus?.(false);
      schedule(0);
    }, SILENT_MS);
  };

  const schedule = (wait: number) => {
    clearTimeout(retry);
    if (!closed) retry = setTimeout(connect, wait);
  };

  const connect = () => {
    if (closed) return;
    const sock = open("/api/live");
    ws = sock;
    sock.onopen = () => {
      if (ws !== sock) return;
      attempt = 0;
      heard();
      onStatus?.(true);
    };
    sock.onmessage = (m) => {
      if (ws !== sock) return;
      heard();
      let msg: unknown;
      try {
        msg = JSON.parse(typeof m.data === "string" ? m.data : "");
      } catch {
        return; /* ignore malformed */
      }
      for (const e of liveEvents(msg)) {
        if ((e as { type: string }).type === "ping") continue;
        try {
          onEvent(e);
        } catch {
          /* one bad event doesn't drop the rest of its batch */
        }
      }
    };
    sock.onclose = () => {
      if (ws !== sock) return; // replaced already
      ws = null;
      clearTimeout(watchdog);
      onStatus?.(false);
      schedule(backoff(attempt++));
    };
    sock.onerror = () => sock.close();
  };

  // Back online, or the page shown again: don't wait out the backoff (or a dead socket).
  const now = () => {
    if (closed || document.visibilityState === "hidden") return;
    if (ws?.readyState === WebSocket.CONNECTING) return;
    // Open and pinged within the last beat and a bit: it's fine.
    if (ws?.readyState === WebSocket.OPEN && Date.now() - lastHeard < FRESH_MS) return;
    const stale = ws;
    ws = null;
    if (stale) {
      stale.close();
      onStatus?.(false);
    }
    attempt = 0;
    schedule(0);
  };
  window.addEventListener("online", now);
  document.addEventListener("visibilitychange", now);

  connect();
  return () => {
    closed = true;
    clearTimeout(retry);
    clearTimeout(watchdog);
    window.removeEventListener("online", now);
    document.removeEventListener("visibilitychange", now);
    const sock = ws;
    ws = null;
    sock?.close();
  };
}
