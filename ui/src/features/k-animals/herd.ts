// The Herd view's arithmetic, kept pure for tests: which rows show, how they read and
// sort, which collars are spare, what a keys file and a sheet of cards hold.

import type { Animal, Collar, RemovedReason, Sex } from "../../api";
import type { ImportField, ImportPreview, LinkedCollar, Mapping } from "../../api/k-animals";
import type { HerdRow } from "../../registry";
import { compareValues } from "../../ui/rows";

export type Shown = "active" | "removed";

// Active: the herd's animals on the farm, each with its collar, then its collars on no
// animal. Removed: animals that left, newest first.
export function herdRows(animals: Animal[], collars: Collar[], herdId: string | undefined, shown: Shown): HerdRow[] {
  const inHerd = animals.filter((a) => a.herd_id === herdId);
  if (shown === "removed") {
    return inHerd.filter((a) => a.removed_at).sort((a, b) => (b.removed_at ?? "").localeCompare(a.removed_at ?? "")).map((a) => ({ id: a.id, animal: a }));
  }
  const byId = new Map(collars.map((c) => [c.id, c]));
  const worn = new Set<string>();
  const rows: HerdRow[] = [];
  for (const a of inHerd) {
    if (a.removed_at) continue;
    const c = a.collar_id ? byId.get(a.collar_id) : undefined;
    if (c) worn.add(c.id);
    rows.push({ id: a.id, animal: a, collar: c });
  }
  for (const c of collars) if (c.herd_id === herdId && !worn.has(c.id) && !c.animal_id) rows.push({ id: c.id, collar: c });
  return rows;
}

// What the filter searches: tag, name, EID, breed and collar.
export const rowText = (r: HerdRow) => [r.animal?.tag, r.animal?.name, r.animal?.eid, r.animal?.breed, r.collar?.name].filter(Boolean).join(" ");

// How a row is named: its tag, else its collar.
export const rowLabel = (r: HerdRow) => r.animal?.tag ?? r.collar?.name ?? r.id;

export const by = (f: (r: HerdRow) => unknown) => (a: HerdRow, b: HerdRow) => compareValues(f(a), f(b));

const SEX: Record<Sex, string> = { female: "female", male: "male", castrated: "castrated" };
export const sexWord = (s?: Sex) => (s ? SEX[s] : "");
export const REASONS: { value: RemovedReason; label: string }[] = [
  { value: "sold", label: "Sold" }, { value: "died", label: "Died" }, { value: "culled", label: "Culled" }, { value: "moved_off", label: "Moved off" },
];
export const reasonWord = (r?: RemovedReason) => REASONS.find((x) => x.value === r)?.label.toLowerCase() ?? "";

export const battery = (b?: number) => (b === undefined ? "" : `${Math.round(b * 100)}%`);

// "Sep 20, 2026" for a date or a time, read in UTC for plain dates so they don't shift.
export function day(iso?: string) {
  if (!iso) return "";
  const d = new Date(iso.length === 10 ? iso + "T12:00:00Z" : iso);
  return d.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric", timeZone: iso.length === 10 ? "UTC" : undefined });
}

// Collars of the herd on no animal: what a swap or a link can use.
export function spareCollars(collars: Collar[], animals: Animal[], herdId: string) {
  const worn = new Set(animals.filter((a) => a.collar_id && !a.removed_at).map((a) => a.collar_id));
  return collars.filter((c) => c.herd_id === herdId && !c.animal_id && !worn.has(c.id)).sort((a, b) => compareValues(a.name, b.name));
}

// #/herd?select=<collar ids>: a selection handed over from elsewhere (the map's lasso). The
// ids, the herd most of them are in (the table shows one herd), and that herd's rows they pick.
export function selectionOf(rest: string): string[] {
  if (!rest.startsWith("?")) return [];
  const v = new URLSearchParams(rest.slice(1)).get("select") ?? "";
  return [...new Set(v.split(",").map((s) => s.trim()).filter(Boolean))];
}
export const selectUrl = (collarIds: string[]) => `/herd?select=${collarIds.map(encodeURIComponent).join(",")}`;

export function herdOfMost(collars: Collar[], ids: string[]): string | undefined {
  const want = new Set(ids);
  const n = new Map<string, number>();
  for (const c of collars) if (want.has(c.id)) n.set(c.herd_id, (n.get(c.herd_id) ?? 0) + 1);
  return [...n].sort((a, b) => b[1] - a[1])[0]?.[0];
}

