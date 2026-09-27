// #/join/<code>: accept a sign-in link, keep the token, open the app. The code leaves the
// address bar (and history) as soon as it's used.

import { useEffect, useState } from "react";
import { setToken } from "../../api";
import { people, type Accepted } from "../../api/j";
import { store } from "../../store";
import { Mark } from "../../ui";

// One accept per code, even when React runs the effect twice.
const accepting = new Map<string, Promise<Accepted>>();

export function Join({ code }: { code: string }) {
  const [err, setErr] = useState<string>();
  useEffect(() => {
    let p = accepting.get(code);
    if (!p) accepting.set(code, (p = people.accept(code)));
    let live = true;
    p.then(
      (a) => {
        setToken(a.token);
        store.tokenSaved();
        location.replace("#/");
      },
      (e) => live && setErr((e as Error).message),
    );
    return () => void (live = false);
  }, [code]);
  return (
    <div className="boot">
      <Mark size={22} />
      {err && <p className="mono dim">{err}</p>}
    </div>
  );
}
