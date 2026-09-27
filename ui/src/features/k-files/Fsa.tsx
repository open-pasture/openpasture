// Paddock sheet: FSA farm, tract and field numbers, when the paddock came from a file that had them.

import type { Paddock } from "../../api";
import { fsaLines } from "./logic";

export function FsaLines({ paddock }: { paddock: Paddock; herdId?: string }) {
  const lines = fsaLines(paddock.props);
  if (!lines.length) return null;
  return (
    <ul className="kv kfiles-fsa">
      {lines.map(([label, v]) => <li key={label}><span>{label}</span><b>{v}</b></li>)}
    </ul>
  );
}
