// Top bar, while the server is out of reach: how old the positions on the map are, after the
// existing "offline" marker ("offline 12m").

import { useStore } from "../../store";
import { age, useNow } from "../../util";
import { newestFix } from "./offline";

export function OfflineAge() {
  const up = useStore((s) => s.up);
  const collars = useStore((s) => s.collars);
  const now = useNow(15_000);
  if (up) return null;
  const t = newestFix(collars);
  if (t === undefined) return null;
  return <span className="mage mono" title="Age of the newest position">{age(new Date(t).toISOString(), now)}</span>;
}
