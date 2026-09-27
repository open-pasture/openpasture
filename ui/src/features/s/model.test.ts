import { describe, expect, test } from "bun:test";
import type { LonLat, Polygon } from "../../api";
import type { CollarSlots } from "../../api/e-srv";
import type { Schedule, ScheduledMove } from "../../api/s";
import { offset, rect } from "../../geo";
import { backLine, boundaryAt, countdown, dayTicks, grazedAt, missingCollars, nextMove, nextOpen, queue, railSpan, rowTime, storedLine, stripAt } from "./model";

const tz = "America/Chicago";
const sw: LonLat = [-93.625, 42.03];
// Six 50 m strips advancing east.
const strips: Polygon[] = Array.from({ length: 6 }, (_, i) => rect(offset(sw, 50 * i, 0), 50, 200));
// 07:00 CDT on Mon 2026-09-28.
const T0 = Date.parse("2026-09-28T12:00:00Z");
const H = 3_600_000;
const iso = (ms: number) => new Date(ms).toISOString();

function mv(index: number, step: number, at: number, state: ScheduledMove["state"] = "staged", extra: Partial<ScheduledMove> = {}): ScheduledMove {
  return { schedule_id: "sch_1", index, step, at: iso(at), geometry: strips[index], state, boundary_version: 10 + index * 4 + step, ...extra };
}

// Strips 2..4 open daily at 07:00, each with a back fence at 11:00, 11:10, 11:20.
function plan(): ScheduledMove[] {
  const out: ScheduledMove[] = [];
  for (const [n, k] of [1, 2, 3].entries()) {
    out.push(mv(k, 0, T0 + n * 24 * H));
    for (const s of [1, 2, 3]) out.push(mv(k, s, T0 + n * 24 * H + 4 * H + (s - 1) * 600_000));
  }
  return out;
}

const schedule: Schedule = {
  id: "sch_1", herd_id: "herd_1", paddock_id: "pad_1", strips, next_index: 1, cadence: { every_days: 1, at: "07:00" },
  starts_at: iso(T0), back_fence: { enabled: true, lag_strips: 0, close_after_min: 240, close_steps: 3, close_every_min: 10 },
  status: "active", created_by: { via: "local" }, created_at: iso(T0 - H), updated_at: iso(T0 - H),
};

describe("the next move", () => {
  test("the next open and the next step", () => {
    const ms = plan();
    expect(nextOpen(ms)?.index).toBe(1);
    ms[0].state = "done";
    expect(nextOpen(ms)?.index).toBe(2);
    expect(nextMove(ms)?.step).toBe(1);
  });

  test("the stored line counts stored and applied collars", () => {
    const m = plan()[0];
    const slots = [{ version: m.boundary_version!, applied: 0, stored: 248, rejected: 0, collars: 250 }];
    expect(storedLine(m, slots, tz, T0 - 6 * H)).toBe("07:00  248/250 stored");
    expect(storedLine(m, slots, tz, T0 - 20 * H)).toBe("Mon 07:00  248/250 stored");
    expect(storedLine({ ...m, boundary_version: undefined }, slots, tz, T0)).toBeUndefined();
    expect(storedLine(m, [{ ...slots[0], collars: 0 }], tz, T0)).toBeUndefined();
  });

  test("collars that should hold the next open and don't", () => {
    const c = (id: string, slots: CollarSlots["slots"], x: Partial<CollarSlots> = {}): CollarSlots =>
      ({ collar_id: id, limits: { outer: 128, holes: 16, hole_vertices: 32, total: 384, slots: 16, slot_bytes: 24576 }, slots, ...x });
    const s = (version: number, extra = {}) => ({ version, status: "received" as const, reported_at: iso(T0), ...extra });
    const collars = [
      c("a", [s(14)]),
      c("b", [s(9)]),
      c("c", [s(30, { copy_of: 14 })]),
      c("d", [], { parked: true }),
      c("e", [], { escaped: true }),
      c("f", [s(14, { status: "rejected" })]),
    ];
    expect(missingCollars(collars, 14)).toEqual(["b", "f"]);
  });
});

