// #/join/<code>: accept a sign-in link, keep the token, open the app. `owner`: #/signin/<code>,
// this machine's one-time owner code instead. The code leaves the address bar (and history) as
// soon as it's used.

import { useEffect, useState } from "react";
import { setToken } from "../../api";
import { people } from "../../api/j";
import { rulesApi } from "../rules/api";
import { store } from "../../store";
import { Mark } from "../../ui";

// One accept per code, even when React runs the effect twice.
const accepting = new Map<string, Promise<{ token: string }>>();

export function Join({ code, owner }: { code: string; owner?: boolean }) {
  const [err, setErr] = useState<string>();
  useEffect(() => {
    let p = accepting.get(code);
    if (!p) accepting.set(code, (p = owner ? rulesApi.signin(code) : people.accept(code)));
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
  }, [code, owner]);
  return (
    <div className="boot">
      <Mark size={22} />
      {err && <p className="mono dim">{err}</p>}
    </div>
  );
}
