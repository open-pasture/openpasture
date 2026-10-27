// Fleet (op-fleet): units from the factory to the farm, factory tests, provisioning and claim,
// firmware rollout, the herd network and the collar console. Types mirror docs/api/fleet.md.

import { ApiError, get, post, put, req } from "./http";
import type { Collar, Role } from "../api";

declare module "../api" {
  interface LiveEvents {
    // Manager and owner sockets only.
    fleet_unit: { unit: Unit };
    fleet_test: { run: TestRun };
    // One exchange on /collar/v1, as the wire log keeps it (op-ingest).
    collar_wire: { collar_id: string; entry: WireEntry };
  }
}

export type UnitState = "built" | "tested" | "provisioned" | "claimed" | "in_service" | "retired" | "rma";
export interface Radios { lora_eui?: string; imei?: string; iccid?: string; ble_addr?: string }
export interface Unit {
  serial: string; device_class: string; hw_rev: string; fw_version: string;
  device_pubkey: string; device_kid: string; device_id: string;
  radios: Radios; image_keys?: string[]; caps?: string[];
  state: UnitState; origin: "station" | "card"; station?: string; claim_code?: string;
  // Goes up on RMA, retire or a new key; only runs of this generation count.
  generation: number; generation_at?: string;
  // The claim code was checked here (drawn here, or matched the factory record).
  claim_checked: boolean;
  collar_id?: string; herd_id?: string;
  // The run that decides provisioning: latest by the server's order, this generation, counted.
  last_test?: { id: string; result: "pass" | "fail"; plan_id: string; plan_version: number; finished_at: string; seq: number };
  maker?: string; maker_kid?: string; maker_verified: boolean; made_at?: string;
  provisioned_at?: string; claimed_at?: string; created_at: string; updated_at: string;
}
export interface RecordView { cose: string; record: Record<string, unknown>; maker_kid: string; verified: boolean }
export interface UnitEvent { at: string; kind: string; detail: Record<string, unknown>; by?: string }
export interface UnitPage { units: Unit[]; total: number; next?: string }
export interface UnitDetail {
  unit: Unit; factory_record?: RecordView; runs: TestRun[]; events: UnitEvent[];
  keys: { device_pubkey: string; device_kid: string; from: string; to?: string }[];
}

export type StepKind = "measure" | "check" | "record";
export interface PlanStep { name: string; kind: StepKind; limit_lo?: number; limit_hi?: number; unit?: string; required: boolean; note?: string }
export interface TestPlan { id: string; version: number; device_class: string; steps: PlanStep[]; shipped: boolean; created_at: string }

export type Verdict = "pass" | "fail" | "skip" | "missing" | "extra";
export interface JudgedStep {
  name: string; status?: "pass" | "fail" | "skip"; value?: number | string | boolean; unit?: string; detail?: string;
  verdict: Verdict; why?: string; kind?: StepKind; limit_lo?: number; limit_hi?: number; plan_unit?: string; required: boolean;
}
export interface TestRun {
  id: string; serial: string; plan_id: string; plan_version: number; station?: string; fixture_id?: string;
  started_at: string; finished_at: string; received_at: string; result: "pass" | "fail"; failed: string[]; steps: JudgedStep[];
  // seq: the server's order. counted: false for a late pass or one from before the unit came back.
  seq: number; generation: number; counted: boolean; run_key?: string;
}

export interface Provisioning {
  serial: string; device_id: string; server_key: string; server_kid: string; endpoint?: string;
  link: { transport: string; lora_region: string; report_s: number; poll_s: number };
  claim_code: string; card: string; factory_record?: RecordView;
}
export interface Claimed {
  unit: Unit; collar: Collar;
  v1: { key: string; public_key: string; endpoint?: string; line?: string; why?: string };
  code_checked: boolean; reclaimed?: boolean;
}

export interface Rate { units: number; passed: number; rate: number }
export interface StepYield {
  name: string; required: boolean; runs: number; pass: number; fail: number; skip: number; missing: number; rate: number;
  values?: { n: number; min: number; p50: number; max: number; mean: number; unit?: string };
}
export interface Yield {
  plan_id: string; plan_version: number; runs: number; units: number; first_pass: Rate; final: Rate; steps: StepYield[]; failures: { name: string; fail: number }[];
  // Runs judged by other versions of the plan: never in the numbers above.
  other_versions: { plan_version: number; runs: number }[];
}
export interface Throughput {
  from: string; to: string; bucket: "hour" | "day";
  stations: { station: string; runs: number; units: number; pass: number; fail: number; cycle_s_p50?: number; buckets: { at: string; runs: number; pass: number; fail: number }[] }[];
}

