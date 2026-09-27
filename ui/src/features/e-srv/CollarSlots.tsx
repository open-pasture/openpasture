import { useEffect, useState } from "react";
import type { Animal, Collar } from "../../api";
import { esrv, type CollarSlots } from "../../api/e-srv";
import { slotsLine } from "./format";

// Animal page: the collar's firmware and the boundaries it holds, "fw 0.2.0  slots 57 58 59".
// Refetched when the collar reports or acks (its last contact or boundary moves).
export function CollarSlotsLine({ collar }: { animal?: Animal; collar?: Collar }) {
  const [s, setS] = useState<CollarSlots>();
  const id = collar?.id;
  useEffect(() => {
    if (!id) return;
    let live = true;
    esrv.collarSlots(id).then((v) => live && setS(v)).catch(() => live && setS(undefined));
    return () => void (live = false);
  }, [id, collar?.last_seen, collar?.boundary_version]);
  const line = s && s.collar_id === id ? slotsLine(s) : null;
  return line ? <div className="mono esrv-slots">{line}</div> : null;
}
