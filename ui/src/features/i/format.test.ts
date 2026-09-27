import { describe, expect, test } from "bun:test";
import {
  cellText, columnDecimals, currencyFor, places, defaultRange, heading, localToday, money, numeric, parsePrintRest, printHash, rateShown, rateStored, validRange,
} from "./format";

const HA_PER_AC = 1 / 2.471053814671653;

describe("report cells", () => {
  test("a column shows as many places as its most precise number, at most two", () => {
    expect(columnDecimals([250, 325.5, null, "Total"])).toBe(1);
    expect(columnDecimals([100, 90])).toBe(0);
    expect(columnDecimals([1655.64, 12.5])).toBe(2);
    expect(columnDecimals([0.125])).toBe(2);
  });

  test("a column's own places win: money has two", () => {
    expect(places({ key: "amount", label: "Amount", unit: "USD", decimals: 2 }, [2322.2, 1840.87])).toBe(2);
    expect(cellText(2322.2, places({ key: "amount", label: "Amount", decimals: 2 }, [2322.2]))).toBe("2,322.20");
    expect(places({ key: "au", label: "AU" }, [250, 325.5])).toBe(1);
  });

  test("numbers print with thousands commas at the column's places; null is empty", () => {
    expect(cellText(2743.8, 1)).toBe("2,743.8");
    expect(cellText(250, 1)).toBe("250.0");
    expect(cellText(1343.75, 2)).toBe("1,343.75");
    expect(cellText(null, 1)).toBe("");
    expect(cellText("2025-09-06 07:00", 0)).toBe("2025-09-06 07:00");
  });

  test("a column of numbers (and blanks) aligns right; text or mixed doesn't", () => {
    expect(numeric([1, null, 2.5])).toBe(true);
    expect(numeric(["P1", "P2"])).toBe(false);
    expect(numeric(["Yes", 3])).toBe(false);
    expect(numeric([null, null])).toBe(false);
  });

  test("a heading carries its unit", () => {
    expect(heading({ key: "area", label: "Area", unit: "ac" })).toBe("Area (ac)");
    expect(heading({ key: "days", label: "Days" })).toBe("Days");
  });
});

describe("dates", () => {
  test("today is the farm's day, not the browser's", () => {
    const t = new Date("2026-09-27T03:30:00Z");
    expect(localToday("America/Chicago", t)).toBe("2026-09-26");
    expect(localToday("Pacific/Auckland", t)).toBe("2026-09-27");
    expect(localToday("Not/AZone", t)).toBe("2026-09-27");
    expect(localToday(undefined, t)).toBe("2026-09-27");
  });

  test("reports default to this year so far", () => {
    expect(defaultRange("2026-09-27")).toEqual({ from: "2026-01-01", to: "2026-09-27" });
    expect(validRange({ from: "2026-01-01", to: "2026-09-27" })).toBe(true);
    expect(validRange({ from: "2026-09-28", to: "2026-09-27" })).toBe(false);
    expect(validRange({ from: "", to: "2026-09-27" })).toBe(false);
  });

  test("the print address round-trips the report and its dates", () => {
    const hash = printHash("nrcs_528", { from: "2025-09-01", to: "2025-09-30", herd_id: "herd_1" });
    expect(hash).toBe("#/print/report/nrcs_528?from=2025-09-01&to=2025-09-30&herd_id=herd_1");
    const rest = hash.slice("#/print/report/".length);
    expect(parsePrintRest(rest, "2026-09-27")).toEqual({ id: "nrcs_528", q: { from: "2025-09-01", to: "2025-09-30", herd_id: "herd_1" } });
    expect(parsePrintRest("lease_head_days", "2026-09-27")).toEqual({ id: "lease_head_days", q: { from: "2026-01-01", to: "2026-09-27" } });
  });
});

describe("lease rates", () => {
  test("a per-acre rent is stored per hectare and shown per acre", () => {
    const stored = rateStored(45, "acre_season", HA_PER_AC);
    expect(stored).toBeCloseTo(111.197, 3);
    expect(money(rateShown(stored, "acre_season", HA_PER_AC))).toBe("45.00");
    // On a metric farm the area unit is the hectare.
    expect(rateStored(110, "acre_season", 1)).toBe(110);
  });

  test("rates per head, AU or month are stored as typed", () => {
    for (const per of ["head_day", "au_day", "aum", "pair_month"] as const) {
      expect(rateStored(0.85, per, HA_PER_AC)).toBe(0.85);
      expect(rateShown(0.85, per, HA_PER_AC)).toBe(0.85);
    }
  });

  test("the currency guess follows the farm's zone", () => {
    expect(currencyFor("America/Chicago")).toBe("USD");
    expect(currencyFor("America/Toronto")).toBe("CAD");
    expect(currencyFor("Europe/London")).toBe("GBP");
    expect(currencyFor("Europe/Paris")).toBe("EUR");
    expect(currencyFor("Australia/Sydney")).toBe("AUD");
    expect(currencyFor("Pacific/Auckland")).toBe("NZD");
    expect(currencyFor(undefined)).toBe("USD");
  });
});
