import { useEffect, useState, type ReactNode } from "react";
import { api, type Brain, type BrainTest, type HostedKey, type NewHostedKey, type SecretName, type SecretStatus, type Settings } from "../api";
import { guarded, interleave, SETTINGS, settingsSections, type SettingsSection } from "../registry";
import { useCan } from "../store/me";
import { Button, Copy, Input } from "../ui";

export function SettingsView() {
  // The built-in rows write owner-only settings and secrets.
  const owner = useCan("owner");
  const [settings, setSettings] = useState<Settings>();
  const [secrets, setSecrets] = useState<SecretStatus[]>([]);
  const reload = async () => {
    const [s, k] = await Promise.all([api.settings(), api.secrets()]);
    setSettings(s);
    setSecrets(k);
  };
  useEffect(() => {
    if (owner) void reload();
  }, [owner]);
  const groups = useGroups();
  const isSet = (n: SecretName) => secrets.some((s) => s.name === n && s.set);
  if (owner && !settings) return <div className="settings" />;

  // A registered group named like a built-in row joins it; the others are rows of their own.
  const joined = (label: keyof typeof SETTINGS) => groups.find((g) => g.group === label)?.node;
  const core = owner && settings ? [
    { key: "Brain", order: SETTINGS.Brain, node: <Brains settings={settings} isSet={isSet} onSaved={reload} extra={joined("Brain")} /> },
    { key: "Daily", order: SETTINGS.Daily, node: <Row label="Daily"><Daily key={settings.decision_time} settings={settings} onSaved={setSettings} />{joined("Daily")}</Row> },
    { key: "Hosting", order: SETTINGS.Hosting, node: <HostedKeys extra={joined("Hosting")} /> },
    { key: "Land", order: SETTINGS.Land, node: <Row label="Land"><Secret name="firecrawl_api_key" placeholder="Firecrawl key" set={isSet("firecrawl_api_key")} onSaved={reload} />{joined("Land")}</Row> },
    { key: "Server", order: SETTINGS.Server, node: <Row label="Server"><Server settings={settings} onSaved={setSettings} />{joined("Server")}</Row> },
  ] : [];
  const builtIn = new Set(core.map((c) => c.key));
  const added = groups
    .filter((g) => !builtIn.has(g.group))
    .map((g) => ({ key: `group:${g.group}`, order: g.order, node: <Row label={g.group}>{g.node}</Row> }));

  return <div className="settings">{interleave(core, added)}</div>;
}

// Registered sections by group, in order; a group sits at its first section's order.
function useGroups(): { group: string; order: number; node: ReactNode }[] {
  const byGroup = new Map<string, SettingsSection[]>();
  for (const s of settingsSections.use()) byGroup.set(s.group, [...(byGroup.get(s.group) ?? []), s]);
  return [...byGroup].map(([group, list]) => ({
    group,
    order: list[0].order,
    node: <div className="sgroup">{list.map((s) => <div key={s.id} aria-label={s.label}>{guarded(s.id, <s.Section />)}</div>)}</div>,
  }));
}

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section className="srow">
      <h2>{label}</h2>
      <div>{children}</div>
    </section>
  );
}

// ---- brain -------------------------------------------------------------------

const SECRET_LABEL: Partial<Record<SecretName, string>> = { compatible_base_url: "Base URL", hosted_url: "URL" };

function Brains({ settings, isSet, onSaved, extra }: { settings: Settings; isSet: (n: SecretName) => boolean; onSaved: () => void; extra?: ReactNode }) {
  const [brains, setBrains] = useState<Brain[] | null>(null);
  const [test, setTest] = useState<BrainTest | "running">();
  const load = () => api.brains().then(setBrains).catch(() => setBrains(null));
  useEffect(() => {
    void load();
  }, []);
  const sel = settings.brain.id;

  const choose = async (b: Brain) => {
    setTest(undefined);
    await api.updateSettings({ brain: { id: b.id, model: b.models[0] ?? null } });
    onSaved();
  };
  const runTest = async () => {
    setTest("running");
    try {
      setTest(await api.testBrain(sel));
    } catch (e) {
      setTest({ ok: false, detail: (e as Error).message, ms: 0 });
    }
  };

  if (!brains) return null;
  return (
    <Row label="Brain">
    <ul className="brains" role="radiogroup" aria-label="Brain">
      {brains.map((b) => {
        const on = b.id === sel;
        const ready = b.available && b.signed_in;
        return (
          <li key={b.id} className={on ? "on" : undefined}>
            <button type="button" role="radio" aria-checked={on} onClick={() => choose(b)}>
              <i className={"dot" + (ready ? " ok" : "")} aria-label={ready ? "ready" : "not set up"} />
              <span>{b.name}</span>
              {!ready && b.detail && <code>{b.detail}</code>}
            </button>
            {on && (
              <div className="more">
                {b.needs.map((n) => (
                  <Secret key={n} name={n} set={isSet(n)} onSaved={() => { onSaved(); void load(); }}
                    placeholder={SECRET_LABEL[n] ?? "Key"} plain={!!SECRET_LABEL[n]} />
                ))}
                <div className="line">
                  {b.models.length > 1 && (
                    <select className="input sm mono" value={settings.brain.model ?? b.models[0]} aria-label="Model"
                      onChange={async (e) => { await api.updateSettings({ brain: { id: b.id, model: e.target.value } }); onSaved(); }}>
                      {b.models.map((m) => <option key={m}>{m}</option>)}
                    </select>
                  )}
                  <Button small onClick={runTest} disabled={test === "running"}>Test</Button>
                  {test && test !== "running" && (
                    <span className={"mono " + (test.ok ? "ok" : "err")}>{test.ok ? `${test.detail}  ${test.ms} ms` : test.detail}</span>
                  )}
                </div>
              </div>
            )}
          </li>
        );
      })}
    </ul>
    {extra}
    </Row>
  );
}

