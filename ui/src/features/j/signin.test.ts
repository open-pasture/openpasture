import { describe, expect, test } from "bun:test";
import { fieldChange, formatPhone, joinCode, showYou } from "./signin";

describe("people", () => {
  test("North American numbers read in groups", () => {
    expect(formatPhone("+15155550123")).toBe("+1 515 555 0123");
    expect(formatPhone("+447700900123")).toBe("+447700900123");
    expect(formatPhone(undefined)).toBe("");
  });

  test("a pasted sign-in link gives its code", () => {
    const code = "3f9a0c1d2e3f4a5b6c7d8e9f0a1b2c3d";
    expect(joinCode(`https://farm.example/#/join/${code}`)).toBe(code);
    expect(joinCode(`  http://192.168.1.2:7878/#/join/${code.toUpperCase()} `)).toBe(code);
    expect(joinCode(`#/join/${code}`)).toBe(code);
    expect(joinCode("opu_" + "a".repeat(64))).toBeUndefined();
    expect(joinCode("#/join/")).toBeUndefined();
  });

  test("inline edits send only what changed", () => {
    expect(fieldChange("name", " Ana ", "Ana")).toBeUndefined();
    expect(fieldChange("name", "", "Ana")).toBeUndefined();
    expect(fieldChange("name", "Ana B", "Ana")).toEqual({ name: "Ana B" });
    expect(fieldChange("phone", "+1 515 555 0123", "+15155550123")).toBeUndefined();
    expect(fieldChange("phone", "515 555 0199", "+15155550123")).toEqual({ phone: "515 555 0199" });
    expect(fieldChange("phone", "", "+15155550123")).toEqual({ phone: null });
    expect(fieldChange("email", "", undefined)).toBeUndefined();
    expect(fieldChange("email", "a@b.co", undefined)).toEqual({ email: "a@b.co" });
  });

  test("the You row is for people who aren't the owner, and for signing out", () => {
    const user = { id: "usr_1", name: "Ana" };
    expect(showYou(null)).toBe(false);
    expect(showYou({ role: "owner", via: "local" })).toBe(false);
    expect(showYou({ role: "owner", via: "local", user })).toBe(false);
    expect(showYou({ role: "owner", via: "user_token", user })).toBe(true);
    expect(showYou({ role: "viewer", via: "user_token", user })).toBe(true);
    expect(showYou({ role: "manager", via: "user_token" })).toBe(false);
  });
});
