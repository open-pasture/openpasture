import { useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { api, type Animal, type Sex } from "../../api";
import { store } from "../../store";
import { sexWord } from "./herd";

export type EditField = "tag" | "name" | "eid" | "breed" | "sex" | "born" | "notes";

// A value that becomes an input when clicked (for those who may edit). Enter or leaving it
// saves; Escape puts it back. An emptied field is cleared.
export function Edit({ a, field, can, onError, placeholder }: {
  a: Animal; field: EditField; can: boolean; onError: (msg?: string) => void; placeholder?: string;
}) {
  const value = (a[field] as string | undefined) ?? "";
  const [editing, setEditing] = useState(false);
  const [v, setV] = useState(value);
  // Enter saves and closes; the blur that follows must not save again.
  const open = useRef(false);
  const shown: ReactNode = field === "sex" ? sexWord(a.sex) : value;
  if (!can || a.removed_at) return <>{shown || <span className="dim">–</span>}</>;

  const save = async (next: string) => {
    if (!open.current) return;
    open.current = false;
    setEditing(false);
    const t = next.trim();
    if (t === value) return;
    onError(undefined);
    try {
      await api.updateAnimal(a.id, { [field]: t || null } as Partial<Animal>);
      await store.refresh();
    } catch (e) {
      onError(`${a.tag}: ${(e as Error).message}`);
    }
  };
  const keys = (e: KeyboardEvent<HTMLInputElement | HTMLSelectElement>) => {
    if (e.key === "Enter") void save((e.target as HTMLInputElement).value);
    if (e.key === "Escape") {
      e.stopPropagation();
      open.current = false;
      setEditing(false);
    }
  };

  if (!editing)
    return (
      <button type="button" className="editv" onClick={(e) => { e.stopPropagation(); setV(value); open.current = true; setEditing(true); }}>
        {shown || <span className="dim">{placeholder ?? "–"}</span>}
      </button>
    );
  if (field === "sex")
    return (
      <select className="input sm mono editin" autoFocus value={v} aria-label="sex" onKeyDown={keys}
        onChange={(e) => void save(e.target.value)} onBlur={() => { open.current = false; setEditing(false); }}>
        <option value="">–</option>
        {(["female", "male", "castrated"] as Sex[]).map((s) => <option key={s} value={s}>{sexWord(s)}</option>)}
      </select>
    );
  return (
    <input className="input sm mono editin" autoFocus spellCheck={false} autoComplete="off" aria-label={field}
      type={field === "born" ? "date" : "text"} value={v} onChange={(e) => setV(e.target.value)} onKeyDown={keys}
      onBlur={(e) => void save(e.target.value)} />
  );
}