// Keys this server issues so other servers can use it as their brain.
function HostedKeys({ extra }: { extra?: ReactNode }) {
  const [keys, setKeys] = useState<HostedKey[] | null>(null);
  const [made, setMade] = useState<NewHostedKey>();
  const load = () => api.hostedKeys().then(setKeys).catch(() => setKeys(null));
  useEffect(() => {
    void load();
  }, []);
  if (!keys) return null;
  return (
    <Row label="Hosting">
      <div className="device">
        {keys.map((k) => (
          <div className="line" key={k.id}>
            <code className="grow mono dim">{k.label || k.id}</code>
            <span className="mono dim">{(k.last_used ?? k.created_at).slice(0, 10)}</span>
            <Button small kind="plain" onClick={async () => { await api.deleteHostedKey(k.id); if (made?.id === k.id) setMade(undefined); void load(); }}>Revoke</Button>
          </div>
        ))}
        {made && <Copy label="key" value={made.key} />}
        <div className="line">
          <Button small onClick={async () => { setMade(await api.createHostedKey()); void load(); }}>New key</Button>
        </div>
      </div>
      {extra}
    </Row>
  );
}

// The farm-local time of the daily decision.
function Daily({ settings, onSaved }: { settings: Settings; onSaved: (s: Settings) => void }) {
  const [v, setV] = useState(settings.decision_time);
  const save = async () => {
    if (/^\d\d:\d\d$/.test(v) && v !== settings.decision_time) onSaved(await api.updateSettings({ decision_time: v }));
  };
  return (
    <form className="line" onSubmit={(e) => { e.preventDefault(); void save(); }}>
      <Input mono type="time" className="sm" value={v} onChange={(e) => setV(e.target.value)} onBlur={save}
        onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); void save(); } }} aria-label="Daily decision time" />
    </form>
  );
}

// ---- secrets -----------------------------------------------------------------

function Secret({ name, set, placeholder, onSaved, plain }: {
  name: SecretName; set: boolean; placeholder: string; onSaved: () => void; plain?: boolean;
}) {
  const [v, setV] = useState("");
  const save = async () => {
    if (!v.trim()) return;
    await api.setSecret(name, v.trim());
    setV("");
    onSaved();
  };
  return (
    <form className="line" onSubmit={(e) => { e.preventDefault(); void save(); }}>
      <Input mono type={plain ? "text" : "password"} value={v} onChange={(e) => setV(e.target.value)}
        placeholder={set ? "••••••••  saved" : placeholder} aria-label={placeholder} />
      {v && <Button small kind="primary" type="submit">Save</Button>}
      {set && !v && <Button small kind="plain" onClick={async () => { await api.deleteSecret(name); onSaved(); }}>Remove</Button>}
    </form>
  );
}

// ---- server ------------------------------------------------------------------

function Server({ settings, onSaved }: { settings: Settings; onSaved: (s: Settings) => void }) {
  const [s, setS] = useState(settings.server);
  const dirty = s.bind !== settings.server.bind || s.port !== settings.server.port || (s.public_url ?? "") !== (settings.server.public_url ?? "");
  const save = async () => onSaved(await api.updateSettings({ server: { bind: s.bind, port: s.port, public_url: s.public_url?.trim() || null } }));
  return (
    <form className="server" onSubmit={(e) => { e.preventDefault(); void save(); }}>
      <div className="line">
        <Input mono value={s.bind} onChange={(e) => setS({ ...s, bind: e.target.value })} aria-label="Address" className="grow" />
        <Input mono value={String(s.port)} inputMode="numeric" onChange={(e) => setS({ ...s, port: Number(e.target.value.replace(/\D/g, "")) || 0 })} aria-label="Port" className="port" />
      </div>
      <Input mono value={s.public_url ?? ""} placeholder="Public URL" onChange={(e) => setS({ ...s, public_url: e.target.value })} aria-label="Public URL" />
      <Copy label="token" value={settings.server.app_token} secret />
      {dirty && <div className="line"><Button small kind="primary" type="submit">Save</Button></div>}
    </form>
  );
}
