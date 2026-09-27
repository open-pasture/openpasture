// Applying the live feed's batches to the store's lists, by id (pure, so it is tested
// without a browser). One positions or ack_batch message is one pass, never a scan per item.

import type { Ack, Animal, Collar, PositionItem } from "../api";
import type { AckItem } from "../api/p";

// Row of each collar in the list.
export function indexById(collars: readonly Collar[]): Map<string, number> {
  const m = new Map<string, number>();
  collars.forEach((c, i) => m.set(c.id, i));
  return m;
}

// The collars with each item's fix, state, battery and last contact. A fix older than the one
// the collar already has doesn't move it back. Returns the same array when nothing changed.
export function applyPositions(collars: Collar[], index: ReadonlyMap<string, number>, items: readonly PositionItem[]): Collar[] {
  let out: Collar[] | undefined;
  for (const it of items) {
    const i = index.get(it.collar_id);
    if (i === undefined) continue;
    const c = (out ?? collars)[i];
    const newer = !c.last_fix || Date.parse(it.fix.at) >= Date.parse(c.last_fix.at);
    const next: Collar = {
      ...c,
      ...(newer ? { last_fix: it.fix, state: it.state } : {}),
      ...(it.battery !== undefined ? { battery: it.battery } : {}),
      ...(it.last_seen !== undefined ? { last_seen: it.last_seen } : {}),
      ...(it.animal_id !== undefined ? { animal_id: it.animal_id } : {}),
    };
    out ??= collars.slice();
    out[i] = next;
  }
  return out ?? collars;
}

// A herd's acks with each item replacing that collar's ack.
export function applyAcks(acks: readonly Ack[], items: readonly AckItem[], at: string): Ack[] {
  const by = new Map(items.map((a) => [a.collar_id, a] as const));
  const kept = acks.filter((a) => !by.has(a.collar_id));
  for (const a of by.values()) kept.push({ collar_id: a.collar_id, version: a.version, status: a.status, reason: a.reason, at });
  return kept;
}

// What a collar is called: its animal's tag, else its own name. Built once per list.
export function labels(collars: readonly Collar[], animals: readonly Animal[]): Map<string, string> {
  const byAnimal = new Map<string, Animal>();
  const byCollar = new Map<string, Animal>();
  for (const a of animals) {
    byAnimal.set(a.id, a);
    if (a.collar_id) byCollar.set(a.collar_id, a);
  }
  const out = new Map<string, string>();
  for (const c of collars) out.set(c.id, (c.animal_id ? byAnimal.get(c.animal_id) : undefined)?.tag ?? byCollar.get(c.id)?.tag ?? c.name);
  return out;
}

// Collars the map never draws: parked, or on a removed animal.
export function undrawn(collars: readonly Collar[], animals: readonly Animal[]): Set<string> {
  const removed = new Set<string>();
  for (const a of animals) if (a.removed_at) {
    removed.add(a.id);
    if (a.collar_id) removed.add(a.collar_id);
  }
  const out = new Set<string>();
  for (const c of collars) if (c.parked_at || removed.has(c.id) || (c.animal_id && removed.has(c.animal_id))) out.add(c.id);
  return out;
}

// Collars the map draws: with a fix, not parked, not on a removed animal.
export function drawable(collars: readonly Collar[], animals: readonly Animal[]): Collar[] {
  const hide = undrawn(collars, animals);
  return collars.filter((c) => c.last_fix && !hide.has(c.id));
}

export const LOW_BATTERY = 0.2;

// One line for a big herd instead of a row per collar: how many, and the ones that need a look.
export interface CollarSummary { total: number; outside: string[]; warning: string[]; low: string[] }
export function summarize(collars: readonly Collar[]): CollarSummary {
  const s: CollarSummary = { total: collars.length, outside: [], warning: [], low: [] };
  for (const c of collars) {
    if (c.state === "outside") s.outside.push(c.id);
    else if (c.state === "warning") s.warning.push(c.id);
    if (c.battery !== undefined && c.battery < LOW_BATTERY) s.low.push(c.id);
  }
  return s;
}
