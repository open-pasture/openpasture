import { expect, test } from "bun:test";
import { seconds, slotsLine } from "./format";
import type { CollarSlots } from "../../api/e-srv";

const limits = { outer: 128, holes: 16, hole_vertices: 32, total: 384, slots: 16, slot_bytes: 24576 };
const at = "2026-09-27T12:00:00.000Z";

test("firmware and held versions, in order", () => {
  const s: CollarSlots = {
    collar_id: "col_a", fw: "0.2.0", limits,
    slots: [
      { version: 59, status: "received", effective_at: at, reported_at: at },
      { version: 57, status: "applied", reported_at: at },
      { version: 58, status: "received", effective_at: at, reported_at: at },
    ],
  };
  expect(slotsLine(s)).toBe("fw 0.2.0  slots 57 58 59");
});

test("copies count as the herd version; refused ones are not held", () => {
  const s: CollarSlots = {
    collar_id: "col_a", fw: "0.2.0", limits,
    slots: [
      { version: 61, copy_of: 57, status: "applied", reported_at: at },
      { version: 62, copy_of: 58, status: "received", reported_at: at },
      { version: 63, status: "rejected", code: "slots_full", reported_at: at },
    ],
  };
  expect(slotsLine(s)).toBe("fw 0.2.0  slots 57 58");
});

test("a legacy collar shows its slots alone, and a silent one nothing", () => {
  expect(slotsLine({ collar_id: "c", limits, slots: [{ version: 3, status: "applied", reported_at: at }] })).toBe("slots 3");
  expect(slotsLine({ collar_id: "c", limits, slots: [] })).toBeNull();
});

test("cadence seconds a collar accepts", () => {
  expect(seconds("60")).toBe(60);
  expect(seconds(" 10 ")).toBe(10);
  expect(seconds("3600")).toBe(3600);
  for (const bad of ["9", "3601", "", "1.5", "ten", "-20"]) expect(seconds(bad)).toBeNull();
});
