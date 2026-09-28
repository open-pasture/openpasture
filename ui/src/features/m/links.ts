// The app's own links from outside it (a notification): "#/map?alert=<id>". Pure.

import { parseHash } from "../../util";

export function alertOfHash(hash: string): string | undefined {
  const [view, rest] = parseHash(hash);
  if (view !== "map" || !rest.startsWith("?")) return undefined;
  return new URLSearchParams(rest.slice(1)).get("alert") ?? undefined;
}
