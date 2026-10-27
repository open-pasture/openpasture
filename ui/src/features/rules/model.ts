// Pure helpers for the rules page and the safety gate (tested in model.test.ts).

import type { Actor, Finding } from "../../api";
import { ApiError } from "../../api/http";

// "Cody", "the owner" for the app token or this machine's sign-in, "someone" otherwise.
export function who(a?: Actor): string {
  if (a?.name) return a.name;
  return a?.via === "app_token" || a?.via === "local" ? "the owner" : "someone";
}

// "Cody · Oct 26", or "changed by Ana · Oct 27" once someone changed it.
export function byLine(r: { created_by: Actor; created_at: string; updated_by?: Actor; updated_at: string }, tz?: string): string {
  const day = (iso: string) => new Date(iso).toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: tz });
  return r.updated_by ? `changed by ${who(r.updated_by)} · ${day(r.updated_at)}` : `${who(r.created_by)} · ${day(r.created_at)}`;
}

// What the safety gate refused, from its 409: the hard findings, the codes a person may
// confirm, and those nobody may send.
export interface Held { error: string; findings: Finding[]; confirm: string[]; never: string[] }

export function heldBy(e: unknown): Held | undefined {
  if (!(e instanceof ApiError) || e.status !== 409 || !e.body || !Array.isArray(e.body.findings)) return undefined;
  const list = (v: unknown) => (Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : []);
  return { error: e.message, findings: e.body.findings as Finding[], confirm: list(e.body.confirm), never: list(e.body.never) };
}

// A person may send it anyway: something to confirm and nothing nobody may send.
export const confirmable = (h: Held) => h.confirm.length > 0 && h.never.length === 0;
