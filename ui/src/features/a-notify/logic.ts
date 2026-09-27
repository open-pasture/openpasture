// Pure parts of Settings > Texting, Verify and Data > Messages (tested with bun test).

import type { Me, MessageLog, User } from "../../api";
import type { ChannelKind, Channels, ChannelsPatch, NotifySecret, Tls } from "../../api/a-notify";

export type Segment = "twilio" | "email" | "webhook" | "relay";
export const SEGMENTS: { value: Segment; label: string }[] = [
  { value: "twilio", label: "Twilio" },
  { value: "email", label: "Email" },
  { value: "webhook", label: "Webhook" },
  { value: "relay", label: "Relay" },
];

// The channels a segment sets up.
const KINDS: Record<Segment, ChannelKind[]> = { twilio: ["sms", "whatsapp"], email: ["email"], webhook: ["webhook"], relay: ["relay"] };

export const isOn = (seg: Segment, configured: readonly string[]) => KINDS[seg].some((k) => configured.includes(k));

// The first segment that can send, else Twilio.
export const firstSegment = (configured: readonly string[]): Segment => SEGMENTS.find((s) => isOn(s.value, configured))?.value ?? "twilio";

// What a segment's Test sends on.
export const testChannel = (seg: Segment, configured: readonly string[]): ChannelKind =>
  seg === "twilio" ? (configured.includes("sms") || !configured.includes("whatsapp") ? "sms" : "whatsapp") : KINDS[seg][0];

// Who a segment's Test goes to at first: the reader's own phone or email.
export function defaultTo(seg: Segment, me: Me | null): string {
  if (seg === "email") return me?.user?.email ?? "";
  if (seg === "twilio" || seg === "relay") return me?.user?.phone ?? "";
  return "";
}

// ---- form fields --------------------------------------------------------------------------

export interface Field {
  id: string;
  label: string;
  // Placeholder while nothing is stored (else the label).
  hint?: string;
  // Settings value at [section, key], or a secret (never shown, only whether it is set).
  path?: [keyof ChannelsPatch, string];
  secret?: NotifySecret;
  plain?: boolean;
  numeric?: boolean;
}

export const FIELDS: Record<Segment, Field[]> = {
  twilio: [
    { id: "sid", label: "Account SID", secret: "twilio_account_sid" },
    { id: "token", label: "Auth token", secret: "twilio_auth_token" },
    { id: "sms", label: "SMS number", path: ["sms", "from"] },
    { id: "wa", label: "WhatsApp number", path: ["whatsapp", "from"] },
    { id: "template", label: "WhatsApp template SID", path: ["whatsapp", "template_sid"] },
  ],
  email: [
    { id: "host", label: "Mail server", path: ["email", "host"] },
    { id: "port", label: "Port", path: ["email", "port"], numeric: true },
    { id: "user", label: "User", path: ["email", "user"] },
    { id: "password", label: "Password", secret: "smtp_password" },
    { id: "from", label: "From address", path: ["email", "from"] },
  ],
  webhook: [
    { id: "url", label: "Webhook URL", path: ["webhook", "url"] },
    { id: "secret", label: "Signing secret", secret: "webhook_secret" },
  ],
  relay: [
    { id: "url", label: "Relay URL", hint: "https://api.openpasture.dev", secret: "hosted_url", plain: true },
    { id: "key", label: "Relay key", hint: "oph_ key", secret: "hosted_api_key" },
  ],
};

export type Draft = Record<string, string>;

function current(c: Channels, f: Field): string {
  if (!f.path) return "";
  const [section, key] = f.path;
  const v = (c as unknown as Record<string, Record<string, unknown>>)[section]?.[key];
  return v === undefined || v === null ? "" : String(v);
}

export const isSet = (c: Channels, name: NotifySecret) => c.secrets.some((s) => s.name === name && s.set);

// What an empty input says: a stored secret says so under its name.
export const placeholder = (f: Field, c: Channels) => (f.secret && isSet(c, f.secret) ? `${f.label}  saved` : (f.hint ?? f.label));

// What the form shows first: stored values; secrets empty (their placeholder says saved).
export function initialDraft(seg: Segment, c: Channels): Draft {
  const d: Draft = {};
  for (const f of FIELDS[seg]) d[f.id] = f.secret ? "" : current(c, f);
  if (seg === "email") d.tls = c.email.tls;
  return d;
}

// The PUT for what changed, or null when nothing did. A cleared value is removed; a secret
// is only sent when typed.
export function patchFor(seg: Segment, draft: Draft, c: Channels): ChannelsPatch | null {
  const p: Record<string, Record<string, unknown>> = {};
  const put = (section: string, key: string, v: unknown) => ((p[section] ??= {})[key] = v);
  for (const f of FIELDS[seg]) {
    const v = (draft[f.id] ?? "").trim();
    if (f.secret) {
      if (v) put("secrets", f.secret, v);
    } else if (f.path && v !== current(c, f)) {
      const [section, key] = f.path;
      if (f.numeric) {
        const n = Number(v);
        if (v && Number.isFinite(n)) put(section, key, Math.round(n));
      } else put(section, key, v || null);
    }
  }
  if (seg === "email" && draft.tls && draft.tls !== c.email.tls) put("email", "tls", draft.tls as Tls);
  return Object.keys(p).length ? (p as ChannelsPatch) : null;
}

// ---- people -------------------------------------------------------------------------------

// A phone can be proven through the farm's own SMS or through the relay.
export const canVerify = (configured: readonly string[]) => configured.includes("sms") || configured.includes("relay");

export const needsVerify = (u: User, configured: readonly string[]) => !!u.phone && !u.phone_verified_at && !u.disabled_at && canVerify(configured);

// "123 456" and "123-456" are 123456.
export const cleanCode = (s: string) => s.replace(/[\s-]/g, "").slice(0, 6);

// ---- messages -----------------------------------------------------------------------------

// "+1 515 555 0123" for North American numbers; others as stored.
export function fmtPhone(s: string): string {
  const m = /^\+1(\d{3})(\d{3})(\d{4})$/.exec(s);
  return m ? `+1 ${m[1]} ${m[2]} ${m[3]}` : s;
}

// "06:12" today, "Sep 27 06:12" this year, "2025-09-27" before.
export function fmtWhen(iso: string, now = Date.now()): string {
  const d = new Date(iso);
  const n = new Date(now);
  const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  if (d.toDateString() === n.toDateString()) return hm;
  if (d.getFullYear() === n.getFullYear()) return `${d.toLocaleString("en-US", { month: "short" })} ${d.getDate()} ${hm}`;
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
}

// A live message into the list: replaced where it is, else at the top (newest first).
export function upsertMessage(rows: MessageLog[], m: MessageLog): MessageLog[] {
  const i = rows.findIndex((r) => r.id === m.id);
  if (i >= 0) {
    const next = rows.slice();
    next[i] = m;
    return next;
  }
  return [m, ...rows];
}

// What the status cell says: a failure says why.
export function statusText(m: MessageLog): { text: string; tone?: "ok" | "err" | "dim" } {
  if (m.status === "failed") return { text: m.error ?? "failed", tone: "err" };
  if (m.status === "delivered") return { text: "delivered", tone: "ok" };
  if (m.status === "queued" || m.status === "sending" || m.status === "ignored") return { text: m.status, tone: "dim" };
  return { text: m.status };
}
