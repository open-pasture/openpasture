import { useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent, type ReactNode } from "react";
import { Check } from "./Check";
import { Input } from "./Input";
import { filterRows, nextSort, sortRows, toggleAll, toggleSelect, windowRange, type SortState } from "./rows";

export interface Column<R> {
  id: string;
  label: ReactNode;
  // px; the last column without one takes the rest.
  width?: number;
  // Present when the column sorts: click the header for ascending, descending, unsorted.
  sort?: (a: R, b: R) => number;
  cell: (row: R) => ReactNode;
}

// A .tbl that renders only the rows in view, so 250 or 25,000 scroll alike. Give it a
// height, or a parent that bounds it. Optional: a text filter (every word must appear in
// text(row)), a check column for selection (shift-click selects a run), sortable headers.
export function Table<R>({
  rows, columns, rowKey, text, query, filter, selected, onSelect, onRowClick, rowHeight = 37, height, initialSort, className,
}: {
  rows: R[];
  columns: Column<R>[];
  rowKey: (row: R) => string;
  text?: (row: R) => string;
  // Controlled filter text; with `filter` and no query the table shows its own input.
  query?: string;
  filter?: boolean;
  selected?: ReadonlySet<string>;
  onSelect?: (next: Set<string>) => void;
  onRowClick?: (row: R) => void;
  rowHeight?: number;
  height?: number | string;
  initialSort?: SortState;
  className?: string;
}) {
  const [own, setOwn] = useState("");
  const [sort, setSort] = useState<SortState | undefined>(initialSort);
  const [top, setTop] = useState(0);
  const [view, setView] = useState(600);
  const box = useRef<HTMLDivElement>(null);
  const anchor = useRef<string>(undefined);

  const q = query ?? own;
  const shown = useMemo(() => {
    const kept = text ? filterRows(rows, q, text) : rows;
    if (!sort) return kept;
    const col = columns.find((c) => c.id === sort.col);
    return col?.sort ? sortRows(kept, col.sort, sort.dir) : kept;
  }, [rows, q, text, sort, columns]);
  const keys = useMemo(() => shown.map(rowKey), [shown, rowKey]);

  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    setView(el.clientHeight);
    const ro = new ResizeObserver(() => setView(el.clientHeight));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  // A new filter or sort starts at the top.
  useEffect(() => {
    if (box.current) box.current.scrollTop = 0;
    setTop(0);
  }, [q, sort]);

  const [start, end] = windowRange(top, view, rowHeight, shown.length);
  const selectable = !!(selected && onSelect);
  const span = columns.length + (selectable ? 1 : 0);
  const all = selectable && keys.length > 0 && keys.every((k) => selected!.has(k));

  const pick = (key: string, e: MouseEvent | KeyboardEvent) => {
    if (!selected || !onSelect) return;
    onSelect(toggleSelect(selected, key, keys, e.shiftKey, anchor.current));
    anchor.current = key;
  };

  return (
    <div className={"tblbox" + (className ? " " + className : "")} style={height !== undefined ? { height } : undefined}>
      {filter && query === undefined && (
        <Input className="sm tblfilter" placeholder="Filter" aria-label="Filter" value={own} onChange={(e) => setOwn(e.target.value)} />
      )}
      <div className="tblw" ref={box} onScroll={(e) => setTop(e.currentTarget.scrollTop)}>
        <table className="tbl">
          <colgroup>
            {selectable && <col style={{ width: 40 }} />}
            {columns.map((c) => <col key={c.id} style={c.width ? { width: c.width } : undefined} />)}
          </colgroup>
          <thead>
            <tr>
              {selectable && (
                <th className="sel"><Check checked={all} onChange={() => onSelect!(toggleAll(selected!, keys))} label="Select all" /></th>
              )}
              {columns.map((c) => {
                const dir = sort?.col === c.id ? sort.dir : undefined;
                return (
                  <th key={c.id} aria-sort={dir ? (dir === "asc" ? "ascending" : "descending") : undefined}>
                    {c.sort ? (
                      <button type="button" className="sorth" data-dir={dir} onClick={() => setSort(nextSort(sort, c.id))}>{c.label}</button>
                    ) : c.label}
                  </th>
                );
              })}
            </tr>
          </thead>
          <tbody>
            {start > 0 && <tr aria-hidden="true" className="gap"><td colSpan={span} style={{ height: start * rowHeight }} /></tr>}
            {shown.slice(start, end).map((r, i) => {
              const k = keys[start + i];
              const on = selectable && selected!.has(k);
              return (
                <tr key={k} style={{ height: rowHeight }} aria-selected={selectable ? on : undefined}
                  className={onRowClick ? "click" : undefined} onClick={onRowClick ? () => onRowClick(r) : undefined}>
                  {selectable && (
                    <td className="sel">
                      <Check checked={on} onChange={(_, e) => pick(k, e)} label="Select" />
                    </td>
                  )}
                  {columns.map((c) => <td key={c.id}>{c.cell(r)}</td>)}
                </tr>
              );
            })}
            {end < shown.length && <tr aria-hidden="true" className="gap"><td colSpan={span} style={{ height: (shown.length - end) * rowHeight }} /></tr>}
          </tbody>
        </table>
      </div>
    </div>
  );
}
