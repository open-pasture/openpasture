import { describe, expect, test } from "bun:test";
import { fmt, num, type Quantity, type Units } from "./units";
// op_core::units' answers for the same inputs (crates/op-core/tests/units_vectors.rs keeps it current).
import vectors from "./units.vectors.json";

const metric = fmt("metric");
const imperial = fmt("imperial");

describe("formatting", () => {
  test("metric", () => {
    expect(metric.area(12.4)).toBe("12.4 ha");
    expect(metric.len(60)).toBe("60 m");
    expect(metric.height(10)).toBe("10 cm");
    expect(metric.mass(545)).toBe("545 kg");
    expect(metric.perHead(496.2)).toBe("496 m²/hd");
    expect(metric.density(250, 12.4)).toBe("20.2 AU/ha");
    expect(metric.unitLabel("area")).toBe("ha");
  });

  test("imperial", () => {
    expect(imperial.area(12.4)).toBe("30.6 ac");
    expect(imperial.len(5)).toBe("16 ft");
    expect(imperial.len(10)).toBe("33 ft");
    expect(imperial.height(10)).toBe("4 in");
    expect(imperial.mass(545)).toBe("1,200 lb");
    expect(imperial.len(60)).toBe("200 ft");
    expect(metric.len(402)).toBe("400 m");
    expect(imperial.perHead(496)).toBe("5,340 ft²/hd");
    expect(imperial.density(250, 12.4)).toBe("8.2 AU/ac");
    expect(imperial.unitLabel("per_head")).toBe("ft²/hd");
  });

  test("small and large areas", () => {
    expect(imperial.area(1.214)).toBe("3.0 ac");
    expect(metric.area(0.04)).toBe("0.04 ha");
    expect(metric.area(1234.5)).toBe("1,235 ha");
  });

  test("thousands and signs", () => {
    expect(num(1234567)).toBe("1,234,567");
    expect(num(-1200.5, 1)).toBe("-1,200.5");
    expect(num(-0.04, 1)).toBe("0.0");
  });
});

describe("parsing input", () => {
  test("a bare number is in the farm's unit", () => {
    expect(imperial.parse("16", "len")).toBeCloseTo(4.877, 3);
    expect(metric.parse("16", "len")).toBe(16);
    expect(imperial.parse("30.6", "area")).toBeCloseTo(12.383, 3);
    expect(imperial.parse("4", "height")).toBeCloseTo(10.16, 2);
    expect(imperial.parse("1,200", "mass")).toBeCloseTo(544.3, 1);
    expect(imperial.parse("8.2", "density")).toBeCloseTo(20.26, 2);
  });

  test("a typed unit wins", () => {
    expect(imperial.parse("5 m", "len")).toBe(5);
    expect(metric.parse("16 ft", "len")).toBeCloseTo(4.877, 3);
    expect(metric.parse("30.6 ac", "area")).toBeCloseTo(12.383, 3);
    expect(metric.parse("4in", "height")).toBeCloseTo(10.16, 2);
    expect(metric.parse("5,340 ft²/hd", "per_head")).toBeCloseTo(496.1, 1);
  });

  test("nonsense is undefined", () => {
    expect(metric.parse("", "len")).toBeUndefined();
    expect(metric.parse("abc", "len")).toBeUndefined();
    expect(metric.parse("5 kg", "len")).toBeUndefined();
  });

  test("display values round like the text", () => {
    expect(imperial.toDisplay(5, "len")).toBe(16);
    expect(imperial.toDisplay(12.4, "area")).toBe(30.6);
    expect(metric.toDisplay(12.44, "area")).toBe(12.4);
  });
});

describe("the same digits as op_core::units", () => {
  const cases: [Units, Quantity][] = (["metric", "imperial"] as const).flatMap((u) =>
    (["area", "len", "height", "mass", "per_head"] as const).map((q): [Units, Quantity] => [u, q]),
  );
  test.each(cases)("%s %s", (units, q) => {
    const f = fmt(units);
    const text = (x: number) =>
      q === "area" ? f.area(x) : q === "len" ? f.len(x) : q === "height" ? f.height(x) : q === "mass" ? f.mass(x) : f.perHead(x);
    const rust: string[] = (vectors as Record<Units, Record<Quantity, string[]>>)[units][q];
    expect(rust.length).toBe(vectors.si.length);
    const differ = vectors.si.flatMap((x, i) => (text(x) === rust[i] ? [] : [`${x} → ${text(x)}, op_core ${rust[i]}`]));
    expect(differ).toEqual([]);
  });

  test.each(["metric", "imperial"] as const)("%s density", (units) => {
    const f = fmt(units);
    const rust = vectors[units].density;
    const differ = vectors.density_in.flatMap(([au, ha], i) =>
      f.density(au, ha) === rust[i] ? [] : [`${au} AU on ${ha} → ${f.density(au, ha)}, op_core ${rust[i]}`],
    );
    expect(differ).toEqual([]);
  });
});
