// Every number a person sees goes through here, in the farm's units (settings.units).
// Mirrors op_core::units: the API is SI (m, ha, kg, cm, m²); imperial shows ft, ac, in, lb, ft².

import { useMemo } from "react";
import type { Settings } from "./api";
import { store, useStore } from "./store";

export type Units = Settings["units"];
export type Quantity = "area" | "len" | "height" | "mass" | "per_head" | "density";

const AC_PER_HA = 2.4710538146716536;
const FT_PER_M = 3.280839895013123;
const IN_PER_CM = 0.3937007874015748;
const LB_PER_KG = 2.2046226218487757;
const FT2_PER_M2 = 10.763910416709722;

// Display value = SI value × factor.
const FACTOR: Record<Quantity, number> = {
  area: AC_PER_HA, len: FT_PER_M, height: IN_PER_CM, mass: LB_PER_KG, per_head: FT2_PER_M2, density: 1 / AC_PER_HA,
};
const LABEL: Record<Units, Record<Quantity, string>> = {
  metric: { area: "ha", len: "m", height: "cm", mass: "kg", per_head: "m²/hd", density: "AU/ha" },
  imperial: { area: "ac", len: "ft", height: "in", mass: "lb", per_head: "ft²/hd", density: "AU/ac" },
};

// Unit words a person may type after a number, per quantity, and what they mean in SI.
const TYPED: Record<Quantity, Record<string, number>> = {
  area: { ha: 1, ac: 1 / AC_PER_HA, acre: 1 / AC_PER_HA, acres: 1 / AC_PER_HA, "m²": 1e-4, m2: 1e-4 },
  len: { m: 1, ft: 1 / FT_PER_M, "'": 1 / FT_PER_M, km: 1000, mi: 1609.344, yd: 0.9144 },
  height: { cm: 1, in: 1 / IN_PER_CM, '"': 1 / IN_PER_CM, mm: 0.1, m: 100 },
  mass: { kg: 1, lb: 1 / LB_PER_KG, lbs: 1 / LB_PER_KG },
  per_head: { "m²/hd": 1, "m2/hd": 1, "ft²/hd": 1 / FT2_PER_M2, "ft2/hd": 1 / FT2_PER_M2 },
  density: { "au/ha": 1, "au/ac": AC_PER_HA },
};

// Digits with "," between thousands, whatever the browser's locale: texts and reports read the same.
export function num(v: number, decimals = 0): string {
  const s = Math.abs(v).toFixed(decimals);
  const [int, frac] = s.split(".");
  const grouped = int.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  const neg = v < 0 && Number(s) !== 0;
  return (neg ? "-" : "") + grouped + (frac ? "." + frac : "");
}

// Three significant figures, at least whole numbers: 5338.9 → 5,340, 496.2 → 496.
function sig3(v: number): string {
  const a = Math.abs(v);
  if (a < 1000) return num(v, 0);
  const step = 10 ** (Math.floor(Math.log10(a)) - 2);
  return num(Math.round(v / step) * step, 0);
}

// Whole units below 100, the nearest 10 from there: 16 ft, 60 m, 197 ft → 200 ft.
function lenText(v: number): string {
  return Math.abs(v) < 100 ? num(v, 0) : num(Math.round(v / 10) * 10, 0);
}

export interface Fmt {
  units: Units;
  area(ha: number): string; // "30.6 ac" | "12.4 ha"
  len(m: number): string; // "200 ft" | "60 m"
  height(cm: number): string; // "4 in" | "10 cm"
  mass(kg: number): string; // "1,200 lb" | "545 kg"
  perHead(m2: number): string; // "5,340 ft²/hd" | "496 m²/hd"
  density(au: number, ha: number): string; // "8.2 AU/ac" | "20.2 AU/ha"
  unitLabel(q: Quantity): string; // "ac"
  // SI → the number shown (no unit), rounded as the formatter rounds.
  toDisplay(si: number, q: Quantity): number;
  // What a person typed → SI. A unit word they type wins ("60 m" on an imperial farm); a bare
  // number is in the farm's unit. undefined when it isn't a number.
  parse(input: string, q: Quantity): number | undefined;
}

export const DECIMALS: Record<Quantity, number> = { area: 1, len: 0, height: 0, mass: 0, per_head: 0, density: 1 };

export function fmt(units: Units): Fmt {
  const imp = units === "imperial";
  const k = (q: Quantity) => (imp ? FACTOR[q] : 1);
  const label = (q: Quantity) => LABEL[units][q];
  const areaText = (ha: number) => {
    const v = ha * k("area");
    const a = Math.abs(v);
    return num(v, a >= 1000 ? 0 : a > 0 && a < 0.1 ? 2 : 1);
  };
  return {
    units,
    area: (ha) => `${areaText(ha)} ${label("area")}`,
    len: (m) => `${lenText(m * k("len"))} ${label("len")}`,
    height: (cm) => `${num(cm * k("height"))} ${label("height")}`,
    mass: (kg) => `${sig3(kg * k("mass"))} ${label("mass")}`,
    perHead: (m2) => `${sig3(m2 * k("per_head"))} ${label("per_head")}`,
    density: (au, ha) => `${num(ha > 0 ? (au / ha) * k("density") : 0, 1)} ${label("density")}`,
    unitLabel: label,
    toDisplay: (si, q) => {
      const d = 10 ** DECIMALS[q];
      return Math.round(si * k(q) * d) / d;
    },
    parse(input, q) {
      const m = input.trim().toLowerCase().replace(/,/g, "").match(/^([-+]?(?:\d+\.?\d*|\.\d+))\s*(.*)$/);
      if (!m) return undefined;
      const v = Number(m[1]);
      const word = m[2].replace(/\s+/g, "");
      if (!word) return v / k(q);
      const per = TYPED[q][word];
      return per === undefined ? undefined : v * per;
    },
  };
}

const cache = new Map<Units, Fmt>();
const of = (u: Units | undefined) => {
  const units = u ?? "metric";
  let f = cache.get(units);
  if (!f) cache.set(units, (f = fmt(units)));
  return f;
};

// Outside React (map overlays, texts built in the browser).
export const unitsNow = (): Fmt => of(store.get().state?.settings.units);

export function useUnits(): Fmt {
  const u = useStore((s) => s.state?.settings.units);
  return useMemo(() => of(u), [u]);
}
