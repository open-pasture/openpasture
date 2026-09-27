// Settings > People (owner): everyone on the farm, one line each. A line opens to its role,
// phone and email, a sign-in link, and what other features put in a person's row.

import { useEffect, useState } from "react";
import type { Role } from "../../api";
import { people, type Person } from "../../api/j";
import { peopleRow, Sections } from "../../registry";
import { loadMe } from "../../store/me";
import { Button, Copy, Input, Segmented } from "../../ui";
import { Field } from "./Field";
import { fieldChange, formatPhone, ROLE_LABEL, ROLES } from "./signin";

const roleOptions = ROLES.map((r) => ({ value: r, label: ROLE_LABEL[r] }));

export function People() {
  const [list, setList] = useState<Person[] | null>(null);
  const [open, setOpen] = useState<string>();
  // Sign-in links made on this page, by person: the URL is only ever shown once.
  const [links, setLinks] = useState<Record<string, string>>({});
  const [adding, setAdding] = useState(false);
  useEffect(() => {
    people.list().then(setList, () => setList(null));
  }, []);
  if (!list) return null;

  const changed = (p: Person) => {
    setList((l) => l && l.map((x) => (x.id === p.id ? p : x)));
    void loadMe(); // it may be you
  };
  const removed = (id: string) => {
    setList((l) => l && l.filter((x) => x.id !== id));
    setOpen(undefined);
    void loadMe();
  };

  return (
    <div className="people">
      {list.length > 0 && (
        <ul>
          {list.map((p) => (
            <PersonRow key={p.id} p={p} open={open === p.id} link={links[p.id]}
              onToggle={() => setOpen(open === p.id ? undefined : p.id)}
              onLink={(url) => setLinks((l) => ({ ...l, [p.id]: url }))}
              onChanged={changed} onRemoved={() => removed(p.id)} />
          ))}
        </ul>
      )}
      {adding ? (
        <AddPerson onCancel={() => setAdding(false)} onAdded={(p) => {
          setList((l) => [...(l ?? []), p]);
          setOpen(p.id);
          setAdding(false);
        }} />
      ) : (
        <div className="line"><Button small onClick={() => setAdding(true)}>Add person</Button></div>
      )}
    </div>
  );
}

function PersonRow({ p, open, link, onToggle, onLink, onChanged, onRemoved }: {
  p: Person; open: boolean; link?: string; onToggle: () => void; onLink: (url: string) => void;
  onChanged: (p: Person) => void; onRemoved: () => void;
}) {
  const [err, setErr] = useState<string>();
  const [busy, setBusy] = useState(false);
  const run = async (f: () => Promise<void>) => {
    setErr(undefined);
    setBusy(true);
    try {
      await f();
    } catch (e) {
      setErr((e as Error).message);
    } finally {
      setBusy(false);
    }
  };
  const save = (key: "name" | "phone" | "email") => async (v: string) => {
    const change = fieldChange(key, v, p[key]);
    if (change) onChanged(await people.update(p.id, change));
  };
  const setRole = (role: Role) => run(async () => onChanged(await people.update(p.id, { role })));
  const disabled = !!p.disabled_at;

  return (
    <li className={open ? "on" : undefined} data-disabled={disabled || undefined}>
      <button type="button" className="prow" aria-expanded={open} onClick={onToggle}>
        <span className="pn">{p.name}</span>
        <span className="mono dim">{p.role}</span>
        <span className="mono">{formatPhone(p.phone)}</span>
        {p.tokens > 0 && <i className="dot ok" title="Signed in" aria-label="signed in" />}
      </button>
      {open && (
        <div className="more">
          <div className="line">
            <Field value={p.name} label="Name" onSave={save("name")} className="pname" />
            <Segmented label="Role" value={p.role} options={roleOptions} onChange={setRole} />
          </div>
          <div className="line">
            <Field value={formatPhone(p.phone)} label="Phone" mono inputMode="tel" onSave={save("phone")} />
            <Field value={p.email ?? ""} label="Email" type="email" onSave={save("email")} />
          </div>
          {link && <Copy label="link" value={link} />}
          <div className="line">
            {!disabled && (
              <Button small disabled={busy} onClick={() => run(async () => {
                const made = await people.link(p.id);
                onLink(made.url);
                onChanged({ ...p, invite_until: made.expires_at });
              })}>Sign-in link</Button>
            )}
            {(p.tokens > 0 || p.invite_until) && (
              <Button small kind="plain" disabled={busy} onClick={() => run(async () => {
                onChanged(await people.revoke(p.id));
                onLink("");
              })}>Revoke</Button>
            )}
            {disabled && (
              <Button small disabled={busy} onClick={() => run(async () => onChanged(await people.update(p.id, { disabled: false })))}>Enable</Button>
            )}
            <Button small kind="plain" className="danger" disabled={busy} onClick={() => run(async () => {
              await people.remove(p.id);
              onRemoved();
            })}>Remove</Button>
            {err && <span className="mono err">{err}</span>}
          </div>
          <Sections of={peopleRow} props={{ user: p }} />
        </div>
      )}
    </li>
  );
}

function AddPerson({ onAdded, onCancel }: { onAdded: (p: Person) => void; onCancel: () => void }) {
  const [f, setF] = useState({ name: "", role: "hand" as Role, phone: "", email: "" });
  const [err, setErr] = useState<string>();
  const add = async () => {
    if (!f.name.trim()) return;
    setErr(undefined);
    try {
      onAdded(await people.add({ name: f.name.trim(), role: f.role, phone: f.phone.trim() || undefined, email: f.email.trim() || undefined }));
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return (
    <form className="padd" onSubmit={(e) => { e.preventDefault(); void add(); }}
      onKeyDown={(e) => e.key === "Escape" && onCancel()}>
      <div className="line">
        <Input autoFocus className="sm" placeholder="Name" aria-label="Name" value={f.name} onChange={(e) => setF({ ...f, name: e.target.value })} />
        <Segmented label="Role" value={f.role} options={roleOptions} onChange={(role) => setF({ ...f, role })} />
      </div>
      <div className="line">
        <Input mono className="sm" placeholder="Phone" aria-label="Phone" inputMode="tel" value={f.phone} onChange={(e) => setF({ ...f, phone: e.target.value })} />
        <Input className="sm" placeholder="Email" aria-label="Email" type="email" value={f.email} onChange={(e) => setF({ ...f, email: e.target.value })} />
      </div>
      <div className="line">
        <Button small kind="plain" onClick={onCancel}>Cancel</Button>
        <Button small kind="primary" type="submit" disabled={!f.name.trim()}>Add</Button>
        {err && <span className="mono err">{err}</span>}
      </div>
    </form>
  );
}
