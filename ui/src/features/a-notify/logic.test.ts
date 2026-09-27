import { describe, expect, test } from "bun:test";
import type { MessageLog, User } from "../../api";
import type { Channels } from "../../api/a-notify";
import {
  canVerify, cleanCode, defaultTo, FIELDS, firstSegment, fmtPhone, fmtWhen, initialDraft, isOn, needsVerify, patchFor, placeholder, statusText,
  testChannel, upsertMessage,
} from "./logic";

const channels = (over: Partial<Channels> = {}): Channels => ({
  sms: { from: "+15155550100" },
  whatsapp: {},
  email: { port: 587, tls: "starttls" },
  webhook: {},
  relay: { enabled: false },
  twilio_api_base: "https://api.twilio.com",
  secrets: [
    { name: "twilio_account_sid", set: true },
    { name: "twilio_auth_token", set: true },
    { name: "smtp_password", set: false },
    { name: "webhook_secret", set: false },
    { name: "hosted_url", set: false },
    { name: "hosted_api_key", set: false },
  ],
  configured: ["sms"],
  ...over,
});

describe("segments", () => {
  test("the first one that can send is shown first", () => {
    expect(firstSegment([])).toBe("twilio");
    expect(firstSegment(["webhook"])).toBe("webhook");
    expect(firstSegment(["relay", "email"])).toBe("email");
    expect(firstSegment(["whatsapp"])).toBe("twilio");
  });
  test("Twilio is on with SMS or WhatsApp", () => {
    expect(isOn("twilio", ["whatsapp"])).toBe(true);
    expect(isOn("twilio", ["email"])).toBe(false);
    expect(isOn("relay", ["relay"])).toBe(true);
  });
  test("Twilio tests SMS unless only WhatsApp is set up", () => {
    expect(testChannel("twilio", ["sms", "whatsapp"])).toBe("sms");
    expect(testChannel("twilio", ["whatsapp"])).toBe("whatsapp");
    expect(testChannel("twilio", [])).toBe("sms");
    expect(testChannel("email", [])).toBe("email");
  });
  test("tests go to the reader's own phone or email first", () => {
    const me = { role: "owner" as const, via: "user_token" as const, user: { id: "usr_1", name: "Cody", phone: "+15155550123", email: "c@farm.example" } };
    expect(defaultTo("twilio", me)).toBe("+15155550123");
    expect(defaultTo("relay", me)).toBe("+15155550123");
    expect(defaultTo("email", me)).toBe("c@farm.example");
    expect(defaultTo("webhook", me)).toBe("");
    expect(defaultTo("twilio", { role: "owner", via: "local" })).toBe("");
  });
});

describe("the form", () => {
  test("starts from the stored values, secrets empty", () => {
    const d = initialDraft("twilio", channels());
    expect(d).toEqual({ sid: "", token: "", sms: "+15155550100", wa: "", template: "" });
    expect(initialDraft("email", channels()).port).toBe("587");
    expect(initialDraft("email", channels()).tls).toBe("starttls");
  });
  test("sends only what changed", () => {
    const c = channels();
    expect(patchFor("twilio", initialDraft("twilio", c), c)).toBeNull();
    expect(patchFor("twilio", { ...initialDraft("twilio", c), wa: " +15155550199 " }, c)).toEqual({ whatsapp: { from: "+15155550199" } });
    expect(patchFor("twilio", { ...initialDraft("twilio", c), sms: "" }, c)).toEqual({ sms: { from: null } });
    expect(patchFor("twilio", { ...initialDraft("twilio", c), token: "tok" }, c)).toEqual({ secrets: { twilio_auth_token: "tok" } });
  });
  test("email port is a number and TLS a choice", () => {
    const c = channels();
    const d = { ...initialDraft("email", c), host: "smtp.farm.example", port: "465", tls: "tls" };
    expect(patchFor("email", d, c)).toEqual({ email: { host: "smtp.farm.example", port: 465, tls: "tls" } });
  });
  test("a stored secret says so under its name; an empty one shows its hint", () => {
    const c = channels();
    const [sid] = FIELDS.twilio;
    expect(placeholder(sid, c)).toBe("Account SID  saved");
    const [url, key] = FIELDS.relay;
    expect(placeholder(url, c)).toBe("https://api.openpasture.dev");
    expect(placeholder(key, c)).toBe("oph_ key");
    const withKey = channels({ secrets: [...c.secrets.filter((s) => s.name !== "hosted_api_key"), { name: "hosted_api_key", set: true }] });
    expect(placeholder(key, withKey)).toBe("Relay key  saved");
    expect(placeholder(FIELDS.twilio[2], c)).toBe("SMS number");
  });
  test("the relay's URL and key are secrets", () => {
    const c = channels();
    expect(patchFor("relay", { url: "https://relay.example", key: "oph_abc" }, c)).toEqual({ secrets: { hosted_url: "https://relay.example", hosted_api_key: "oph_abc" } });
  });
});

