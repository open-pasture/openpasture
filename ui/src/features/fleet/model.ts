// Fleet's words and numbers, kept pure so they can be tested: states, verdicts, link quality,
// claim codes, the fence distance line and the wire log's lines.

import type { FwState, JudgedStep, Quality, Rollout, TestRun, Unit, UnitState, WireEntry } from "../../api/fleet";

export const STATE_WORDS: Record<UnitState, string> = {
  built: "built", tested: "tested", provisioned: "provisioned", claimed: "claim pending", in_service: "in service", retired: "retired", rma: "RMA",
};

export const FW_WORDS: Record<FwState, string> = {
  current: "current", waiting: "waiting", held: "held", unsupported: "no updates over the air", ready: "ready",
  offered: "installing", confirmed: "confirmed", failed: "failed", rolled_back: "rolled back",
};

// A claim code as printed: four groups of four.
export function grouped(code: string): string {
  return code.replace(/[^0-9A-Za-z]/g, "").toUpperCase().match(/.{1,4}/g)?.join("-") ?? "";
}

// What a typed or scanned claim is: the card's text (OC2:serial:key:code) or a bare code.
export function readClaim(text: string): { card?: string; claim_code?: string } {
  const t = text.trim();
  if (/^OC2:/i.test(t)) return { card: t };
  return t ? { claim_code: t } : {};
}

// One judged step in a line: "2.1 V  (3.2–3.4 V)  below 3.2 V".
export function stepLine(s: JudgedStep): string {
  const unit = s.unit ?? s.plan_unit;
  const value = s.value === undefined ? "" : `${s.value}${unit ? ` ${unit}` : ""}`;
  const limits = s.limit_lo !== undefined || s.limit_hi !== undefined ? `${s.limit_lo ?? ""}–${s.limit_hi ?? ""}${s.plan_unit ? ` ${s.plan_unit}` : ""}` : "";
  return [value, limits && `(${limits})`, s.why ?? ""].filter(Boolean).join("  ");
}

export function pct(rate: number): string {
  return `${Math.round(rate * 1000) / 10} %`;
}

// "stage 2 of 3: the first herd" for a rollout.
export function stageLine(r: Rollout): string {
  const kind = r.waves[r.wave]?.kind;
  const what = kind === "collar" ? "one collar" : kind === "herd" ? "the first herd" : "every herd";
  return `stage ${r.wave + 1} of ${r.waves.length}: ${what}`;
}

export function qualityLine(q: Quality, rssi?: number, snr?: number): string {
  const nums = [rssi !== undefined ? `${rssi} dBm` : "", snr !== undefined ? `SNR ${snr} dB` : ""].filter(Boolean).join(", ");
  return nums ? `${q}  ${nums}` : q;
}

// The collar against its fence: "inside 12 m" or "outside 3 m". Undefined without both.
export function fenceLine(signedM: number | undefined, len: (m: number) => string): string | undefined {
  if (signedM === undefined || !Number.isFinite(signedM)) return undefined;
  return signedM >= 0 ? `inside ${len(signedM)}` : `outside ${len(-signedM)}`;
}

// A wire log line: "12:03:05  in  report  412 B  2 fixes" / "out  report  200  …".
export function wireLine(e: WireEntry): string {
  const t = new Date(e.at).toISOString().slice(11, 19);
  const parts = [t, e.dir.padEnd(3), e.route.padEnd(8), e.status !== undefined ? String(e.status) : "", `${e.bytes} B`, e.ms !== undefined ? `${e.ms} ms` : "", e.note ?? ""];
  return parts.filter(Boolean).join("  ");
}

// Keep the newest `max` wire entries, oldest first, adding a live one.
export function addWire(list: readonly WireEntry[], e: WireEntry, max = 64): WireEntry[] {
  const next = [...list, e];
  return next.length > max ? next.slice(next.length - max) : next;
}

// Seconds as "4h 12m" or "38 s", for uptime and latency.
export function duration(s: number): string {
  const a = Math.abs(s);
  if (a < 90) return `${Math.round(s)} s`;
  if (a < 5400) return `${Math.round(s / 60)} min`;
  const h = Math.floor(a / 3600);
  if (h < 48) return `${s < 0 ? "-" : ""}${h}h ${Math.round((a % 3600) / 60)}m`;
  return `${s < 0 ? "-" : ""}${Math.round(h / 24)} d`;
}

// A further page of units after the ones shown: in order, none twice (a unit can move up
// between pages when it changes).
export function addPage(shown: readonly Unit[], page: readonly Unit[]): Unit[] {
  const seen = new Set(shown.map((u) => u.serial));
  return [...shown, ...page.filter((u) => !seen.has(u.serial))];
}

// "200 of 1,000 units": how much of the list is on screen.
export function shownLine(shown: number, total: number): string {
  const n = (x: number) => x.toLocaleString("en-US");
  if (total === 0) return "no units";
  return shown >= total ? `${n(total)} unit${total === 1 ? "" : "s"}` : `${n(shown)} of ${n(total)} units`;
}

// What happens next to a unit, in a line: one per state, so a unit is never in doubt.
export function nextStep(u: Unit): string {
  switch (u.state) {
    case "built":
      return u.last_test ? "Failed its latest test. Rework it, then test it again." : u.generation > 0 ? "New key: test it again at a station." : "Waiting for its test at a station.";
    case "tested":
      return u.last_test?.result === "pass" ? "Passed. Provision it, then print its label." : "Test it again at a station.";
    case "provisioned":
      return "Ready to claim. Print its label if it has none.";
    case "claimed":
      // Pending until the collar answers under its new key (protocol v2: acks the bind).
      return u.claim_checked ? "Claim pending: it is in service from its first report." : "Claim pending, from a card without a factory record: the collar checks the code when it first reports.";
    case "in_service":
      return "In service on a farm.";
    case "rma":
      return "Back for repair. It needs a fresh passing test; earlier passes don't count.";
    case "retired":
      return "Retired. Send it back (RMA) to use it again.";
  }
}

// Why a run doesn't decide the unit's state, when it doesn't.
export function runNote(r: TestRun, generation: number): string | undefined {
  if (r.generation !== generation) return "before the unit came back";
  if (!r.counted) return r.result === "pass" ? "not counted: arrived after a later run, or finished before the unit came back" : undefined;
  return undefined;
}
