// Pure alert helpers: order, counts, ages, what an alert points at. No React, no map.

import type { Alert, LonLat, Severity } from "../../api";

export const RANK: Record<Severity, number> = { info: 0, warning: 1, critical: 2 };

// Most urgent first: critical before warning before info, unacked before acked, newest first.
export function byUrgency(a: Alert, b: Alert): number {
  return (
    RANK[b.severity] - RANK[a.severity] ||
    Number(a.status === "acked") - Number(b.status === "acked") ||
    b.opened_at.localeCompare(a.opened_at) ||
    b.id.localeCompare(a.id)
  );
}

// The unresolved list after a live `alert` event: replaced by id, dropped once resolved.
export function upsert(list: readonly Alert[], a: Alert): Alert[] {
  const rest = list.filter((x) => x.id !== a.id);
  return a.status === "resolved" ? rest : [...rest, a];
}

// What the top bar shows: unacked alerts, and whether any is critical.
export function openCount(list: readonly Alert[]): { n: number; critical: boolean } {
  const open = list.filter((a) => a.status === "open");
  return { n: open.length, critical: open.some((a) => a.severity === "critical") };
}

// A herd's rows in its panel. A waiting decision already shows as the panel's decision.
export function herdRows(list: readonly Alert[], herdId: string): Alert[] {
  return list.filter((a) => a.herd_id === herdId && a.status !== "resolved" && a.kind !== "decision_waiting").sort(byUrgency);
}

const str = (a: Alert, k: string): string | undefined => {
  const v = (a.data as Record<string, unknown> | null)?.[k];
  return typeof v === "string" ? v : undefined;
};

// Since when the thing is wrong (the rule's `since`), else when the alert opened. Epoch ms.
export function since(a: Alert): number {
  const s = str(a, "since");
  const t = s ? Date.parse(s) : NaN;
  return Number.isFinite(t) ? t : Date.parse(a.opened_at);
}

// "6m", "3h", "2d": the same steps as the texts.
export function ageText(sinceMs: number, now: number): string {
  const m = Math.max(1, Math.floor((now - sinceMs) / 60_000));
  if (m < 60) return `${m}m`;
  if (m < 48 * 60) return `${Math.floor(m / 60)}h`;
  return `${Math.floor(m / 1440)}d`;
}

// Collar ids an alert is about, in its targets' order.
export function collarsOf(a: Alert): string[] {
  return a.targets.filter(([k]) => k === "collar").map(([, id]) => id);
}

// Per collar: the fact a rollup's members carry (members line up with the collar targets).
export function memberFacts(a: Alert): Map<string, Record<string, unknown>> {
  const ids = collarsOf(a);
  const members = (a.data as { members?: Record<string, unknown>[] } | null)?.members;
  const out = new Map<string, Record<string, unknown>>();
  if (Array.isArray(members)) members.forEach((m, i) => ids[i] && out.set(ids[i], m));
  else if (ids.length === 1) out.set(ids[0], (a.data ?? {}) as Record<string, unknown>);
  return out;
}

// "Collar silent for {n} min" → ["Collar silent for ", " min"]; null without a number.
export function splitSentence(s: string): [string, string] | null {
  const i = s.indexOf("{n}");
  return i < 0 ? null : [s.slice(0, i), s.slice(i + 3)];
}

// A closed or handled alert in the history: who, or how it went.
export function statusText(a: Alert): string {
  if (a.status === "open") return "open";
  if (a.status === "acked") return a.acked_by?.name ? `acked by ${a.acked_by.name}` : "acked";
  if (a.rolled_into) return "rolled up";
  if (!a.resolved_by) return "cleared";
  return a.resolved_by.name ? `resolved by ${a.resolved_by.name}` : "resolved";
}

// How long an alert was open: "25m", "3h".
export function openFor(a: Alert, now: number): string {
  const end = a.resolved_at ? Date.parse(a.resolved_at) : now;
  return ageText(Date.parse(a.opened_at), end);
}

// A ring of `radiusM` metres around a point, as a closed polygon ring (equirectangular: fine
// for accuracy circles of a few metres to a few hundred).
export function circle(center: LonLat, radiusM: number, n = 48): LonLat[] {
  const [lon, lat] = center;
  const dLat = radiusM / 111_320;
  const dLon = radiusM / (111_320 * Math.cos((lat * Math.PI) / 180));
  const ring: LonLat[] = [];
  for (let i = 0; i < n; i++) {
    const t = (i / n) * 2 * Math.PI;
    ring.push([lon + dLon * Math.cos(t), lat + dLat * Math.sin(t)]);
  }
  ring.push(ring[0]);
  return ring;
}

// The slow ring on an unacked critical alert: radius (px) and opacity at time t (ms).
export function beat(t: number, period = 2400): { radius: number; opacity: number } {
  const p = (t % period) / period;
  return { radius: 7 + 17 * p, opacity: 0.7 * (1 - p) };
}
