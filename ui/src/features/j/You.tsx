// Settings > You: your own name, phone and email when you aren't the owner, and Sign out for a
// browser signed in with its own token.

import { setToken } from "../../api";
import { people } from "../../api/j";
import { loadMe, useMe } from "../../store/me";
import { Button } from "../../ui";
import { Field } from "./Field";
import { fieldChange, formatPhone, showYou } from "./signin";

export function You() {
  const me = useMe();
  if (!me?.user || !showYou(me)) return null;
  const user = me.user;
  const save = (key: "name" | "phone" | "email") => async (v: string) => {
    const change = fieldChange(key, v, user[key]);
    if (!change) return;
    await people.profile(change);
    await loadMe();
  };
  const signout = async () => {
    await people.signout().catch(() => {});
    setToken("");
    location.replace("#/");
    location.reload();
  };
  return (
    <div className="you">
      {me.role !== "owner" && (
        <>
          <div className="line">
            <Field value={user.name} label="Name" onSave={save("name")} className="pname" />
            <span className="mono dim">{me.role}</span>
          </div>
          <div className="line">
            <Field value={formatPhone(user.phone)} label="Phone" mono inputMode="tel" onSave={save("phone")} />
            <Field value={user.email ?? ""} label="Email" type="email" onSave={save("email")} />
          </div>
        </>
      )}
      {me.via === "user_token" && <div className="line"><Button small kind="plain" onClick={signout}>Sign out</Button></div>}
    </div>
  );
}
