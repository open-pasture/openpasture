// What "/" matches locally: animals by tag, name or EID (collars without an animal by name) and
// paddocks by name. Exact beats prefix beats a word inside; every typed word must match.

import type { Animal, Collar, Paddock } from "../../api";

// 3 exact, 2 starts with, 1 contains, 0 no match (per field, case-insensitive; the best field wins).
export function score(q: string, fields: (string | undefined)[]): number {
  const words = q.trim().toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return 0;
  const hay = fields.filter((f): f is string => !!f).map((f) => f.toLowerCase());
  if (!hay.length) return 0;
  const whole = words.join(" ");
  if (hay.some((f) => f === whole)) return 3;
  if (hay.some((f) => f.startsWith(whole))) return 2;
  return words.every((w) => hay.some((f) => f.includes(w))) ? 1 : 0;
}

export interface AnimalHit { label: string; collarId?: string; tag?: string; score: number }

// Animals still on the farm, then collars that no animal wears. Best first, at most `limit`.
export function findAnimals(q: string, animals: Animal[], collars: Collar[], limit = 6): AnimalHit[] {
  const worn = new Set<string>();
  const hits: AnimalHit[] = [];
  for (const a of animals) {
    if (a.collar_id) worn.add(a.collar_id);
    if (a.removed_at) continue;
    const collarId = a.collar_id ?? collars.find((c) => c.animal_id === a.id)?.id;
    const s = score(q, [a.tag, a.name, a.eid]);
    if (s) hits.push({ label: a.name ? `${a.tag}  ${a.name}` : a.tag, collarId, tag: a.tag, score: s });
  }
  for (const c of collars) {
    if (worn.has(c.id) || c.animal_id) continue;
    const s = score(q, [c.name]);
    if (s) hits.push({ label: c.name, collarId: c.id, score: s });
  }
  return rank(hits, (h) => h.label).slice(0, limit);
}

export interface PaddockHit { label: string; paddock: Paddock; score: number }

export function findPaddocks(q: string, paddocks: Paddock[], limit = 4): PaddockHit[] {
  const hits = paddocks.flatMap((p) => {
    const s = score(q, [p.name]);
    return s ? [{ label: p.name, paddock: p, score: s }] : [];
  });
  return rank(hits, (h) => h.label).slice(0, limit);
}

// Best score first, then natural order ("P2" before "P10").
function rank<T extends { score: number }>(hits: T[], label: (h: T) => string): T[] {
  return hits.sort((a, b) => b.score - a.score || label(a).localeCompare(label(b), undefined, { numeric: true }));
}
