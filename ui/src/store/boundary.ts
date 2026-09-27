// What the map and the herd panel read from a herd's boundary status (pure, for tests).

import type { AckStatus, Boundary, BoundaryStatus } from "../api";

// The staged boundary that takes effect next: the earliest still to come, drawn dashed. Not
// `pending`, which is the last one staged (days out while a strip schedule runs); older
// servers without `staged` have only that.
export const nextStaged = (b?: BoundaryStatus): Boundary | undefined => b?.staged?.[0] ?? b?.pending;

export interface AckLine {
  version: number;
  // Collars of `of` that apply it (hold it, when `held`).
  n: number;
  of: number;
  // Nothing is in effect yet: the line is the next staged boundary and who holds it.
  held: boolean;
}

// The herd panel's ack line: the boundary in effect and how many of these collars apply it.
// A collar's last ack is often for a boundary staged since (a schedule stages days ahead), so
// the slot counts fetched with the status count too; live acks count as they arrive. Before
// any boundary is in effect, the next staged one and how many hold it.
export function ackLine(b: BoundaryStatus | undefined, collarIds: ReadonlySet<string>): AckLine | undefined {
  const cur = b?.active ?? nextStaged(b);
  if (!b || !cur || collarIds.size === 0) return undefined;
  const held = !b.active;
  const counts = (s: AckStatus) => s === "applied" || (held && s === "received");
  const live = b.acks.filter((a) => a.version === cur.version && counts(a.status) && collarIds.has(a.collar_id)).length;
  const c = b.slots?.find((s) => s.version === cur.version);
  const slot = c ? c.applied + (held ? c.stored : 0) : 0;
  return { version: cur.version, n: Math.min(collarIds.size, Math.max(live, slot)), of: collarIds.size, held };
}
