// The farm's texting channels, loaded once and shared by Settings > Texting and every
// person's Verify (one request, however many people).

import { useEffect } from "react";
import { notifyApi, type Channels } from "../api/a-notify";
import { createSlice } from "./slice";

// null until loaded (or when the reader may not see them).
export const channels = createSlice<Channels | null>(null);

let loading: Promise<void> | undefined;

export function loadChannels(force = false): Promise<void> {
  if (loading) return loading;
  if (channels.get() && !force) return Promise.resolve();
  loading = notifyApi
    .channels()
    .then((c) => channels.set(c))
    .catch(() => {})
    .finally(() => (loading = undefined));
  return loading;
}

export function useChannels(): Channels | null {
  useEffect(() => {
    void loadChannels();
  }, []);
  return channels.use((c) => c);
}