export interface Station { id: string; name: string; created_at: string; last_used_at?: string; revoked_at?: string }
export interface ProvisioningSettings { lora_region: string; maker?: string; maker_kid: string; maker_key: string }

export interface SigningKey { kid: string; name: string; public_key: string; kind: "vendor" | "owner"; purpose: "image" | "maker"; created_at?: string; revoked_at?: string }
export interface Firmware {
  id: string; version: string; classes: string[]; hw_revs: string[]; sha256: string; size: number; signer_kid: string;
  security_counter: number; min_battery?: number; revoke?: string[]; channel: "stable" | "beta"; notes?: string; manifest: string;
  has_image: boolean; created_at: string; withdrawn_at?: string; signer?: { kid: string; name: string; kind: "vendor" | "owner" };
}
export interface FirmwarePolicy { mode: "auto" | "paused"; channel: "stable" | "beta"; battery_floor: number; wave_hours: number; held_herds?: string[] }
export interface Wave { kind: "collar" | "herd" | "all"; after_h: number; started_at?: string; collar_id?: string; herd_id?: string }
export interface Rollout {
  id: string; firmware_id: string; version: string; classes: string[];
  state: "active" | "paused" | "done" | "cancelled"; paused_reason?: string; waves: Wave[]; wave: number;
  counts: { targeted: number; waiting: number; held: number; ready: number; offered: number; confirmed: number; failed: number; rolled_back: number; unsupported: number };
  created_at: string; created_by?: string;
}
export type FwState = "current" | "waiting" | "held" | "unsupported" | "ready" | "offered" | "confirmed" | "failed" | "rolled_back";
export interface UnitFirmware { serial?: string; collar_id: string; name: string; herd_id?: string; current?: string; target?: string; rollout_id?: string; state: FwState; reason?: string; updated_at: string }

export type NetRole = "anchor" | "node" | "direct" | "unknown";
export type Quality = "good" | "fair" | "poor" | "lost" | "unknown";
export interface Member { collar_id: string; name: string; role: NetRole; serial?: string; last_heard_at?: string; via?: string; rssi_dbm?: number; snr_db?: number; quality: Quality }
export interface HerdNetwork {
  herd_id: string; active_anchor?: string;
  anchors: { id: string; name: string; source: "collar" | "fixed"; active: boolean; last_beacon_at?: string; battery?: number; lte_rsrp_dbm?: number; serial?: string }[];
  members: Member[];
  gaps: { collar_id: string; name: string; since: string; point?: [number, number] }[];
}
export interface FixedAnchor { id: string; name: string; point: [number, number]; serial?: string; herd_ids: string[]; created_at: string }

export interface CollarConsole {
  collar: Collar; unit?: Unit;
  contact: { state: "waiting" | "heard"; since: string; endpoint?: string; why?: string; last_seen?: string; latency_s?: number };
  health?: {
    at: string; battery?: number; battery_v?: number; charging?: boolean; temp_c?: number; uptime_s?: number; reset?: string;
    still_s?: number; tilt_deg?: number; fix_attempts?: number; fix_ok?: number;
    cell?: { rsrp_dbm?: number; rsrq_db?: number; snr_db?: number; mode?: string; band?: number; cell_id?: string; tac?: number };
  };
  fix?: { at: string; point: [number, number]; accuracy_m: number; sats: number; cn0?: number; hdop?: number; ttf_s?: number };
  acks: { at: string; received_at: string; version: number; status: string; code?: string; reason?: string; command_id: string }[];
  cues: { at: string; kind: string; level: number; margin_m: number; ring?: number; dur_ms?: number; boundary_version?: number }[];
  firmware?: UnitFirmware;
  network?: Member;
}

// The collar bridge's wire log (op-ingest).
export interface WireEntry { at: string; dir: "in" | "out"; route: string; status?: number; bytes: number; body: string; truncated?: boolean; note?: string; ms?: number }

const enc = encodeURIComponent;
const unitPath = (serial: string, rest = "") => `/api/fleet/units/${enc(serial)}${rest}`;

