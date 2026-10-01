// What openpasture needs from the farmer, as asks: each a question with the answer it expects
// first. Everything else it is doing (moving the herd, bringing an animal back, deciding) is not
// an ask; the pilot line shows that. Pure: no React, no store.
//
// Asks are the herd's waiting decision and every open alert on the farm, most urgent first. An
// alert the farmer has acked ("I'll check") is no longer an ask: it is watched until it clears.

import type { Alert, Decision, Severity } from "../api";

export type Ask =
  | { k: "decision"; id: string; herdId: string; rank: number; at: string; d: Decision }
  | { k: "alert"; id: string; herdId?: string; rank: number; at: string; a: Alert };

const SEV: Record<Severity, number> = { info: 1, warning: 2, critical: 4 };
// A decision waits on the farmer. One on a timer goes ahead by itself, so it ranks under one that
// waits, and both under a critical alert (an animal out, the herd gone quiet).
const DECISION = 3;
const DECISION_TIMED = 2.5;

// The asks for the herd showing (`herdId`), from its decisions and the farm's unresolved alerts.
// Other herds' waiting decisions come as their decision_waiting alerts; this herd's as its own.
export function asksOf(decisions: readonly Decision[], alerts: readonly Alert[], herdId: string | undefined): Ask[] {
  const out: Ask[] = [];
  const d = decisions.find((x) => x.status === "proposed" && x.herd_id === herdId);
  if (d) out.push({ k: "decision", id: d.id, herdId: d.herd_id, rank: d.apply_at ? DECISION_TIMED : DECISION, at: d.created_at, d });
  for (const a of alerts) {
    if (a.status !== "open") continue;
    if (a.kind === "decision_waiting" && a.herd_id === herdId) continue;
    out.push({ k: "alert", id: a.id, herdId: a.herd_id, rank: a.kind === "decision_waiting" ? DECISION : SEV[a.severity], at: a.opened_at, a });
  }
  // Most urgent, then this herd's before others', then newest.
  return out.sort((x, y) => y.rank - x.rank || Number(y.herdId === herdId) - Number(x.herdId === herdId) || y.at.localeCompare(x.at));
}

// Acked alerts: the farmer has it in hand; openpasture keeps watching until it clears.
export const watched = (alerts: readonly Alert[], herdId: string | undefined) =>
  alerts.filter((a) => a.status === "acked" && a.kind !== "decision_waiting" && (!a.herd_id || a.herd_id === herdId));

// ---- the words --------------------------------------------------------------------------

// How openpasture asks about an alert: the question under its title, and the answers. `take`
// acks it ("I'll check": openpasture stops asking and watches), `done` resolves it.
export interface AlertWords { ask: string; take: string; done: string; show?: string }

const WORDS: Record<string, AlertWords> = {
  herd_silent: { ask: "No collar has reported. The base station or its power is the usual cause. Can you check?", take: "I'll check", done: "Fixed" },
  silent: { ask: "These collars stopped reporting. Can you look them over?", take: "I'll check", done: "Fixed", show: "Show" },
  outside: { ask: "Their collars are cueing them back. Do you want to go and look?", take: "I'm going", done: "They're back", show: "Show" },
  escaped: { ask: "Their collars are cueing them back. Do you want to go and look?", take: "I'm going", done: "They're back", show: "Show" },
  stragglers: { ask: "The move left them behind. Can you walk them up?", take: "I'm going", done: "They're with the herd", show: "Show" },
  move_stalled: { ask: "The herd stopped short of the new boundary. Can you take a look?", take: "I'll look", done: "Sorted", show: "Show" },
  low_battery: { ask: "Can you charge or swap these batteries?", take: "I'll do it", done: "Done" },
  boundary_not_applied: { ask: "These collars haven't taken the new boundary. Can you check they're in range?", take: "I'll check", done: "Fixed", show: "Show" },
  schedule_not_stored: { ask: "These collars are missing the next strip. Can you check they're in range?", take: "I'll check", done: "Fixed", show: "Show" },
  drop_off: { ask: "Not moving can mean a collar came off. Can you check?", take: "I'll check", done: "It's fine", show: "Show" },
  gps_degraded: { ask: "Positions from these collars are rough for now. Worth a look if it lasts.", take: "Noted", done: "Fixed", show: "Show" },
  fit_check_due: { ask: "These collars are due a fit check.", take: "I'll do it", done: "Checked" },
};
const DEFAULT_WORDS: AlertWords = { ask: "", take: "Noted", done: "Resolved", show: "Show" };

export const alertWords = (kind: string): AlertWords => WORDS[kind] ?? DEFAULT_WORDS;

// A decision as the question openpasture asks: "Move Herd 1 to P2?", "Keep Herd 1 in P1?", or
// the brain's own question when it needs to know something.
export function decisionAsk(d: Decision, herd: string, pad: (id?: string) => string | undefined, here?: string): string {
  const sched = scheduleNext(d);
  if (d.action === "MOVE") return `Move ${herd} to ${pad(d.to_paddock_id) ?? "the new boundary"}?`;
  if (d.action === "STAY" && sched) return `Open strip ${sched.strip} of ${sched.of} ${sched.opens}?`;
  if (d.action === "STAY") return `Keep ${herd} in ${pad(here) ?? "place"}?`;
  if (d.action === "HOLD") return `Hold ${herd} on today's strip?`;
  return d.need?.trim() || `What should ${herd} do next?`;
}

// The answers to a decision: yes, and no, in its own words.
export function decisionAnswers(d: Decision): { yes: string; no: string } {
  if (d.apply_at) return { yes: d.action === "MOVE" ? "Move now" : "Do it now", no: "Hold" };
  if (d.action === "MOVE") return { yes: "Move", no: "Not today" };
  if (d.action === "STAY") return { yes: scheduleNext(d) ? "Open it" : "Keep", no: "Not today" };
  if (d.action === "HOLD") return { yes: "Hold", no: "Don't" };
  return { yes: "Send", no: "Skip" };
}

// S: a call about a strip schedule names the next strip.
export function scheduleNext(d: Decision): { strip: number; of: number; opens: string } | undefined {
  const sc = (d.inputs as { schedule?: { status?: string; next?: { strip: number; of: number; opens: string } } } | undefined)?.schedule;
  return sc?.status === "active" ? sc.next : undefined;
}

// ---- the next call ---------------------------------------------------------------------

// Minutes from `now` until the next daily call at `hhmm` ("06:00") in `tz`: 1..1440.
export function minutesToCall(hhmm: string, tz: string | undefined, now = Date.now()): number | undefined {
  const m = /^(\d{1,2}):(\d{2})/.exec(hhmm);
  if (!m) return undefined;
  const parts = new Intl.DateTimeFormat("en-GB", { timeZone: tz, hour: "2-digit", minute: "2-digit", hourCycle: "h23" }).formatToParts(new Date(now));
  const h = Number(parts.find((p) => p.type === "hour")?.value);
  const min = Number(parts.find((p) => p.type === "minute")?.value);
  const left = (Number(m[1]) * 60 + Number(m[2]) - (h * 60 + min) + 1440) % 1440;
  return left === 0 ? 1440 : left;
}

// "in 14 h", "in 35 m"
export const inWords = (min: number) => (min >= 90 ? `in ${Math.round(min / 60)} h` : `in ${min} m`);
