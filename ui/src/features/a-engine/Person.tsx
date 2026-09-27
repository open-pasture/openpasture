// Inside each person's row in Settings > People: how alerts reach them. Channels the farm has
// set up, the least severity they want, their quiet hours and On duty. Nothing until a
// channel can send. Hands and managers set their own the same way in Settings > You.

import { useEffect, useState } from "react";
import type { User } from "../../api";
import { alertsApi, type PersonChannel, type PersonPrefs, type PrefsChange } from "../../api/a-engine";
import { Check } from "../../ui/Check";
import { Segmented } from "../../ui";
import { useMe } from "../../store/me";
import { TimeField } from "./Settings";

const LABEL: Record<PersonChannel, string> = { sms: "SMS", whatsapp: "WhatsApp", email: "Email" };

// One read of everyone's prefs and the channels for all the rows on the page, read again for a
// person it doesn't know yet (added since).
type Loaded = { people: PersonPrefs[]; channels: PersonChannel[]; quiet?: [string, string] };
let shared: Promise<Loaded> | undefined;
const readAll = (): Promise<Loaded> => Promise.all([alertsApi.prefs(), alertsApi.rules()]).then(([people, r]) => ({
  people,
  channels: r.person_channels,
  quiet: r.policy.quiet_start && r.policy.quiet_end ? [r.policy.quiet_start, r.policy.quiet_end] as [string, string] : undefined,
}));
function load(userId: string) {
  shared ??= readAll();
  const got = shared.then((d) => (d.people.some((x) => x.user_id === userId) ? d : (shared = readAll())));
  got.catch(() => (shared = undefined));
  return got;
}

export function PersonAlerts({ user }: { user: User }) {
  const [p, setP] = useState<PersonPrefs>();
  const [channels, setChannels] = useState<PersonChannel[]>([]);
  const [quiet, setQuiet] = useState<[string, string]>();
  const [err, setErr] = useState<string>();
  useEffect(() => {
    load(user.id).then((d) => {
      setP(d.people.find((x) => x.user_id === user.id));
      setChannels(d.channels);
      setQuiet(d.quiet);
    }, () => setP(undefined));
  }, [user.id]);
  if (!p || !channels.length || user.disabled_at) return null;
  const save = async (b: PrefsChange) => {
    setErr(undefined);
    try {
      setP(await alertsApi.setPrefs(user.id, b));
      shared = undefined;
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return <Prefs p={p} channels={channels} quiet={quiet} save={save} err={err} />;
}

// Settings > You: your own, for a hand or a manager (the owner's own is in People).
export function MyAlerts() {
  const me = useMe();
  const uid = me?.user?.id;
  const [p, setP] = useState<PersonPrefs>();
  const [channels, setChannels] = useState<PersonChannel[]>([]);
  const [quiet, setQuiet] = useState<[string, string]>();
  const [err, setErr] = useState<string>();
  const mine = !!uid && me?.role !== "owner";
  useEffect(() => {
    if (!mine) return;
    Promise.all([alertsApi.myPrefs(), alertsApi.rules()]).then(([mp, r]) => {
      setP(mp);
      setChannels(r.person_channels);
      setQuiet(r.policy.quiet_start && r.policy.quiet_end ? [r.policy.quiet_start, r.policy.quiet_end] : undefined);
    }, () => setP(undefined));
  }, [mine, uid]);
  if (!mine || !p || !channels.length) return null;
  const save = async (b: PrefsChange) => {
    setErr(undefined);
    try {
      setP(await alertsApi.setMyPrefs(b));
      shared = undefined;
    } catch (e) {
      setErr((e as Error).message);
    }
  };
  return <Prefs p={p} channels={channels} quiet={quiet} save={save} err={err} />;
}

function Prefs({ p, channels, quiet, save, err }: {
  p: PersonPrefs; channels: PersonChannel[]; quiet?: [string, string]; save: (b: PrefsChange) => Promise<void>; err?: string;
}) {
  const toggle = (c: PersonChannel, on: boolean) => save({ channels: on ? [...p.channels, c] : p.channels.filter((x) => x !== c) });
  return (
    <div className="aprefs">
      {channels.map((c) => <Check key={c} checked={p.channels.includes(c)} onChange={(on) => toggle(c, on)}>{LABEL[c]}</Check>)}
      <Segmented label="Least severity" value={p.min_severity === "critical" ? "critical" : "warning"}
        onChange={(min_severity) => save({ min_severity })}
        options={[{ value: "warning", label: "Warning" }, { value: "critical", label: "Critical" }]} />
      <span className="aquiet">
        <TimeField value={p.quiet_start} placeholder={quiet?.[0]} label="Quiet from" onChange={(t) => save({ quiet_start: t, quiet_end: t === null ? null : p.quiet_end ?? quiet?.[1] ?? "06:00" })} />
        <TimeField value={p.quiet_end} placeholder={quiet?.[1]} label="Quiet to" onChange={(t) => save({ quiet_end: t, quiet_start: t === null ? null : p.quiet_start ?? quiet?.[0] ?? "22:00" })} />
      </span>
      <Check checked={p.on_duty} onChange={(on_duty) => save({ on_duty })}>On duty</Check>
      {err && <span className="mono err">{err}</span>}
    </div>
  );
}
