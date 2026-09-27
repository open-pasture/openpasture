// Texting and the message log (A-notify): channels, tests, phone verification, the
// relay host's settings. See docs/API.md "Texting and delivery".

import type { MessageLog } from "../api";
import { get, post, put } from "./http";

export type ChannelKind = "sms" | "whatsapp" | "email" | "webhook" | "relay";
export type Tls = "starttls" | "tls" | "none";
export type NotifySecret = "twilio_account_sid" | "twilio_auth_token" | "smtp_password" | "webhook_secret" | "hosted_url" | "hosted_api_key";

// The notify.channels setting. Absent strings are not set.
export interface ChannelsConfig {
  sms: { from?: string };
  whatsapp: { from?: string; template_sid?: string };
  email: { host?: string; port: number; user?: string; from?: string; tls: Tls };
  webhook: { url?: string };
  // enabled only after the relay answered GET /v1/notify/recipients with 200.
  relay: { enabled: boolean; checked_at?: string };
  twilio_api_base: string;
}

// GET /api/notify/channels: the config, which secrets are set (never their values), and
// the channels that can send now.
export interface Channels extends ChannelsConfig {
  secrets: { name: NotifySecret; set: boolean }[];
  configured: ChannelKind[];
}

// PUT /api/notify/channels: a merge patch; null clears. Secrets: a value sets, null removes.
export interface ChannelsPatch {
  sms?: { from?: string | null };
  whatsapp?: { from?: string | null; template_sid?: string | null };
  email?: { host?: string | null; port?: number; user?: string | null; from?: string | null; tls?: Tls };
  webhook?: { url?: string | null };
  relay?: { enabled: boolean };
  secrets?: Partial<Record<NotifySecret, string | null>>;
}

export interface TestResult { ok: boolean; detail: string }
// via: the farm's own SMS, or the relay (which texts its own code). verified: the relay
// already knew the number, so no code was needed.
export interface CodeSent { via: "sms" | "relay"; verified?: boolean }
export interface Verified { verified: true; phone_verified_at: string }
// notify.hosting: this server relaying texts for others' keys.
export interface Hosting { enabled: boolean; per_key_minute: number; per_key_day: number; deadman_after_min: number }
export interface MessagesQuery { direction?: "in" | "out"; limit?: number; from?: string; to?: string; user_id?: string }

export const notifyApi = {
  channels: () => get<Channels>("/api/notify/channels"),
  saveChannels: (p: ChannelsPatch) => put<Channels>("/api/notify/channels", p),
  // Without `to`, Twilio checks the account and the relay lists its recipients.
  test: (channel: ChannelKind, to?: string) => post<TestResult>("/api/notify/test", to ? { channel, to } : { channel }),
  sendCode: (user_id: string) => post<CodeSent>("/api/notify/verify", { user_id }),
  confirmCode: (user_id: string, code: string) => post<Verified>("/api/notify/verify/confirm", { user_id, code }),
  messages: (q?: MessagesQuery) => get<MessageLog[]>("/api/messages", q ? { ...q } : undefined),
  hosting: () => get<Hosting>("/api/notify/hosting"),
  saveHosting: (p: Partial<Hosting>) => put<Hosting>("/api/notify/hosting", p),
};
