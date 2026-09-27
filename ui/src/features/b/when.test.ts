import { describe, expect, test } from "bun:test";
import { atLabel, nextAt, offsetMin, soon, zonedToUtc } from "./when";

const CHI = "America/Chicago";
const iso = (t: number) => new Date(t).toISOString();

describe("farm-local times", () => {
  test("offsets", () => {
    expect(offsetMin(Date.parse("2026-09-27T12:00:00Z"), CHI)).toBe(-300);
    expect(offsetMin(Date.parse("2026-12-27T12:00:00Z"), CHI)).toBe(-360);
    expect(offsetMin(Date.parse("2026-09-27T12:00:00Z"), "Asia/Kolkata")).toBe(330);
  });

  test("a wall time in the farm's zone", () => {
    expect(iso(zonedToUtc("2026-09-27", "07:00", CHI))).toBe("2026-09-27T12:00:00.000Z");
    expect(iso(zonedToUtc("2026-12-27", "07:00", CHI))).toBe("2026-12-27T13:00:00.000Z");
    // New Zealand went to daylight time (+13) at 02:00 that morning.
    expect(iso(zonedToUtc("2026-09-27", "07:00", "Pacific/Auckland"))).toBe("2026-09-26T18:00:00.000Z");
  });

  test("across daylight saving", () => {
    // 1 Nov 2026: 01:30 happens twice in Chicago; the first (CDT) one.
    expect(iso(zonedToUtc("2026-11-01", "01:30", CHI))).toBe("2026-11-01T06:30:00.000Z");
    expect(iso(zonedToUtc("2026-11-01", "07:00", CHI))).toBe("2026-11-01T13:00:00.000Z");
    // 8 Mar 2026: 02:30 doesn't exist; the clock reads 03:30 then.
    expect(iso(zonedToUtc("2026-03-08", "02:30", CHI))).toBe("2026-03-08T08:30:00.000Z");
    expect(iso(zonedToUtc("2026-03-08", "07:00", CHI))).toBe("2026-03-08T12:00:00.000Z");
  });

  test("the next time the clock reads it: today, else tomorrow", () => {
    const now = Date.parse("2026-09-27T12:30:00Z"); // 07:30 in Iowa
    expect(iso(nextAt("07:35", CHI, now))).toBe("2026-09-27T12:35:00.000Z");
    expect(iso(nextAt("07:00", CHI, now))).toBe("2026-09-28T12:00:00.000Z");
    // Late evening in Iowa is already tomorrow in UTC.
    const late = Date.parse("2026-09-28T04:00:00Z"); // 23:00 on the 27th
    expect(iso(nextAt("23:30", CHI, late))).toBe("2026-09-28T04:30:00.000Z");
    expect(iso(nextAt("06:00", CHI, late))).toBe("2026-09-28T11:00:00.000Z");
  });

  test("labels", () => {
    const now = Date.parse("2026-09-27T12:30:00Z");
    expect(atLabel(Date.parse("2026-09-27T12:35:00Z"), CHI, now)).toBe("07:35");
    expect(atLabel(Date.parse("2026-09-30T12:00:00Z"), CHI, now)).toBe("Wed 07:00");
    expect(soon(CHI, Date.parse("2026-09-27T12:30:20Z"))).toBe("07:36");
  });
});
