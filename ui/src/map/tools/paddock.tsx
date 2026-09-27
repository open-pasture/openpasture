import { useState } from "react";
import { api, type Paddock } from "../../api";
import { store } from "../../store";
import { Button, Input } from "../../ui";
import { current } from "../drawn";
import { useDrawing, type ToolProps } from "../tools";

// Draw a paddock, name it, save it.
export function PaddockTool({ draw, done }: ToolProps) {
  const { drawn } = useDrawing(draw, "paddock");
  const [name, setName] = useState(() => nextName(store.get().state?.paddocks ?? []));
  const [busy, setBusy] = useState(false);

  const save = async () => {
    const g = current(draw);
    if (!g) return;
    setBusy(true);
    try {
      await api.createPaddock({ name: name.trim() || nextName(store.get().state?.paddocks ?? []), geometry: g });
      done();
      await store.refresh();
    } finally {
      setBusy(false);
    }
  };

  if (!drawn)
    return (
      <>
        <span className="toolhint">Paddock</span>
        <Button small kind="plain" onClick={done}>Cancel</Button>
      </>
    );
  return (
    <form className="toolform" onSubmit={(e) => { e.preventDefault(); void save(); }}>
      <Input autoFocus value={name} onChange={(e) => setName(e.target.value)} aria-label="Name" className="sm" />
      <Button small kind="plain" onClick={done}>Cancel</Button>
      <Button small kind="primary" type="submit" disabled={busy}>Save</Button>
    </form>
  );
}

export function nextName(ps: Paddock[]) {
  let n = ps.length + 1;
  while (ps.some((p) => p.name === `P${n}`)) n++;
  return `P${n}`;
}
