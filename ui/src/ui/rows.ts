// The Table's arithmetic, kept pure so it can be tested: which rows a scroll position
// shows, filtering, sorting and selection.

// Rows [start, end) to render for a viewport, with `overscan` extra rows each side.
export function windowRange(scrollTop: number, viewport: number, rowHeight: number, count: number, overscan = 8): [number, number] {
  if (count === 0 || rowHeight <= 0) return [0, 0];
  const first = Math.floor(Math.max(0, scrollTop) / rowHeight);
  const visible = Math.ceil(Math.max(0, viewport) / rowHeight) + 1;
  const start = Math.max(0, Math.min(count - 1, first) - overscan);
  const end = Math.min(count, first + visible + overscan);
  return [start, Math.max(start, end)];
}

// Rows whose text contains every word of the query, any case. An empty query keeps all.
export function filterRows<R>(rows: R[], query: string, text: (r: R) => string): R[] {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return rows;
  return rows.filter((r) => {
    const t = text(r).toLowerCase();
    return words.every((w) => t.includes(w));
  });
}

export type SortDir = "asc" | "desc";
export interface SortState { col: string; dir: SortDir }

// A stable sort by one comparator; desc reverses it (ties keep their order).
export function sortRows<R>(rows: R[], cmp: ((a: R, b: R) => number) | undefined, dir: SortDir): R[] {
  if (!cmp) return rows;
  const s = dir === "asc" ? 1 : -1;
  return rows.map((r, i) => [r, i] as const).sort((a, b) => s * cmp(a[0], b[0]) || a[1] - b[1]).map(([r]) => r);
}

// Clicking a header: ascending, then descending, then unsorted.
export function nextSort(cur: SortState | undefined, col: string): SortState | undefined {
  if (!cur || cur.col !== col) return { col, dir: "asc" };
  return cur.dir === "asc" ? { col, dir: "desc" } : undefined;
}

// Natural order for mixed values: numbers by value, text with numbers in it ("P10" after "P9"), empty last.
export function compareValues(a: unknown, b: unknown): number {
  const empty = (v: unknown) => v === undefined || v === null || v === "";
  if (empty(a) || empty(b)) return empty(a) === empty(b) ? 0 : empty(a) ? 1 : -1;
  if (typeof a === "number" && typeof b === "number") return a - b;
  return String(a).localeCompare(String(b), undefined, { numeric: true, sensitivity: "base" });
}

// A click on a row's check: toggles it; with shift, sets every row from the last one
// clicked to this one to this row's new state. `order` is the rows as shown.
export function toggleSelect(sel: ReadonlySet<string>, key: string, order: string[], shift: boolean, anchor?: string): Set<string> {
  const next = new Set(sel);
  const on = !sel.has(key);
  const a = anchor === undefined ? -1 : order.indexOf(anchor);
  const b = order.indexOf(key);
  if (shift && a >= 0 && b >= 0) {
    const [lo, hi] = a < b ? [a, b] : [b, a];
    for (let i = lo; i <= hi; i++) on ? next.add(order[i]) : next.delete(order[i]);
  } else on ? next.add(key) : next.delete(key);
  return next;
}

// The header check: selects every shown row, or clears them when all are already selected.
export function toggleAll(sel: ReadonlySet<string>, shown: string[]): Set<string> {
  const next = new Set(sel);
  const all = shown.length > 0 && shown.every((k) => sel.has(k));
  for (const k of shown) all ? next.delete(k) : next.add(k);
  return next;
}
