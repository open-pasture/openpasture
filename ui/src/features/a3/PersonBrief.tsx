// In a person's row in Settings > People: whether they get the morning brief by text, and
// that they texted STOP when they did.

import { useState } from "react";
import type { User } from "../../api";
import { useChannels } from "../../store/a-notify";
import { Check } from "../../ui/Check";
import { textingApi } from "./api";
import { briefChannels, personOf, reachable, withPerson } from "./logic";
import { texting, useTexting } from "./state";

export function PersonBrief({ user }: { user: User }) {
  const t = useTexting();
  const c = useChannels();
  const [err, setErr] = useState<string>();
  if (!t || !c || !reachable(user, briefChannels(c))) return null;
  const p = personOf(t, user.id);
  const flip = async (on: boolean) => {
    setErr(undefined);
    try {
      const next = await textingApi.setBrief(user.id, on);
      const cur = texting.get();
      if (cur) texting.set(withPerson(cur, next));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <div className="line a3person">
      <Check checked={p.brief} onChange={(on) => void flip(on)}>Morning brief</Check>
      {p.sms_opt_out && <span className="mono dim">texted STOP</span>}
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
