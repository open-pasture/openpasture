import type { Alert, Animal, Collar } from "../api";

// The app's simulation area (see simulation/README.md): nothing here is farm data.
// The simulator (`simulation/collar-sim`) runs collars that aren't on any animal. The server
// marks them `simulated` when it links them; the app says so wherever they show, in this one
// plain word. The first-run story (Story.tsx) is the app's only other simulation, and says so too.
export const SIMULATED = "Simulated";

export const isSimulated = (c: Pick<Collar, "simulated"> | undefined): boolean => !!c?.simulated;

// Every collar on the farm is simulated (and there is at least one): a simulated farm.
export const allSimulated = (collars: readonly Pick<Collar, "simulated">[]): boolean =>
  collars.length > 0 && collars.every(isSimulated);

// Of these collars, how many are simulated.
export const simulatedCount = (collars: readonly Pick<Collar, "simulated">[]): number => collars.filter(isSimulated).length;

// Animals wearing a simulated collar.
export function simulatedAnimals(collars: readonly Collar[], animals: readonly Animal[]): Set<string> {
  const sim = new Set(collars.filter(isSimulated).map((c) => c.id));
  const out = new Set<string>();
  for (const c of collars) if (c.simulated && c.animal_id) out.add(c.animal_id);
  for (const a of animals) if (a.collar_id && sim.has(a.collar_id)) out.add(a.id);
  return out;
}

// An alert the server marked as about simulated collars only.
export const simulatedAlert = (a: Pick<Alert, "data">): boolean => (a.data as { simulated?: boolean } | undefined)?.simulated === true;

// "12 collars", "12 collars, 3 simulated".
export function collarCount(collars: readonly Pick<Collar, "simulated">[]): string {
  const n = simulatedCount(collars);
  const base = `${collars.length} collar${collars.length === 1 ? "" : "s"}`;
  return n > 0 && n < collars.length ? `${base}, ${n} simulated` : base;
}

// The word the map shows over a herd: "Simulated" when all its collars are, "3 simulated"
// when some are, nothing when none are. Parked collars aren't drawn, so they don't count.
export function mapWord(collars: readonly Pick<Collar, "herd_id" | "simulated" | "parked_at">[], herdId: string | undefined): string | undefined {
  return herdWord(collars.filter((c) => c.herd_id === herdId && !c.parked_at));
}

// The same word for one herd's collars.
export function herdWord(collars: readonly Pick<Collar, "simulated">[]): string | undefined {
  const n = simulatedCount(collars);
  if (!n) return undefined;
  return n === collars.length ? SIMULATED : `${n} simulated`;
}
