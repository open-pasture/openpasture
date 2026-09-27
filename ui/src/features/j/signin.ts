// Pure helpers for People and sign-in (tested in signin.test.ts).

import type { Me, Role } from "../../api";

export const ROLES: Role[] = ["viewer", "hand", "manager", "owner"];
export const ROLE_LABEL: Record<Role, string> = { viewer: "Viewer", hand: "Hand", manager: "Manager", owner: "Owner" };

// "+15155550123" → "+1 515 555 0123". Numbers outside North America stay as stored.
export function formatPhone(e164?: string): string {
  if (!e164) return "";
  const m = /^\+1(\d{3})(\d{3})(\d{4})$/.exec(e164);
  return m ? `+1 ${m[1]} ${m[2]} ${m[3]}` : e164;
}

// The code in a pasted sign-in link ("https://farm/#/join/<code>", "#/join/<code>").
export function joinCode(text: string): string | undefined {
  const m = /#\/join\/([0-9a-f]{16,128})\b/i.exec(text.trim());
  return m?.[1].toLowerCase();
}

// A field edited inline: what to send, or nothing when it didn't change. An emptied phone or
// email is cleared (null); a name can't be emptied.
export function fieldChange(key: "name" | "phone" | "email", typed: string, stored?: string): Record<string, string | null> | undefined {
  const v = typed.trim();
  if (key === "name") return v && v !== stored ? { name: v } : undefined;
  if (key === "phone" && v && formatPhone(stored) === v) return undefined;
  if (v === (stored ?? "")) return undefined;
  return { [key]: v || null };
}

// Whether this browser should offer the "You" row: people who aren't the owner edit themselves
// there (the owner does it in People), and anyone signed in with their own token signs out there.
export function showYou(me: Me | null): boolean {
  if (!me?.user) return false;
  return me.role !== "owner" || me.via === "user_token";
}