export function pickRows(rows: HerdRow[], collarIds: string[]): Set<string> {
  const want = new Set(collarIds);
  return new Set(rows.filter((r) => r.collar && want.has(r.collar.id)).map((r) => r.id));
}

// The picked rows first (as the lasso caught them in the table's order), then the rest.
export const pickedFirst = (rows: HerdRow[], picked: ReadonlySet<string>) => [...rows.filter((r) => picked.has(r.id)), ...rows.filter((r) => !picked.has(r.id))];

// #/herd/<tag>: the animal with that tag (this herd first, animals on the farm before
// removed ones), else a collar by id or name.
export function resolve(rest: string, animals: Animal[], collars: Collar[], herdId?: string): { animal?: Animal; collar?: Collar } {
  const key = decodeURIComponent(rest.split(/[/?]/)[0]);
  if (!key) return {};
  const tagged = animals.filter((a) => a.tag === key);
  const rank = (a: Animal) => (a.removed_at ? 2 : 0) + (a.herd_id === herdId ? 0 : 1);
  const animal = tagged.sort((a, b) => rank(a) - rank(b))[0] ?? animals.find((a) => a.id === key);
  if (animal) return { animal, collar: collars.find((c) => c.id === animal.collar_id) };
  const collar = collars.find((c) => c.id === key) ?? collars.find((c) => c.name === key);
  if (!collar) return {};
  return { collar, animal: animals.find((a) => a.id === collar.animal_id) };
}

// What to put after #/herd/ so resolve() opens exactly this animal whichever herd is selected:
// its tag while no other animal has it (tags repeat across herds, and a removed animal's can
// come back), else its id. A collar on no animal goes by its id.
export function pageKeys(animals: Animal[]): (animal?: Animal, collar?: Collar) => string {
  const n = new Map<string, number>();
  for (const a of animals) n.set(a.tag, (n.get(a.tag) ?? 0) + 1);
  return (animal, collar) => (animal ? (n.get(animal.tag) === 1 ? animal.tag : animal.id) : (collar?.id ?? ""));
}
export const pageHash = (key: string) => `/herd/${encodeURIComponent(key)}`;

export const FIELDS: { key: ImportField; label: string; required?: boolean }[] = [
  { key: "tag", label: "tag", required: true }, { key: "eid", label: "EID" }, { key: "name", label: "name" }, { key: "breed", label: "breed" },
  { key: "sex", label: "sex" }, { key: "born", label: "born" }, { key: "collar", label: "collar" }, { key: "notes", label: "notes" },
];

// The preview's first rows as the mapped fields would read them.
export function mappedRows(p: ImportPreview, m: Mapping, n = 5): Partial<Record<ImportField, string>>[] {
  const idx = FIELDS.flatMap((f) => (m[f.key] !== undefined && p.columns.includes(m[f.key]!) ? [[f.key, p.columns.indexOf(m[f.key]!)] as const] : []));
  return p.rows.slice(0, n).map((r) => Object.fromEntries(idx.map(([k, i]) => [k, r[i] ?? ""])));
}

// Collar keys as a CSV, for setting collars up without cards. Shown once, like the keys.
export function keysCsv(collars: LinkedCollar[]) {
  const q = (s: string) => (/[",\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s);
  const lines = collars.map((c) => [c.tag ?? "", c.collar.name, c.collar.id, c.key, c.endpoint, c.public_key].map(q).join(","));
  return ["tag,collar,collar_id,key,endpoint,public_key", ...lines].join("\n") + "\n";
}

// Cards in sheets of 12 (3 across, 4 down).
export function sheets<T>(items: T[], per = 12): T[][] {
  const out: T[][] = [];
  for (let i = 0; i < items.length; i += per) out.push(items.slice(i, i + per));
  return out;
}

// Cards print only from an https public URL: the endpoint goes into every collar.
export const cardsPossible = (publicUrl?: string) => !!publicUrl && publicUrl.startsWith("https://");

// Run f over items, a few at a time. The first error stops every worker and is thrown.
export async function each<T>(items: T[], f: (t: T) => Promise<unknown>, width = 6) {
  let i = 0;
  let failed = false;
  const worker = async () => {
    while (!failed && i < items.length) {
      try {
        await f(items[i++]);
      } catch (e) {
        failed = true;
        throw e;
      }
    }
  };
  await Promise.all(Array.from({ length: Math.min(width, items.length) }, worker));
}
