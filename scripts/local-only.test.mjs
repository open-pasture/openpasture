// scripts/local-only.mjs: bun and node dev scripts drive this machine only.
//   bun test scripts/local-only.test.mjs
import { expect, test } from "bun:test";
import { localOnly } from "./local-only.mjs";

test("this machine passes", () => {
  for (const u of ["http://127.0.0.1:17150", "http://localhost:7991/", "http://[::1]:8080", "ws://127.0.0.1:17150/api/live?token=t"]) {
    expect(localOnly(u)).toBe(u.replace(/\/+$/, ""));
  }
});

test("staging, production and other hosts are refused", () => {
  for (const u of ["https://op-server-staging-staging.up.railway.app", "http://op-server-staging-staging.up.railway.app",
    "https://openpasture.dev", "http://192.168.1.20:7878", "http://127.0.0.1.nip.io:17150", "https://127.0.0.1:17150", "", "127.0.0.1:17150"]) {
    expect(() => localOnly(u)).toThrow();
  }
});
