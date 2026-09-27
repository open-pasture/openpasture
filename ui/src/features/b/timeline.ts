// The words of the Decisions timeline, from a decision record: the call, who made it, who
// answered and how, what came of it. Times read in the farm's time zone.

import type { Actor, Decision } from "../../api";

type Named = (paddockId: string | undefined) => string | undefined;

// "Move to P2.", "Stay in P1.", the question a NEEDS_INFO asked.
export function sentence(d: Decision, name: Named): string {
  const inputs = (d.inputs ?? {}) as { from_paddock_id?: string };
  if (d.action === "MOVE") return `Move to ${name(d.to_paddock_id) ?? "a new boundary"}.`;
  if (d.action === "STAY") return `Stay in ${name(inputs.from_paddock_id) ?? "place"}.`;
  if (d.action === "NEEDS_INFO") return d.need?.trim() || "Needs more information.";
  // S: a strip schedule's hold.
  if (d.action === "HOLD") return "Hold today's strip.";
  if (d.status === "running") return "Deciding.";
  return d.error ? "No decision." : "Decision.";
}

// Who made the call: the brain and its model, the heuristic, or the farmer.
export function source(d: Decision): string {
  if (d.source === "farmer") return "farmer";
  if (d.source === "heuristic") return "heuristic";
  return [d.brain ?? "brain", d.model].filter(Boolean).join(" ");
}

// 24-hour HH:MM in `tz` (the browser's zone when absent).
export function hhmm(iso: string, tz?: string): string {
  return new Date(iso).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", hour12: false, timeZone: tz });
}

// "Sep 27" in `tz`.
export function day(iso: string, tz?: string): string {
  return new Date(iso).toLocaleDateString("en-US", { month: "short", day: "numeric", timeZone: tz });
}

const OWNER_VIAS = new Set<Actor["via"]>(["local", "app_token"]);

// Who answered and how: "by text, Cody 06:42", "by Ana 07:10", "by owner 06:55" (the app token
// or this machine, no person), "answered 06:55" (from before answers named who), "on timer 07:40",
// "on auto 06:02". Nothing while unanswered.
export function responder(d: Decision, tz?: string): string | undefined {
  const r = (d.inputs as { farmer_response?: { at?: string; by?: Actor } } | undefined)?.farmer_response;
  if (r) {
    const at = r.at ?? d.responded_at;
    const by = r.by;
    const who = by?.name ?? (by && OWNER_VIAS.has(by.via) ? "owner" : undefined);
    const how = by?.via === "text" ? "text" : undefined;
    const words = [how, who].filter(Boolean).join(", ");
    return [words ? `by ${words}` : "answered", at ? hhmm(at, tz) : undefined].filter(Boolean).join(" ");
  }
  // Sent without an answer: the herd's timer, or auto (due the moment it was made).
  if (d.status === "applied" && d.apply_at && d.source !== "farmer") {
    const auto = Date.parse(d.apply_at) - Date.parse(d.created_at) < 60_000;
    return `on ${auto ? "auto" : "timer"} ${hhmm(d.apply_at, tz)}`;
  }
  return undefined;
}

// What came of an applied move, a day on: "held, 3 cues", "not held, 14 cues", "1 cue".
export function outcome(d: Decision): string | undefined {
  const o = d.outcome as { herd_held_boundary?: boolean | null; cue_count?: number | null } | null | undefined;
  if (!o) return undefined;
  const held = o.herd_held_boundary === true ? "held" : o.herd_held_boundary === false ? "not held" : undefined;
  const n = o.cue_count;
  const cues = typeof n === "number" ? `${n} cue${n === 1 ? "" : "s"}` : undefined;
  const words = [held, cues].filter(Boolean).join(", ");
  return words || undefined;
}

// The one line under the call: source, responder, outcome, joined by "  ".
export function detail(d: Decision, tz?: string): string {
  return [source(d), responder(d, tz), outcome(d)].filter(Boolean).join("  ");
}

// Decisions made since `from` (the Data range's start), newest first as the API sends them. The
// range's end is when it was picked, so decisions made after that still show.
export function since(list: Decision[], from: string): Decision[] {
  const a = Date.parse(from);
  return list.filter((d) => Date.parse(d.created_at) >= a);
}