describe("verify", () => {
  const u = (over: Partial<User> = {}): User => ({ id: "usr_1", name: "Hank", role: "hand", phone: "+15155550123", created_at: "2026-09-27T00:00:00Z", ...over });
  test("needs a phone, a way to text, and no verification yet", () => {
    expect(canVerify(["sms"])).toBe(true);
    expect(canVerify(["relay"])).toBe(true);
    expect(canVerify(["email", "webhook", "whatsapp"])).toBe(false);
    expect(needsVerify(u(), ["sms"])).toBe(true);
    expect(needsVerify(u({ phone_verified_at: "2026-09-27T01:00:00Z" }), ["sms"])).toBe(false);
    expect(needsVerify(u({ phone: undefined }), ["sms"])).toBe(false);
    expect(needsVerify(u({ disabled_at: "2026-09-27T01:00:00Z" }), ["sms"])).toBe(false);
    expect(needsVerify(u(), ["email"])).toBe(false);
  });
  test("codes read however they are typed", () => {
    expect(cleanCode("123 456")).toBe("123456");
    expect(cleanCode("123-456")).toBe("123456");
    expect(cleanCode("1234567")).toBe("123456");
  });
});

describe("messages", () => {
  const m = (id: string, over: Partial<MessageLog> = {}): MessageLog => ({
    id, direction: "out", channel: "sms", address: "+15155550123", kind: "alert", text: "t", status: "queued", attempts: 0,
    created_at: "2026-09-27T06:12:00Z", updated_at: "2026-09-27T06:12:00Z", ...over,
  });
  test("phones print in groups", () => {
    expect(fmtPhone("+15155550123")).toBe("+1 515 555 0123");
    expect(fmtPhone("+447700900123")).toBe("+447700900123");
    expect(fmtPhone("https://hooks.example.com")).toBe("https://hooks.example.com");
  });
  test("times are short", () => {
    const now = new Date(2026, 8, 27, 18, 0).getTime();
    expect(fmtWhen(new Date(2026, 8, 27, 6, 12).toISOString(), now)).toBe("06:12");
    expect(fmtWhen(new Date(2026, 8, 20, 6, 12).toISOString(), now)).toBe("Sep 20 06:12");
    expect(fmtWhen(new Date(2025, 8, 20, 6, 12).toISOString(), now)).toBe("2025-09-20");
  });
  test("live messages replace their row or go on top", () => {
    const rows = [m("b"), m("a")];
    expect(upsertMessage(rows, m("c")).map((r) => r.id)).toEqual(["c", "b", "a"]);
    const next = upsertMessage(rows, m("a", { status: "sent" }));
    expect(next.map((r) => r.status)).toEqual(["queued", "sent"]);
    expect(rows[1].status).toBe("queued");
  });
  test("a failure says why", () => {
    expect(statusText(m("a", { status: "failed", error: "Twilio 21211: bad number" }))).toEqual({ text: "Twilio 21211: bad number", tone: "err" });
    expect(statusText(m("a", { status: "delivered" }))).toEqual({ text: "delivered", tone: "ok" });
    expect(statusText(m("a", { status: "sent" }))).toEqual({ text: "sent" });
    expect(statusText(m("a", { status: "queued" })).tone).toBe("dim");
  });
});
