import { describe, expect, test } from "bun:test";
import type { Animal, Collar, Paddock } from "../../api";
import { findAnimals, findPaddocks, score } from "./find";

const collar = (id: string, name: string, animal_id?: string): Collar => ({ id, name, herd_id: "h", state: "inside", animal_id });
const animal = (id: string, tag: string, over: Partial<Animal> = {}): Animal => ({ id, tag, herd_id: "h", ...over });
const paddock = (id: string, name: string): Paddock =>
  ({ id, name, area_ha: 1, status: "resting", created_at: "", geometry: { type: "Polygon", coordinates: [] } });

describe("matching", () => {
  test("exact beats prefix beats a word inside", () => {
    expect(score("214", ["214"])).toBe(3);
    expect(score("21", ["214"])).toBe(2);
    expect(score("bess", ["031", "Bessie"])).toBe(2);
    expect(score("red cow", ["The red cow"])).toBe(1);
    expect(score("red dog", ["The red cow"])).toBe(0);
    expect(score("  ", ["214"])).toBe(0);
  });

  test("an animal by tag, name or EID, best first", () => {
    const animals = [
      animal("a1", "2140", { collar_id: "c1" }),
      animal("a2", "214", { name: "Bessie", collar_id: "c2" }),
      animal("a3", "031", { eid: "982000123456789", collar_id: "c3" }),
      animal("a4", "214b", { removed_at: "2026-09-01T00:00:00Z" }),
    ];
    const collars = [collar("c1", "C-01", "a1"), collar("c2", "C-02", "a2"), collar("c3", "C-03", "a3"), collar("c9", "spare 214")];
    const hits = findAnimals("214", animals, collars);
    expect(hits.map((h) => h.label)).toEqual(["214  Bessie", "2140", "spare 214"]);
    expect(hits[0].collarId).toBe("c2");
    expect(hits[0].tag).toBe("214");
    expect(findAnimals("98200012", animals, collars).map((h) => h.tag)).toEqual(["031"]);
    expect(findAnimals("bessie", animals, collars).map((h) => h.collarId)).toEqual(["c2"]);
    // A collar an animal wears is found through the animal, not twice.
    expect(findAnimals("C-02", animals, collars)).toEqual([]);
  });

  test("paddocks in natural order", () => {
    const ps = [paddock("1", "P10"), paddock("2", "P2"), paddock("3", "North 40")];
    expect(findPaddocks("p", ps).map((h) => h.label)).toEqual(["P2", "P10"]);
    expect(findPaddocks("north", ps).map((h) => h.label)).toEqual(["North 40"]);
  });
});
