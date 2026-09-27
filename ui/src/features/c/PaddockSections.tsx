import { useEffect, useState } from "react";
import type { Paddock } from "../../api";
import { cApi, type Layout } from "../../api/c";
import { cState } from "../../store/c";
import { store, useStore } from "../../store";
import { Button, Input } from "../../ui";
import { toXY } from "../../geo";
import { attempt } from "../../util";
import { startTool } from "./freehand";

// Saved strip layouts of this paddock: Apply opens the strip tool on it; the name renames in place.
export function PaddockLayouts({ paddock, herdId }: { paddock: Paddock; herdId?: string }) {
  const [list, setList] = useState<Layout[]>([]);
  const [editing, setEditing] = useState<string>();
  const [err, setErr] = useState<string>();
  // Apply needs the strip tool, which shows while the selected herd is in a paddock.
  const canApply = useStore((s) => !!herdId && !!s.state?.herds.find((h) => h.id === herdId)?.paddock_id);

  useEffect(() => {
    let live = true;
    cApi.layouts(paddock.id).then((l) => live && setList(l), () => live && setList([]));
    return () => {
      live = false;
    };
  }, [paddock.id]);

  if (!list.length) return null;
  const rename = async (l: Layout, name: string) => {
    setEditing(undefined);
    if (!name.trim() || name.trim() === l.name) return;
    await attempt(async () => {
      const next = await cApi.renameLayout(l.id, name.trim());
      setList((xs) => xs.map((x) => (x.id === l.id ? next : x)));
    }, { failed: setErr });
  };
  const remove = (l: Layout) => attempt(async () => {
    await cApi.deleteLayout(l.id);
    setList((xs) => xs.filter((x) => x.id !== l.id));
  }, { failed: setErr });
  const apply = (l: Layout) => {
    cState.patch({ open: { paddockId: paddock.id, layoutId: l.id, at: Date.now() } });
    startTool("t");
  };
  return (
    <ul className="clayouts">
      {list.map((l) => (
        <li key={l.id}>
          {editing === l.id ? (
            <form onSubmit={(e) => { e.preventDefault(); void rename(l, (e.currentTarget.elements.namedItem("n") as HTMLInputElement).value); }}>
              <Input name="n" autoFocus className="sm" defaultValue={l.name} aria-label="Layout name"
                onBlur={(e) => void rename(l, e.target.value)}
                onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); setEditing(undefined); } }} />
            </form>
          ) : (
            <button type="button" className="lname" title="Rename" onClick={() => setEditing(l.id)}>{l.name}</button>
          )}
          {canApply && <Button small onClick={() => apply(l)}>Apply</Button>}
          <Button small kind="plain" onClick={() => void remove(l)}>Delete</Button>
        </li>
      ))}
      {err && <li><span className="mono err">{err}</span></li>}
    </ul>
  );
}

// Copy: a new paddock of the same shape, a little south of this one so both outlines and
// names show, to reshape into what's needed.
export function PaddockCopy({ paddock }: { paddock: Paddock }) {
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string>();
  const copy = () => attempt(async () => {
    await cApi.copyPaddock(paddock.id, { offset_m: [0, -nudgeM(paddock)] });
    await store.refresh();
  }, { busy: setBusy, failed: setErr });
  return (
    <div className="acts ccopy">
      <Button small onClick={() => void copy()} disabled={busy}>Copy</Button>
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}

// How far south the copy goes: 8% of the paddock's north-south extent, 15 to 60 m.
export function nudgeM(p: Paddock): number {
  const ys = p.geometry.coordinates[0].map((q) => toXY(q, q[1])[1]);
  return Math.round(Math.min(60, Math.max(15, (Math.max(...ys) - Math.min(...ys)) * 0.08)));
}