export const fleet = {
  units: (q: { state?: string; device_class?: string; station?: string; q?: string; limit?: number; cursor?: string } = {}) => get<UnitPage>("/api/fleet/units", q),
  unit: (serial: string) => get<UnitDetail>(unitPath(serial)),
  setState: (serial: string, state: "retired" | "rma", note?: string) => post<Unit>(unitPath(serial, "/state"), { state, note }),
  provision: (serial: string, b: { claim_code?: string; rotate_claim?: boolean } = {}) => post<Provisioning>(unitPath(serial, "/provision"), b),
  label: (serial: string) => get<string>(unitPath(serial, "/label.svg")),
  claim: (b: { claim_code?: string; card?: string; factory_record?: string; herd_id: string; name?: string; animal_id?: string }) => post<Claimed>("/api/fleet/claim", b),
  summary: () => get<{ total: number; by_state: Record<string, number>; by_class: Record<string, number> }>("/api/fleet/summary"),

  plans: () => get<TestPlan[]>("/api/fleet/test-plans"),
  plan: (cls: string, version?: number) => get<TestPlan>(`/api/fleet/test-plans/${enc(cls)}`, { version }),
  savePlan: (cls: string, steps: PlanStep[]) => put<TestPlan>(`/api/fleet/test-plans/${enc(cls)}`, { steps }),
  yield: (q: { plan_id: string; plan_version?: number; from?: string; to?: string; station?: string }) => get<Yield>("/api/fleet/yield", q),
  throughput: (q: { from?: string; to?: string; bucket?: "hour" | "day" } = {}) => get<Throughput>("/api/fleet/throughput", q),

  stations: () => get<Station[]>("/api/fleet/stations"),
  addStation: (name: string) => post<{ station: Station; token: string }>("/api/fleet/stations", { name }),
  revokeStation: (id: string) => post<Station>(`/api/fleet/stations/${enc(id)}/revoke`),
  provisioning: () => get<ProvisioningSettings>("/api/fleet/provisioning"),
  saveProvisioning: (b: { lora_region: string; maker?: string }) => put<ProvisioningSettings>("/api/fleet/provisioning", b),

  firmware: () => get<Firmware[]>("/api/fleet/firmware"),
  addRelease: (b: { manifest: string; channel?: string; notes?: string }) => post<Firmware>("/api/fleet/firmware", b),
  uploadImage: (id: string, file: Blob) => req<Firmware>("PUT", `/api/fleet/firmware/${enc(id)}/image`, file, undefined, { "Content-Type": "application/octet-stream" }),
  withdraw: (id: string) => post<Firmware>(`/api/fleet/firmware/${enc(id)}/withdraw`),
  policy: () => get<FirmwarePolicy>("/api/fleet/firmware/policy"),
  savePolicy: (p: FirmwarePolicy) => put<FirmwarePolicy>("/api/fleet/firmware/policy", p),
  rollouts: () => get<Rollout[]>("/api/fleet/firmware/rollouts"),
  startRollout: (firmware_id: string) => post<Rollout>("/api/fleet/firmware/rollouts", { firmware_id }),
  rolloutAct: (id: string, action: "pause" | "resume" | "cancel") => post<Rollout>(`/api/fleet/firmware/rollouts/${enc(id)}/${action}`),
  fwUnits: (q: { herd_id?: string; rollout_id?: string } = {}) => get<UnitFirmware[]>("/api/fleet/firmware/units", q),
  keys: () => get<SigningKey[]>("/api/fleet/signing-keys"),
  addKey: (b: { name: string; public_key: string; purpose: "image" | "maker" }) => post<SigningKey>("/api/fleet/signing-keys", b),
  revokeKey: (kid: string) => post<SigningKey>(`/api/fleet/signing-keys/${enc(kid)}/revoke`),

  network: (herd_id: string) => get<HerdNetwork>(`/api/fleet/network/${enc(herd_id)}`),
  anchors: () => get<FixedAnchor[]>("/api/fleet/anchors"),
  addAnchor: (b: { name: string; point: [number, number]; serial?: string; herd_ids?: string[] }) => post<FixedAnchor>("/api/fleet/anchors", b),
  console: (collar_id: string) => get<CollarConsole>(`/api/fleet/collars/${enc(collar_id)}/console`),
};

// The wire log, when this server keeps one: entries oldest first, or null when it has none
// (an older server answers 404 for the route itself).
export async function wireLog(collar_id: string): Promise<WireEntry[] | null> {
  try {
    const v = await get<WireEntry[] | { entries: WireEntry[] }>(`/api/collars/${enc(collar_id)}/wire`);
    return Array.isArray(v) ? v : v.entries;
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) {
      // 404 also means "no such collar"; the console already shows that one exists.
      return null;
    }
    throw e;
  }
}

export const FLEET_ROLE: Role = "manager";
