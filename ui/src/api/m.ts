// M: Web Push (/api/push*). A browser subscribes with the farm's VAPID key; alerts and the brief
// then reach it as notifications. Works only while the server is reached over https.

import { del, get, post, put } from "./http";

export interface MySubscription { id: string; endpoint: string; created_at: string; last_ok?: string }
export interface PushView {
  // Served over https and switched on: browsers may subscribe.
  available: boolean;
  enabled: boolean;
  reason?: string;
  vapid_public_key?: string;
  key_set: boolean;
  mine: MySubscription[];
}
export interface PushTest { ok: boolean; detail: string }

export const pushApi = {
  get: () => get<PushView>("/api/push"),
  subscribe: (s: PushSubscriptionJSON) => post<MySubscription>("/api/push/subscriptions", s),
  remove: (id: string) => del(`/api/push/subscriptions/${encodeURIComponent(id)}`),
  test: (id: string) => post<PushTest>(`/api/push/subscriptions/${encodeURIComponent(id)}/test`),
  settings: (b: { enabled?: boolean; new_keys?: boolean }) => put<PushView>("/api/push/settings", b),
};
