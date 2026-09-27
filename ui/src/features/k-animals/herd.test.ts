import { describe, expect, test } from "bun:test";
import type { Animal, Collar } from "../../api";
import type { ImportPreview, LinkedCollar } from "../../api/k-animals";
import { cardsPossible, each, herdRows, keysCsv, mappedRows, resolve, rowLabel, rowText, sheets, spareCollars } from "./herd";

const animal = (id: string, tag: string, more: Partial<Animal> = {}): Animal => ({ id, tag, herd_id: "h1", ...more });
const collar = (id: string, name: string, more: Partial<Collar> = {}): Collar => ({ id, name, herd_id: "h1", state: "inside", ...more });

describe("herd rows", () => {
  const animals = [
    animal("a1", "214", { collar_id: "c1", name: "Daisy", eid: "982000123456789" }),
    animal("a2", "215"),
    animal("a3", "31", { herd_id: "h2" }),
    animal("a4", "216", { removed_at: "2026-09-20T15:00:00.000Z", removed_reason: "sold" }),
    animal("a5", "217", { removed_at: "2026-09-21T15:00:00.000Z", removed_reason: "died" }),
  ];
  const collars = [collar("c1", "C-0001", { animal_id: "a1" }), collar("c2", "C-0002"), collar("c3", "H-1", { herd_id: "h2" })];

  test("active rows are the herd's animals with their collars, then its spare collars", () => {
    const rows = herdRows(animals, collars, "h1", "active");
    expect(rows.map((r) => r.id)).toEqual(["a1", "a2", "c2"]);
    expect(rows[0].collar?.id).toBe("c1");
    expect(rows[2].animal).toBeUndefined();
    expect(rows.map(rowLabel)).toEqual(["214", "215", "C-0002"]);
  });

  test("removed rows are the animals that left, newest first", () => {
    expect(herdRows(animals, collars, "h1", "removed").map((r) => r.id)).toEqual(["a5", "a4"]);
  });

  test("the filter reads tag, name, EID, breed and collar", () => {
    const [first] = herdRows(animals, collars, "h1", "active");
    expect(rowText(first)).toBe("214 Daisy 982000123456789 C-0001");
  });

  test("250 animals make 250 rows without quadratic work", () => {
    const many = Array.from({ length: 250 }, (_, i) => animal(`a${i}`, String(1000 + i), { collar_id: `c${i}` }));
    const worn = Array.from({ length: 250 }, (_, i) => collar(`c${i}`, `C-${i}`, { animal_id: `a${i}` }));
    const t = performance.now();
    const rows = herdRows(many, worn, "h1", "active");
    expect(rows.length).toBe(250);
    expect(rows.every((r) => r.collar)).toBe(true);
    expect(performance.now() - t).toBeLessThan(50);
  });
});

describe("spare collars and pages", () => {
  test("spares are the herd's collars on no animal", () => {
    const animals = [animal("a1", "214", { collar_id: "c1" })];
    const collars = [collar("c1", "C-1", { animal_id: "a1" }), collar("c3", "C-10"), collar("c2", "C-2"), collar("c9", "X", { herd_id: "h2" })];
    expect(spareCollars(collars, animals, "h1").map((c) => c.name)).toEqual(["C-2", "C-10"]);
  });

  test("#/herd/<tag> finds the animal, this herd and the farm first, else a collar", () => {
    const animals = [
      animal("old", "214", { removed_at: "2026-01-01T00:00:00Z" }),
      animal("other", "214", { herd_id: "h2" }),
      animal("here", "214", { collar_id: "c1" }),
      animal("a2", "B 7"),
    ];
    const collars = [collar("c1", "C-1", { animal_id: "here" }), collar("c2", "Spare")];
    expect(resolve("214", animals, collars, "h1").animal?.id).toBe("here");
    expect(resolve("214", animals, collars, "h1").collar?.id).toBe("c1");
    expect(resolve("214", animals, collars, "h2").animal?.id).toBe("other");
    expect(resolve("B%207", animals, collars, "h1").animal?.id).toBe("a2");
    expect(resolve("c2", animals, collars, "h1")).toEqual({ collar: collars[1], animal: undefined });
    expect(resolve("Spare", animals, collars, "h1").collar?.id).toBe("c2");
    expect(resolve("nope", animals, collars, "h1")).toEqual({});
  });
});

describe("import preview", () => {
  test("mapped rows read the chosen columns", () => {
    const p: ImportPreview = {
      import_id: "imp_1", columns: ["Visual ID", "EID", "Weight"], mapping: { tag: "Visual ID" }, total: 2, errors: [],
      rows: [["214", "982000123456789", "1210"], ["215", "", "1175"]],
    };
    expect(mappedRows(p, { tag: "Visual ID", eid: "EID" })).toEqual([{ tag: "214", eid: "982000123456789" }, { tag: "215", eid: "" }]);
    expect(mappedRows(p, { tag: "Gone" })).toEqual([{}, {}]);
  });
});

describe("keys and cards", () => {
  const linked = (i: number, tag?: string): LinkedCollar => ({
    collar: collar(`col_${i}`, `C-${i}`), key: `k${i}`, endpoint: "https://farm.example.com/collar/v1", public_key: "PK=", tag,
  });

  test("the keys file has a header and one row per collar, quoted where needed", () => {
    const csv = keysCsv([linked(1, "214"), { ...linked(2), collar: collar("col_2", 'Spare, "blue"') }]);
    expect(csv).toBe(
      "tag,collar,collar_id,key,endpoint,public_key\n" +
      "214,C-1,col_1,k1,https://farm.example.com/collar/v1,PK=\n" +
      ',"Spare, ""blue""",col_2,k2,https://farm.example.com/collar/v1,PK=\n',
    );
  });

  test("250 cards make 21 sheets of up to 12", () => {
    const s = sheets(Array.from({ length: 250 }, (_, i) => i));
    expect(s.length).toBe(21);
    expect(s[0].length).toBe(12);
    expect(s[20].length).toBe(10);
  });

  test("cards need an https public URL", () => {
    expect(cardsPossible(undefined)).toBe(false);
    expect(cardsPossible("http://192.168.1.20:7878")).toBe(false);
    expect(cardsPossible("https://farm.example.com")).toBe(true);
  });

  test("each runs a few at a time and every item once", async () => {
    let running = 0, most = 0;
    const seen: number[] = [];
    await each(Array.from({ length: 40 }, (_, i) => i), async (i) => {
      running++;
      most = Math.max(most, running);
      await new Promise((r) => setTimeout(r, 1));
      seen.push(i);
      running--;
    }, 6);
    expect(most).toBe(6);
    expect(seen.sort((a, b) => a - b)).toEqual(Array.from({ length: 40 }, (_, i) => i));
  });

  test("each stops starting new work after the first error", async () => {
    const started: number[] = [];
    const run = each(Array.from({ length: 40 }, (_, i) => i), async (i) => {
      started.push(i);
      await new Promise((r) => setTimeout(r, 1));
      if (i === 3) throw new Error("409");
    }, 4);
    await expect(run).rejects.toThrow("409");
    await new Promise((r) => setTimeout(r, 20));
    expect(started.length).toBeLessThan(12);
  });
});
