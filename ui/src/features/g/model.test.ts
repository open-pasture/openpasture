import { describe, expect, test } from "bun:test";
import type { Coverage, FleetRow } from "../../api/g";
import type { HerdRow } from "../../registry";
import { fmt } from "../../units";
import { byDaysLeft, byFitDue, cellText, collarIds, daysText, dueText, squares, tone, trendText } from "./model";

describe("coverage squares", () => {
  const c: Coverage = {
    metric: "accuracy", cell_m: 10, unit: "m", size: [0.0002, 0.0001],
    cells: [[-93.62, 42.03, 2.5, 720], [-93.6198, 42.03, 9.1, 12]],
  };

  test("each cell is a square of its size around its centre, edge to edge with the next", () => {
    const s = squares(c);
    expect(s.features.length).toBe(2);
    const [a, b] = s.features.map((f) => f.geometry.coordinates[0]);
    expect(a[0][0]).toBeCloseTo(-93.6201, 9);
    expect(a[2][0]).toBeCloseTo(-93.6199, 9);
    expect(a[0][1]).toBeCloseTo(42.02995, 9);
    expect(a[2][1]).toBeCloseTo(42.03005, 9);
    expect(a[4]).toEqual(a[0]);
    // The east edge of one is the west edge of the next.
    expect(b[0][0]).toBeCloseTo(a[1][0], 9);
    expect(s.features[0].properties).toEqual({ v: 2.5, n: 720, tone: "good" });
    expect(s.features[1].properties.tone).toBe("poor");
  });

  test("tones: lower accuracy is better, more fixes are better", () => {
    expect([2.9, 3, 7.9, 8].map((v) => tone("accuracy", v))).toEqual(["good", "fair", "fair", "poor"]);
    expect([1, 0.95, 0.94, 0.8, 0.79].map((v) => tone("fixes", v))).toEqual(["good", "good", "fair", "fair", "poor"]);
  });

  test("hover text in the farm's units", () => {
    expect(cellText("accuracy", 2.5, fmt("metric"))).toBe("3 m");
    expect(cellText("accuracy", 2.5, fmt("imperial"))).toBe("8 ft");
    expect(cellText("fixes", 0.857, fmt("imperial"))).toBe("86%");
  });
});

describe("fleet words", () => {
  const DAY = 86_400_000;
  const now = new Date(2026, 8, 27, 15, 0).getTime();

  test("a fit check due, late or today", () => {
    expect(dueText(new Date(2026, 9, 9, 8, 0).toISOString(), now)).toEqual({ text: "in 12 d", late: false });
    expect(dueText(new Date(2026, 8, 27, 23, 0).toISOString(), now)).toEqual({ text: "today", late: false });
    expect(dueText(new Date(2026, 8, 27, 1, 0).toISOString(), now)).toEqual({ text: "today", late: false });
    expect(dueText(new Date(now - 3 * DAY).toISOString(), now)).toEqual({ text: "3 d late", late: true });
  });

  test("trend and days left", () => {
    expect(trendText(-2.13)).toBe("-2.1%/d");
    expect(trendText(0.5)).toBe("+0.5%/d");
    expect(daysText(41.4)).toBe("41 d");
  });

  const row = (id: string, collar?: string): HerdRow => ({ id, collar: collar ? { id: collar, name: collar, herd_id: "h", state: "inside" } : undefined });

  test("a selection's collars, each once", () => {
    expect(collarIds([row("a", "c1"), row("b"), row("c", "c2"), row("d", "c1")])).toEqual(["c1", "c2"]);
  });

  test("sorts put rows without fleet data last", () => {
    const f = (id: string, days_left: number | undefined, due: string): FleetRow =>
      ({ collar_id: id, name: id, herd_id: "h", fit_due_at: due, parked: false, daily: [], days_left });
    const rows = { c1: f("c1", 40, "2026-10-09T00:00:00Z"), c2: f("c2", 3, "2026-09-20T00:00:00Z"), c3: f("c3", undefined, "2026-11-01T00:00:00Z") };
    const list = [row("x", "c3"), row("y"), row("z", "c1"), row("w", "c2")];
    expect([...list].sort(byDaysLeft(rows)).map((r) => r.id)).toEqual(["w", "z", "x", "y"]);
    expect([...list].sort(byFitDue(rows)).map((r) => r.id)).toEqual(["w", "z", "x", "y"]);
  });
});
