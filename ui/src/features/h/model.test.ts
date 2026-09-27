import { describe, expect, test } from "bun:test";
import type { HerdWelfare, Learning, WelfareDay } from "../../api/h";
import { endingWord, outcomesText, ringWord, secs, sparks, statusRank, statusText, tickFeatures, tickText, toneText, trainingLine } from "./model";

const none = { turned_back: 0, crossed: 0, rest: 0, boundary_changed: 0 };

describe("welfare words", () => {
  test("trained or learning, since when, and nothing without episodes", () => {
    const day = (iso: string) => iso.slice(5, 10);
    const trained: Learning = { status: "trained", since: "2026-09-24T12:00:00Z", streak: 6, outcomes: { ...none, turned_back: 6 } };
    expect(statusText(trained, day)).toBe("trained since 09-24");
    expect(statusText({ ...trained, status: "learning", since: "2026-09-20T08:00:00Z" }, day)).toBe("learning since 09-20");
    expect(statusText({ streak: 0, outcomes: none })).toBeUndefined();
    expect(statusText(undefined)).toBeUndefined();
  });

  test("episodes by how they ended, leaving out what didn't happen", () => {
    expect(outcomesText({ turned_back: 8, crossed: 1, rest: 2, boundary_changed: 0 })).toBe("8 turned back  1 crossed  2 rest");
    expect(outcomesText(none)).toBe("");
    expect(endingWord("boundary_changed")).toBe("boundary changed");
    expect(endingWord(undefined)).toBe("");
  });

  test("rings, tone and seconds", () => {
    expect([ringWord(0), ringWord(2), ringWord(undefined)]).toEqual(["edge", "hole 2", ""]);
    expect(toneText(300)).toBe("0.3 s");
    expect(secs(12.64)).toBe("12.6 s");
    expect(secs(0)).toBe("0.0 s");
  });

  test("the herd panel's training line only while training is on", () => {
    const h: HerdWelfare = { head: 250, trained: 31, learning: 190, animals: [], training: { enabled: true, warn_m: 10, trained_after: 5 } };
    expect(trainingLine(h)).toBe("training  31/250 trained");
    expect(trainingLine({ ...h, training: { ...h.training!, enabled: false } })).toBeUndefined();
    expect(trainingLine(undefined)).toBeUndefined();
  });

  test("sparklines of cues and tone a day, oldest first", () => {
    const d = (warn: number, outside: number, tone_s: number): WelfareDay => ({ date: "2026-09-20", warn, outside, tone_s, episodes: 0 });
    expect(sparks([d(3, 1, 1.2), d(0, 0, 0), d(5, 0, 1.5)])).toEqual({ cues: [4, 0, 5], tone: [1.2, 0, 1.5] });
  });

  test("the trained column sorts trained, learning, then nothing", () => {
    const order = ["learning", undefined, "trained"].sort((a, b) => statusRank(a as never) - statusRank(b as never));
    expect(order).toEqual(["trained", "learning", undefined]);
  });

  test("map ticks: red where an animal crossed, stronger with more cues", () => {
    const fc = tickFeatures([[-93.62, 42.03, 1, 0], [-93.61, 42.03, 19, 1], [-93.6, 42.03, 400, 0]]);
    expect(fc.features.map((f) => f.properties.kind)).toEqual(["warn", "outside", "warn"]);
    const w = fc.features.map((f) => f.properties.weight);
    expect(w[0]).toBeGreaterThan(0.3);
    expect(w[0]).toBeLessThan(w[1]);
    expect(w[1]).toBeCloseTo(1, 5);
    expect(w[2]).toBe(1);
    expect(fc.features[0].geometry.coordinates).toEqual([-93.62, 42.03]);
    expect(tickText(12, 1)).toBe("12 warn  1 outside");
    expect(tickText(0, 3)).toBe("3 outside");
  });
});
