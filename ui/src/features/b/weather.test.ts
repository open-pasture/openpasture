import { describe, expect, test } from "bun:test";
import type { Weather } from "../../api/b";
import { rain, temp, todayIn, weatherLines } from "./weather";

describe("temperature and rain", () => {
  test("degrees in the farm's units", () => {
    expect(temp(18, "metric")).toBe("18°C");
    expect(temp(18, "imperial")).toBe("64°F");
    expect(temp(-0.4, "metric")).toBe("0°C");
    expect(temp(-12.5, "metric")).toBe("-13°C");
    expect(temp(0, "imperial", true)).toBe("32°");
  });

  test("rain", () => {
    expect(rain(3.5, "metric")).toBe("3.5 mm");
    expect(rain(12.4, "metric")).toBe("12 mm");
    expect(rain(0.02, "metric")).toBe("0 mm");
    expect(rain(3.5, "imperial")).toBe("0.14 in");
    expect(rain(30.5, "imperial")).toBe("1.2 in");
    expect(rain(0.1, "imperial")).toBe("0 in");
  });
});

describe("weather lines", () => {
  const w: Weather = {
    status: "ok",
    current: { air_temp_c: 18.2, precip_mm_24h: 4.5, snow_depth_cm: null },
    history: [{ date: "2026-09-26", precip_mm: 4.5, temp_max_c: 20, temp_min_c: 9 }],
    forecast: [
      { date: "2026-09-27", precip_mm: 0.2, temp_max_c: 22.4, temp_min_c: 10.6 },
      { date: "2026-09-28", precip_mm: 12, temp_max_c: 19, temp_min_c: 8 },
      { date: "2026-09-29", precip_mm: 0, temp_max_c: 17, temp_min_c: 6 },
      { date: "2026-09-30", precip_mm: 30, temp_max_c: 15, temp_min_c: 5 },
    ],
  };

  test("now, then three days from today", () => {
    expect(weatherLines(w, "metric", "2026-09-27")).toEqual(["18°C now, 4.5 mm last 24 h", "Sun 22°/11°", "Mon 19°/8° 12 mm", "Tue 17°/6°"]);
    expect(weatherLines(w, "imperial", "2026-09-28")).toEqual(["65°F now, 0.18 in last 24 h", "Mon 66°/46° 0.47 in", "Tue 63°/43°", "Wed 59°/41° 1.2 in"]);
  });

  test("snow on the ground reads as a height", () => {
    const snowy: Weather = { status: "ok", current: { air_temp_c: -4, precip_mm_24h: 0, snow_depth_cm: 8.2 } };
    expect(weatherLines(snowy, "metric", "2026-01-10")).toEqual(["-4°C now, 8 cm snow"]);
    expect(weatherLines(snowy, "imperial", "2026-01-10")).toEqual(["25°F now, 3 in snow"]);
  });

  test("nothing without numbers", () => {
    expect(weatherLines(undefined, "metric", "2026-09-27")).toEqual([]);
    expect(weatherLines({ status: "ok", current: {}, forecast: [{ date: "2026-09-27" }] }, "metric", "2026-09-27")).toEqual([]);
  });

  test("today where the farm is", () => {
    // 03:00 UTC on the 28th is still the 27th in Iowa.
    expect(todayIn("America/Chicago", Date.parse("2026-09-28T03:00:00Z"))).toBe("2026-09-27");
    expect(todayIn("Europe/London", Date.parse("2026-09-28T03:00:00Z"))).toBe("2026-09-28");
  });
});
