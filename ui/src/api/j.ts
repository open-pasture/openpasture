// People, sign-in links and a person's own token (docs/API.md, "People, roles and sign-in").

import type { Actor, Role, User } from "../api";
import { del, get, patch, post } from "./http";

// A person as Settings > People shows them.
export interface Person extends User {
  tokens: number; // browsers signed in
  last_used?: string;
  invite_until?: string; // an open sign-in link stops working then
}

export interface Invite {
  id: string; user_id: string; name: string; role: Role; phone?: string; email?: string;
  created_by?: Actor; created_at: string; expires_at: string; accepted_at?: string;
}
// `code` and `url` come back this once.
export interface CreatedInvite extends Invite { code: string; url: string }

export interface TokenInfo { id: string; user_id: string; label: string; created_at: string; last_used?: string; revoked_at?: string }

export interface Accepted { token: string; user: User }

export interface NewPerson { name: string; role: Role; phone?: string; email?: string }
export interface PersonPatch { name?: string; role?: Role; phone?: string | null; email?: string | null; disabled?: boolean }
export interface ProfilePatch { name?: string; phone?: string | null; email?: string | null }

export const people = {
  list: () => get<Person[]>("/api/users"),
  add: (b: NewPerson) => post<Person>("/api/users", b),
  update: (id: string, b: PersonPatch) => patch<Person>(`/api/users/${id}`, b),
  remove: (id: string) => del(`/api/users/${id}`),
  // Every browser signed out, the open link dropped.
  revoke: (id: string) => post<Person>(`/api/users/${id}/revoke`),
  // A new sign-in link for someone in People (replaces their open one).
  link: (userId: string) => post<CreatedInvite>("/api/invites", { user_id: userId }),
  accept: (code: string) => post<Accepted>("/api/invites/accept", { code }),
  // Your own.
  profile: (b: ProfilePatch) => patch<User>("/api/me/profile", b),
  signout: () => post<void>("/api/me/signout"),
};
