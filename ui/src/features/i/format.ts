// Pure helpers for the reports UI: how report cells print, the default dates, the print
// page's address, lease rates in the farm's area unit. No React, no network (bun test).

import { num } from "../../units";
import type { Cell, Column, RatePer, ReportQuery } from "../../api/i";

// Places a number column shows: as many as its most precise value, at most two, so a
// column reads "250.0 / 325.5" rather than "250 / 325.5".
export function columnDecimals(values: Cell[]): number {
  let d = 0;
  for (const v of values) {
    if (typeof v !== "number" || !Number.isFinite(v)) continue;
    const s = String(v);
    const dot = s.indexOf(".");
    if (dot >= 0 && !s.includes("e")) d = Math.max(d, s.length - dot - 1);
  }
  return Math.min(d, 2);
}

// The places a column prints with: the report's say, else its most precise value.
export const places = (c: Column, values: Cell[]) => c.decimals ?? columnDecimals(values);

// A cell as a person reads it: numbers with "," thousands (as op_core prints), empty for none.
export function cellText(v: Cell, decimals: number): string {
  if (v === null || v === undefined) return "";
  return typeof v === "number" ? num(v, decimals) : v;
}

// Whether a column holds numbers (they align right).
export const numeric = (values: Cell[]) => values.some((v) => typeof v === "number") && values.every((v) => v === null || typeof v === "number");

// "Area" with its unit "ac", for a heading.
export const heading = (c: Column) => (c.unit ? `${c.label} (${c.unit})` : c.label);

// Today in the farm's time zone, YYYY-MM-DD.
export function localToday(tz: string | undefined, now = new Date()): string {
  const f = (zone: string) => new Intl.DateTimeFormat("en-CA", { timeZone: zone, year: "numeric", month: "2-digit", day: "2-digit" }).format(now);
  try {
    return f(tz || "UTC");
  } catch {
    return f("UTC");
  }
}

// This year to today.
export const defaultRange = (today: string): ReportQuery => ({ from: `${today.slice(0, 4)}-01-01`, to: today });

export const validRange = (q: ReportQuery) => /^\d{4}-\d\d-\d\d$/.test(q.from) && /^\d{4}-\d\d-\d\d$/.test(q.to) && q.from <= q.to;

// #/print/report/<id>?from=&to=[&herd_id=]
export function printHash(id: string, q: ReportQuery): string {
  const p = new URLSearchParams({ from: q.from, to: q.to });
  if (q.herd_id) p.set("herd_id", q.herd_id);
  return `#/print/report/${id}?${p}`;
}

// The print page's rest ("paddock_record?from=…") back to a report id and dates.
export function parsePrintRest(rest: string, today: string): { id: string; q: ReportQuery } {
  const [id, query = ""] = rest.split("?");
  const p = new URLSearchParams(query);
  const d = defaultRange(today);
  const q: ReportQuery = { from: p.get("from") || d.from, to: p.get("to") || d.to };
  const herd = p.get("herd_id");
  if (herd) q.herd_id = herd;
  return { id: id.replace(/\/$/, ""), q };
}

// Lease rates. acre_season is stored per hectare; `haPerUnit` is how many hectares one of
// the farm's area units holds (1 for ha, 0.4047 for ac).
export function rateShown(perHa: number, per: RatePer, haPerUnit: number): number {
  return per === "acre_season" ? perHa * haPerUnit : perHa;
}

export function rateStored(shown: number, per: RatePer, haPerUnit: number): number {
  return per === "acre_season" ? shown / haPerUnit : shown;
}

// Two places, no grouping: what a rate input holds.
export const money = (v: number) => (Math.round(v * 100) / 100).toFixed(2);

// A guess at the farm's currency from its time zone; the field stays editable.
export function currencyFor(tz: string | undefined): string {
  const z = tz ?? "";
  if (z === "Europe/London" || z === "Europe/Belfast") return "GBP";
  if (z === "Europe/Dublin" || /^Europe\/(Paris|Berlin|Madrid|Rome|Amsterdam|Brussels|Vienna|Lisbon|Athens|Helsinki|Luxembourg|Bratislava|Ljubljana|Tallinn|Riga|Vilnius|Zagreb|Malta)$/.test(z)) return "EUR";
  if (z.startsWith("Australia/")) return "AUD";
  if (z === "Pacific/Auckland" || z === "Pacific/Chatham") return "NZD";
  if (/^America\/(Toronto|Vancouver|Edmonton|Winnipeg|Halifax|Regina|St_Johns|Moncton|Whitehorse|Yellowknife|Iqaluit)$/.test(z)) return "CAD";
  if (z === "Africa/Johannesburg") return "ZAR";
  if (z === "America/Argentina/Buenos_Aires") return "ARS";
  if (z === "America/Sao_Paulo") return "BRL";
  return "USD";
}
