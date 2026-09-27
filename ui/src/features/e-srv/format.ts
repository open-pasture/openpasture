import type { CollarSlots } from "../../api/e-srv";

// "fw 0.2.0  slots 57 58 59": firmware and the herd versions a collar holds, in order.
// A copy handed back after an escape counts as the version it copies; refused ones aren't held.
// Nothing to say (a collar that never reported) gives null.
export function slotsLine(s: CollarSlots): string | null {
  const held = [...new Set(s.slots.filter((x) => x.status !== "rejected").map((x) => x.copy_of ?? x.version))].sort((a, b) => a - b);
  const parts = [s.fw ? `fw ${s.fw}` : "", held.length ? `slots ${held.join(" ")}` : ""].filter(Boolean);
  return parts.length ? parts.join("  ") : null;
}

// The cadence row's numbers: whole seconds between 10 and 3600, else null (the collar would refuse it).
export function seconds(text: string): number | null {
  if (!/^\s*\d+\s*$/.test(text)) return null;
  const n = Number(text);
  return n >= 10 && n <= 3600 ? n : null;
}
