// What the server is and can do, from GET /api/server: its address, the collar address,
// its capabilities (clients ask instead of probing) and the work it's in the middle of.

import { api, type ServerInfo } from "../api";
import { createSlice } from "./slice";

export const server = createSlice<ServerInfo | null>(null);

let loading: Promise<void> | undefined;
export function loadServer(force = false): Promise<void> {
  if (loading && !force) return loading;
  loading = api.server().then(
    (s) => server.set(s),
    () => {
      loading = undefined; // asked again next time
    },
  );
  return loading;
}

// Whether the server says it can; false until it has answered.
export const hasCapability = (info: ServerInfo | null, cap: string) => !!info?.capabilities?.includes(cap);

export function useServer(): ServerInfo | null {
  const s = server.use((v) => v);
  if (!s) void loadServer();
  return s;
}

export const useCapability = (cap: string) => hasCapability(useServer(), cap);
