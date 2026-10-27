import { describe, expect, test } from "bun:test";
import type { JudgedStep, Rollout, TestRun, Unit, WireEntry } from "../../api/fleet";
import { STATE_WORDS, addPage, addWire, duration, fenceLine, grouped, nextStep, pct, qualityLine, readClaim, runNote, shownLine, stageLine, stepLine, wireLine } from "./model";

describe("fleet model", () => {
  test("claim codes print in fours and claims read as a card or a code", () => {
    expect(grouped("7q4k9m2xh8d3tv5w")).toBe("7Q4K-9M2X-H8D3-TV5W");
    expect(grouped("7Q4K-9M2X H8D3")).toBe("7Q4K-9M2X-H8D3");
    expect(readClaim(" OC2:OCN-2611-00123:abc:7Q4K9M2XH8D3TV5W ")).toEqual({ card: "OC2:OCN-2611-00123:abc:7Q4K9M2XH8D3TV5W" });
    expect(readClaim("7q4k-9m2x")).toEqual({ claim_code: "7q4k-9m2x" });
    expect(readClaim("  ")).toEqual({});
  });

  test("a judged step reads as value, limits and why", () => {
    const s: JudgedStep = { name: "rail_3v3", status: "pass", value: 2.1, unit: "V", verdict: "fail", why: "2.1 V below 3.2 V", kind: "measure", limit_lo: 3.2, limit_hi: 3.4, plan_unit: "V", required: true };
    expect(stepLine(s)).toBe("2.1 V  (3.2–3.4 V)  2.1 V below 3.2 V");
    expect(stepLine({ name: "imu", verdict: "missing", why: "Not in the run", required: true })).toBe("Not in the run");
    expect(pct(0.9876)).toBe("98.8 %");
  });

  test("a rollout says its stage", () => {
    const r = { wave: 1, waves: [{ kind: "collar", after_h: 0 }, { kind: "herd", after_h: 24 }, { kind: "all", after_h: 72 }] } as Rollout;
    expect(stageLine(r)).toBe("stage 2 of 3: the first herd");
  });

  test("link quality, fence distance and durations", () => {
    expect(qualityLine("good", -95, 6.5)).toBe("good  -95 dBm, SNR 6.5 dB");
    expect(qualityLine("unknown")).toBe("unknown");
    const len = (m: number) => `${Math.round(m)} m`;
    expect(fenceLine(12.2, len)).toBe("inside 12 m");
    expect(fenceLine(-3, len)).toBe("outside 3 m");
    expect(fenceLine(undefined, len)).toBeUndefined();
    expect(duration(38)).toBe("38 s");
    expect(duration(600)).toBe("10 min");
    expect(duration(4 * 3600 + 720)).toBe("4h 12m");
    expect(duration(86400 * 3)).toBe("3 d");
  });

  test("wire lines and the live window", () => {
    const e: WireEntry = { at: "2026-10-26T12:03:05Z", dir: "out", route: "report", status: 200, bytes: 41, body: "{}", note: "config v3", ms: 4 };
    expect(wireLine(e)).toBe("12:03:05  out  report    200  41 B  4 ms  config v3");
    let list: WireEntry[] = [];
    for (let i = 0; i < 70; i++) list = addWire(list, { ...e, bytes: i });
    expect(list.length).toBe(64);
    expect(list[0].bytes).toBe(6);
    expect(list[63].bytes).toBe(69);
  });

  // Review P2-04: half of a 1,000-unit batch showed with no sign of the rest.
  test("a long list says how much of it shows and pages without repeats", () => {
    const u = (serial: string) => ({ serial }) as Unit;
    expect(shownLine(200, 1000)).toBe("200 of 1,000 units");
    expect(shownLine(1000, 1000)).toBe("1,000 units");
    expect(shownLine(1, 1)).toBe("1 unit");
    expect(shownLine(0, 0)).toBe("no units");
    expect(addPage([u("A"), u("B")], [u("B"), u("C")]).map((x) => x.serial)).toEqual(["A", "B", "C"]);
  });

  test("every unit state says what happens next", () => {
    const base = { generation: 0, claim_checked: true } as Unit;
    const states = ["built", "tested", "provisioned", "claimed", "in_service", "rma", "retired"] as const;
    const lines = states.map((state) => nextStep({ ...base, state }));
    expect(new Set(lines).size).toBe(states.length);
    expect(nextStep({ ...base, state: "rma" })).toContain("fresh passing test");
    expect(nextStep({ ...base, state: "claimed", claim_checked: false })).toContain("checks the code");
    expect(STATE_WORDS.claimed).toBe("claim pending");
    const run = { result: "pass", counted: false, generation: 1 } as TestRun;
    expect(runNote(run, 1)).toContain("not counted");
    expect(runNote({ ...run, counted: true }, 2)).toBe("before the unit came back");
    expect(runNote({ ...run, counted: true }, 1)).toBeUndefined();
  });
});
