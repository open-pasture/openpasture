// Who this browser is, from GET /api/me. Registries hide items below the role.

import { api, ApiError, type Me, type Role } from "../api";
import { createSlice } from "./slice";

const RANK: Record<Role, number> = { viewer: 0, hand: 1, manager: 2, owner: 3 };

// Whether `role` may do what `need` may.
export const atLeast = (role: Role, need: Role = "viewer") => RANK[role] >= RANK[need];

// null until the first answer.
export const me = createSlice<Me | null>(null);

export async function loadMe() {
  try {
    me.set(await api.me());
  } catch (e) {
    // A server from before roles has no /api/me and gives everyone the whole app.
    if (e instanceof ApiError && e.status === 404) me.set({ role: "owner", via: "local" });
    else if (!me.get()) throw e;
  }
}

// Before /api/me answers nothing role-gated shows: the app waits for it at boot.
export const role = (): Role | undefined => me.get()?.role;
export const can = (need: Role = "viewer") => {
  const r = role();
  return r !== undefined && atLeast(r, need);
};

export const useMe = () => me.use((m) => m);
// The person this browser acts as (a person token, or the owner who added themselves to People).
export const useMeUser = () => me.use((m) => m?.user);
export const useVia = () => me.use((m) => m?.via);
export const useRole = () => me.use((m) => m?.role);
export const useCan = (need: Role = "viewer") => me.use((m) => (m ? atLeast(m.role, need) : false));