test("the countdown reads in hours within a day, days beyond", () => {
  expect(countdown(3 * H + 12 * 60_000 + 40_000)).toBe("03:12:40");
  expect(countdown(71 * H + 30 * 60_000)).toBe("2d 23h");
  expect(countdown(-5)).toBe("00:00:00");
});

describe("the queue by day", () => {
  test("opens with their back fence folded in, by farm day", () => {
    const days = queue(plan(), tz, T0 - H);
    expect(days.map((d) => d.day)).toEqual(["Mon", "Tue", "Wed"]);
    expect(days[0].rows.map((r) => [r.kind, r.index, rowTime(r, tz)])).toEqual([["open", 1, "07:00"], ["fence", 1, "11:00–11:20"]]);
    expect(days[0].rows[0].first).toBe(true);
    expect(days[1].rows[0].first).toBe(false);
  });

  test("held and skipped places ahead show; the past doesn't", () => {
    const ms = plan();
    ms[0].state = "done";
    ms.push(mv(0, 0, T0 + 24 * H, "skipped", { skipped: "held" }));
    ms[4] = { ...ms[4], state: "skipped", skipped: "skipped" };
    const days = queue(ms, tz, T0 + 5 * H);
    expect(days[0].rows.map((r) => r.kind)).toEqual(["fence"]);
    expect(days[1].rows.map((r) => r.kind)).toEqual(["held", "skipped", "fence"]);
  });
});

describe("the time rail", () => {
  test("the boundary and the strip at a time", () => {
    const ms = plan();
    expect(boundaryAt(ms, T0 - 1)).toBeUndefined();
    expect(boundaryAt(ms, T0 + 1)?.index).toBe(1);
    expect(boundaryAt(ms, T0 + 4 * H + 1)?.step).toBe(1);
    expect(stripAt(ms, T0 + 30 * H)).toBe(2);
    // Held places and skipped moves don't shape the fence.
    const held = [...ms, mv(1, 0, T0 + 24 * H, "skipped", { skipped: "held" })];
    expect(boundaryAt(held, T0 + 24 * H + 1)?.index).toBe(2);
  });

  test("grazed strips follow the back fence", () => {
    const ms = plan();
    expect(grazedAt(schedule, ms, T0 - 1)).toEqual([]);
    // Strip 2 open, its fence not closed: nothing behind strip 1 yet.
    expect(grazedAt(schedule, ms, T0 + H)).toEqual([]);
    // Closed: strip 1 is grazed ground.
    expect(grazedAt(schedule, ms, T0 + 5 * H)).toEqual([0]);
    expect(grazedAt(schedule, ms, T0 + 30 * H)).toEqual([0, 1]);
    // Without a back fence everything before the open strip is grazed (but still open).
    expect(grazedAt({ ...schedule, back_fence: { ...schedule.back_fence, enabled: false } }, ms, T0 + H)).toEqual([0]);
  });

  test("the back fence is the edge facing back along the strips", () => {
    const g = rect(offset(sw, 50, 0), 100, 200);
    const lines = backLine(strips, g, 2);
    expect(lines.length).toBe(1);
    // The west edge, where x is 50 m from the corner.
    const west = offset(sw, 50, 0)[0];
    for (const p of lines[0]) expect(Math.abs(p[0] - west)).toBeLessThan(1e-9);
    expect(backLine(strips, g, 0)).toEqual([]);
  });

  test("the rail spans to past the last move, with day marks", () => {
    const ms = plan();
    const [a, b] = railSpan(ms, T0 - H);
    expect(a).toBe(T0 - H);
    expect(b).toBe(T0 + 48 * H + 4 * H + 1_200_000 + H);
    const ticks = dayTicks(a, b, tz);
    expect(ticks.map((x) => x.label)).toEqual(["Tue", "Wed"]);
    expect(new Date(ticks[0].at).toISOString()).toBe("2026-09-29T05:00:00.000Z");
    // Across the fall-back change the midnights move with the clock.
    const nov = dayTicks(Date.parse("2026-10-31T12:00:00Z"), Date.parse("2026-11-02T12:00:00Z"), tz);
    expect(nov.map((x) => new Date(x.at).toISOString())).toEqual(["2026-11-01T05:00:00.000Z", "2026-11-02T06:00:00.000Z"]);
  });
});
