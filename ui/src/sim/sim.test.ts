import { describe, expect, test } from "bun:test";
import { renderToStaticMarkup } from "react-dom/server";
import { createElement } from "react";
import type { Animal, Collar } from "../api";
import { allSimulated, collarCount, herdWord, isSimulated, mapWord, SIMULATED, simulatedAlert, simulatedAnimals } from ".";
import { Story } from "./Story";

const collar = (id: string, o: Partial<Collar> = {}): Collar => ({ id, name: id, herd_id: "h1", state: "inside", ...o });

describe("simulated collars", () => {
  test("the flag from the server is the only source", () => {
    expect(isSimulated(collar("a", { simulated: true }))).toBe(true);
    expect(isSimulated(collar("a"))).toBe(false);
    expect(isSimulated(undefined)).toBe(false);
  });

  test("a farm is simulated only when every collar is", () => {
    expect(allSimulated([])).toBe(false);
    expect(allSimulated([collar("a", { simulated: true }), collar("b", { simulated: true })])).toBe(true);
    expect(allSimulated([collar("a", { simulated: true }), collar("b")])).toBe(false);
  });

  test("counts say how many are simulated when some are", () => {
    expect(collarCount([collar("a"), collar("b")])).toBe("2 collars");
    expect(collarCount([collar("a", { simulated: true }), collar("b")])).toBe("2 collars, 1 simulated");
    expect(collarCount([collar("a", { simulated: true })])).toBe("1 collar");
    expect(herdWord([collar("a", { simulated: true }), collar("b", { simulated: true })])).toBe(SIMULATED);
    expect(herdWord([collar("a", { simulated: true }), collar("b")])).toBe("1 simulated");
    expect(herdWord([collar("a")])).toBeUndefined();
  });

  test("the map word is for the herd shown, parked collars left out", () => {
    const all = [collar("a", { simulated: true }), collar("b", { simulated: true, parked_at: "2026-10-26T00:00:00Z" }), collar("c", { herd_id: "h2" })];
    expect(mapWord(all, "h1")).toBe(SIMULATED);
    expect(mapWord(all, "h2")).toBeUndefined();
    expect(mapWord([...all, collar("d")], "h1")).toBe("1 simulated");
  });

  test("animals on simulated collars, by either link", () => {
    const animals: Animal[] = [
      { id: "an1", tag: "101", herd_id: "h1", collar_id: "a" },
      { id: "an2", tag: "102", herd_id: "h1", collar_id: "c" },
    ];
    const sims = simulatedAnimals([collar("a", { simulated: true }), collar("b", { simulated: true, animal_id: "an3" }), collar("c")], animals);
    expect([...sims].sort()).toEqual(["an1", "an3"]);
  });

  test("alerts the server marked", () => {
    expect(simulatedAlert({ data: { simulated: true, label: "101" } })).toBe(true);
    expect(simulatedAlert({ data: { label: "101" } })).toBe(false);
    expect(simulatedAlert({ data: null })).toBe(false);
  });
});

describe("the first-run story", () => {
  test("says it is a simulation, in a plain word a screen reader reads too", () => {
    const html = renderToStaticMarkup(createElement(Story, { active: true }));
    expect(html).toContain(`>${SIMULATED}<`);
    // The canvas is decoration; the word is not hidden.
    expect(html).toMatch(/<canvas aria-hidden="true"/);
    expect(html).not.toMatch(/<div[^>]*aria-hidden/);
  });

  test("writes nothing: no api, no store", async () => {
    const src = await Bun.file(new URL("./Story.tsx", import.meta.url)).text();
    expect(src).not.toMatch(/from "\.\.\/(api|store|registry)/);
    expect(src).not.toMatch(/\bfetch\(|localStorage|indexedDB|WebSocket/);
  });

  test("keeps the word while it fades out", () => {
    const html = renderToStaticMarkup(createElement(Story, { active: false }));
    expect(html).toContain("story off");
    expect(html).toContain(SIMULATED);
  });
});

// Astra 6, 03 P2-06: rows listing collars or animals showed no mark, and the first-run
// simulation lived among the product's views.
describe("simulation is marked where collars and animals are listed, and kept in one place", () => {
  const LISTINGS = [
    "features/k-animals/HerdView.tsx", // the herd table
    "features/k-animals/Sidebar.tsx", // the herd sidebar
    "features/k-animals/AnimalPage.tsx", // an animal's page
    "features/fleet/Network.tsx", // anchors, members, gaps
    "features/fleet/Console.tsx", // one collar's console
    "features/b/index.ts", // search hits
    "views/HerdPanel.tsx", // the herd panel's collars
    "shell/Desk.tsx", // vitals and alert asks
  ];
  for (const f of LISTINGS)
    test(f, async () => {
      const src = await Bun.file(new URL(`../${f}`, import.meta.url)).text();
      expect(src).toMatch(/\.simulated\b|simulatedAlert|herdWord|isSimulated/);
    });

  test("the first-run story is in the simulation area, not among the views", async () => {
    expect(await Bun.file(new URL("../views/Story.tsx", import.meta.url)).exists()).toBe(false);
    const first = await Bun.file(new URL("../views/FirstRun.tsx", import.meta.url)).text();
    expect(first).toContain('from "../sim/Story"');
  });
});
