// Texting in (A3): how replies reach the farm, the morning brief by text, each person's
// brief. See docs/API.md "Texts in and the morning brief".

import { get, put } from "../../api/http";

export type InboundMode = "webhook" | "polling" | "relay" | "off";

// The `texting` setting.
export interface TextingConfig {
  inbound: boolean;
  poll_s: number;
  approve_window_h: number;
  brief: { enabled: boolean; time: string };
}

// The last check of Twilio (polling) or of the relay's inbox.
export interface Checked { at?: string; ok_at?: string; error?: string }
export interface PersonTexting { user_id: string; brief: boolean; sms_opt_out: boolean }

// GET /api/texting: the setting, plus how texts come in now (read-only).
export interface Texting extends TextingConfig {
  inbound_mode: InboundMode;
  hooks?: { sms?: string; whatsapp?: string };
  checked?: Checked;
  people: PersonTexting[];
}

export type TextingPatch = Partial<Pick<TextingConfig, "inbound" | "poll_s" | "approve_window_h">> & { brief?: Partial<TextingConfig["brief"]> };

export const textingApi = {
  get: () => get<Texting>("/api/texting"),
  save: (p: TextingPatch) => put<Texting>("/api/texting", p),
  setBrief: (userId: string, brief: boolean) => put<PersonTexting>(`/api/texting/people/${encodeURIComponent(userId)}`, { brief }),
};
