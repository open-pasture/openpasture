// Pure parts of A3's rows (tested with bun test).

import type { User } from "../../api";
import type { PersonTexting, Texting } from "./api";

// Channels that carry a text back to the farm.
const INBOUND = ["sms", "whatsapp", "relay"];
// Channels that can carry a brief to someone.
const OUTBOUND = ["sms", "whatsapp", "email", "relay", "push"];

export interface RepliesLine {
  label: string;
  urls: { label: string; url: string }[];
  error?: string;
}

// The inbound line in Settings > Texting, or null when nothing could text the farm back.
export function repliesLine(t: Texting, configured: readonly string[]): RepliesLine | null {
  if (!configured.some((c) => INBOUND.includes(c))) return null;
  if (!t.inbound) return { label: "Replies", urls: [] };
  switch (t.inbound_mode) {
    case "webhook": {
      const urls = [
        t.hooks?.sms ? { label: "SMS", url: t.hooks.sms } : null,
        t.hooks?.whatsapp ? { label: "WhatsApp", url: t.hooks.whatsapp } : null,
      ].filter((u): u is { label: string; url: string } => u !== null);
      return { label: "Replies by webhook", urls };
    }
    case "polling":
      return { label: `Replies checked every ${t.poll_s} s`, urls: [], error: t.checked?.error };
    case "relay":
      return { label: "Replies through the relay", urls: [], error: t.checked?.error };
    default:
      return null;
  }
}

// Channels a brief can go out on (the server's alert_channels): WhatsApp only with an approved
// template, since Twilio lets a business start a WhatsApp conversation only with one.
export const briefChannels = (c: { configured: readonly string[]; whatsapp: { template_sid?: string } }) =>
  c.configured.filter((k) => k !== "whatsapp" || !!c.whatsapp.template_sid);

// The brief time beside Daily shows once something can carry it.
export const canBrief = (configured: readonly string[]) => configured.some((c) => OUTBOUND.includes(c));

// A person can get the brief: a verified phone over a text channel, an email over the farm's own
// email (the relay texts only phones proven to it), or a browser of theirs that takes notifications
// (`push`, from their texting row) while push is a farm channel.
export function reachable(u: User, configured: readonly string[], push = false): boolean {
  if (u.disabled_at) return false;
  const phone = !!u.phone && !!u.phone_verified_at && ["sms", "whatsapp", "relay"].some((c) => configured.includes(c));
  const email = !!u.email && configured.includes("email");
  return phone || email || (push && configured.includes("push"));
}

export const personOf = (t: Texting, userId: string): PersonTexting =>
  t.people.find((p) => p.user_id === userId) ?? { user_id: userId, brief: false, sms_opt_out: false, push: false };

// HH:MM, 00:00 to 23:59.
export const validTime = (s: string) => /^([01]\d|2[0-3]):[0-5]\d$/.test(s);

export function withPerson(t: Texting, p: PersonTexting): Texting {
  const people = t.people.some((x) => x.user_id === p.user_id) ? t.people.map((x) => (x.user_id === p.user_id ? p : x)) : [...t.people, p];
  return { ...t, people };
}
