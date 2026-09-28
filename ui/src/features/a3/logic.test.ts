import { describe, expect, test } from "bun:test";
import type { User } from "../../api";
import type { Texting } from "./api";
import { briefChannels, canBrief, personOf, reachable, repliesLine, validTime, withPerson } from "./logic";

const base: Texting = {
  inbound: true, poll_s: 10, approve_window_h: 12, brief: { enabled: false, time: "06:30" },
  inbound_mode: "polling", people: [],
};
const user = (u: Partial<User> = {}): User => ({ id: "usr_1", name: "Cody", role: "owner", created_at: "2026-09-27T00:00:00Z", ...u });

describe("repliesLine", () => {
  test("says how texts come in, in the mode's words", () => {
    expect(repliesLine(base, ["sms"])).toEqual({ label: "Replies checked every 10 s", urls: [], error: undefined });
    expect(repliesLine({ ...base, poll_s: 30, checked: { error: "Twilio can't be reached." } }, ["sms"])?.error).toBe("Twilio can't be reached.");
    expect(repliesLine({ ...base, inbound_mode: "relay" }, ["relay"])?.label).toBe("Replies through the relay");
    const hooked = repliesLine({ ...base, inbound_mode: "webhook", hooks: { sms: "https://farm.example/hooks/twilio/sms", whatsapp: "https://farm.example/hooks/twilio/whatsapp" } }, ["sms", "whatsapp"]);
    expect(hooked).toEqual({
      label: "Replies by webhook",
      urls: [{ label: "SMS", url: "https://farm.example/hooks/twilio/sms" }, { label: "WhatsApp", url: "https://farm.example/hooks/twilio/whatsapp" }],
    });
  });

  test("shows nothing when nothing can text the farm back, and an off switch when off", () => {
    expect(repliesLine(base, [])).toBeNull();
    expect(repliesLine(base, ["email", "webhook"])).toBeNull();
    expect(repliesLine({ ...base, inbound: false, inbound_mode: "off" }, ["sms"])).toEqual({ label: "Replies", urls: [] });
    expect(repliesLine({ ...base, inbound_mode: "off" }, ["sms"])).toBeNull();
  });
});

describe("the brief", () => {
  test("is offered once a channel can carry it", () => {
    expect(canBrief([])).toBe(false);
    expect(canBrief(["webhook"])).toBe(false);
    for (const c of ["sms", "whatsapp", "email", "relay", "push"]) expect(canBrief([c])).toBe(true);
  });

  test("reaches a verified phone or an email", () => {
    expect(reachable(user({ phone: "+15155550123" }), ["sms"])).toBe(false);
    expect(reachable(user({ phone: "+15155550123", phone_verified_at: "2026-09-27T00:00:00Z" }), ["sms"])).toBe(true);
    expect(reachable(user({ phone: "+15155550123", phone_verified_at: "2026-09-27T00:00:00Z" }), ["email"])).toBe(false);
    expect(reachable(user({ email: "cody@farm.example" }), ["email"])).toBe(true);
    expect(reachable(user({ email: "cody@farm.example" }), ["relay"])).toBe(false);
    expect(reachable(user({ email: "cody@farm.example", disabled_at: "2026-09-27T00:00:00Z" }), ["email"])).toBe(false);
    // Reached by push only: a browser of theirs takes notifications, and push is a farm channel.
    expect(reachable(user({}), ["push"], true)).toBe(true);
    expect(reachable(user({}), ["push"], false)).toBe(false);
    expect(reachable(user({}), ["sms"], true)).toBe(false);
    expect(reachable(user({ disabled_at: "2026-09-27T00:00:00Z" }), ["push"], true)).toBe(false);
  });

  test("goes by WhatsApp only with an approved template", () => {
    expect(briefChannels({ configured: ["whatsapp", "email"], whatsapp: {} })).toEqual(["email"]);
    expect(briefChannels({ configured: ["whatsapp"], whatsapp: { template_sid: "HX0123" } })).toEqual(["whatsapp"]);
    const phone = user({ phone: "+15155550123", phone_verified_at: "2026-09-27T00:00:00Z" });
    expect(reachable(phone, briefChannels({ configured: ["whatsapp"], whatsapp: {} }))).toBe(false);
    expect(canBrief(briefChannels({ configured: ["whatsapp"], whatsapp: {} }))).toBe(false);
  });

  test("times are HH:MM", () => {
    expect(validTime("06:30")).toBe(true);
    expect(validTime("23:59")).toBe(true);
    for (const t of ["6:30", "24:00", "06:60", "", "06.30"]) expect(validTime(t)).toBe(false);
  });

  test("a person's state is kept per person", () => {
    expect(personOf(base, "usr_1")).toEqual({ user_id: "usr_1", brief: false, sms_opt_out: false, push: false });
    const t = withPerson(base, { user_id: "usr_1", brief: true, sms_opt_out: false, push: false });
    expect(personOf(t, "usr_1").brief).toBe(true);
    expect(withPerson(t, { user_id: "usr_1", brief: false, sms_opt_out: true, push: false }).people).toEqual([{ user_id: "usr_1", brief: false, sms_opt_out: true, push: false }]);
  });
});
