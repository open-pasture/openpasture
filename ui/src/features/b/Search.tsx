import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import type { KnowledgeHit } from "../../api";
import { searchAll, type SearchHit } from "../../registry";
import { searchOpen } from "../../store/b";

// A 7×7 pixel magnifier, in the pixel marks' style.
const GLASS = [".###...", "#...#..", "#...#..", "#...#..", ".###...", ".....#.", "......#"];

function Glass() {
  return (
    <svg className="px" width={14} height={14} viewBox="0 0 7 7" aria-hidden>
      {GLASS.flatMap((row, y) => [...row].map((c, x) => (c === "#" ? <rect key={`${x},${y}`} x={x} y={y} width={1} height={1} /> : null)))}
    </svg>
  );
}

// Top bar: "/" (or the glass) opens a box that finds animals, paddocks and farm knowledge.
export function SearchBox() {
  const open = searchOpen.use((v) => v);
  const [q, setQ] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [sel, setSel] = useState(0);
  const box = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (open) return;
    setQ("");
    setHits([]);
    setSel(0);
  }, [open]);

  useEffect(() => {
    if (!q.trim()) return setHits([]);
    let live = true;
    const t = setTimeout(() => {
      void searchAll(q).then((h) => {
        if (!live) return;
        setHits(h.slice(0, 12));
        setSel(0);
      });
    }, 120);
    return () => {
      live = false;
      clearTimeout(t);
    };
  }, [q]);

  // A click outside closes it.
  useEffect(() => {
    if (!open) return;
    const down = (e: MouseEvent) => box.current?.contains(e.target as Node) || searchOpen.set(false);
    window.addEventListener("mousedown", down);
    return () => window.removeEventListener("mousedown", down);
  }, [open]);

  if (!open)
    return (
      <button type="button" className="bfind" aria-label="Search" title="Search (/)" onClick={() => searchOpen.set(true)}>
        <Glass />
        <span className="blabel" aria-hidden="true">Search</span>
        <kbd aria-hidden="true">/</kbd>
      </button>
    );

  const pick = (h: SearchHit | undefined) => {
    if (!h) return;
    searchOpen.set(false);
    h.run();
  };
  const onKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Escape") {
      e.preventDefault();
      e.stopPropagation();
      searchOpen.set(false);
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (hits.length) setSel((s) => (s + (e.key === "ArrowDown" ? 1 : hits.length - 1)) % hits.length);
    } else if (e.key === "Enter") {
      e.preventDefault();
      pick(hits[sel]);
    }
  };
  return (
    <div className="bsearch" ref={box} role="search">
      <input className="input sm" autoFocus spellCheck={false} autoComplete="off" placeholder="Find an animal, paddock, collar or note" aria-label="Find"
        value={q} onChange={(e) => setQ(e.target.value)} onKeyDown={onKey} />
      {hits.length > 0 && (
        <ul className="bhits" role="listbox" aria-label="Found">
          {hits.map((h, i) => (
            <li key={`${h.kind}:${h.label}:${i}`} role="option" aria-selected={i === sel}>
              <button type="button" onMouseEnter={() => setSel(i)} onClick={() => pick(h)}>
                <span>{h.label}</span>
                <span className="mono dim">{h.kind}</span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

// A knowledge entry in the map's side sheet.
export function KnowledgeSheet({ hit }: { hit: KnowledgeHit }) {
  return (
    <div className="bknow">
      <h3>{hit.title}</h3>
      <p className="mono dim">{hit.source}</p>
      <p className="body">{hit.body}</p>
    </div>
  );
}
