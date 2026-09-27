import { describe, expect, test } from "bun:test";
import type { Behaviour } from "../../api";
import { walkedPerDay } from "./walk";

// A row as /api/analytics/behaviour gives it: `days` is every UTC date the range touches.
const row = (distance_km: number, hours: { paddocks?: Record<string, number>; outside?: number }, days: number): Behaviour => ({
  animal_id: "a1", collar_id: "c1", tag: "101", name: null, fixes: 100, distance_km,
  paddock_hours: hours.paddocks ?? {}, outside_hours: hours.outside ?? 0,
  days: Array.from({ length: days }, (_, i) => `2026-09-${String(20 + i).padStart(2, "0")}`), cues_per_day: Array(days).fill(0), cues: 0, learning: null,
});

describe("walked a day", () => {
  test("a collar with a day of fixes walked what it walked, whatever dates the range touches", () => {
    // The last 24 h cross two UTC dates; the last week eight.
    expect(walkedPerDay(row(0.552, { paddocks: { p1: 24 } }, 2))).toBeCloseTo(552);
    expect(walkedPerDay(row(0.552, { paddocks: { p1: 20 }, outside: 4 }, 8))).toBeCloseTo(552);
  });

  test("a new collar with hours of data isn't stretched to a day", () => {
    expect(walkedPerDay(row(0.3, { paddocks: { p1: 5 } }, 8))).toBeCloseTo(300);
  });

  test("a week of fixes averages over the days it has", () => {
    expect(walkedPerDay(row(7, { paddocks: { p1: 100, p2: 68 } }, 8))).toBeCloseTo(1000);
    expect(walkedPerDay(row(3.5, { paddocks: { p1: 84 } }, 8))).toBeCloseTo(1000);
  });

  test("no walk, no distance", () => {
    expect(walkedPerDay({ ...row(0, {}, 2), distance_km: undefined as unknown as number })).toBe(0);
  });
});
