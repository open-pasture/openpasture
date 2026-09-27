import { describe, expect, test } from "bun:test";
import type { Alert } from "../../api";
import { ageText, byUrgency, circle, collarsOf, herdRows, memberFacts, openCount, openFor, pulse, since, splitSentence, statusText, upsert } from "./model";
import { drawn } from "./overlay";

const T = "2026-09-27T12:00:00.000Z";
const at = (min: number) => new Date(Date.parse(T) + min * 60_000).toISOString();

function alert(p: Partial<Alert> & { id: string }): Alert {
  return {
    kind: "outside", key: `outside:${p.id}`, severity: "warning", status: "open", herd_id: "herd_1", title: p.id,
    targets: [], data: {}, opened_at: T, updated_at: T, ...p,
  };
}

describe("order and counts", () => {
  test("critical first, then unacked, then newest", () => {
    const list = [
      alert({ id: "old-warn", opened_at: at(-10) }),
      alert({ id: "crit-acked", severity: "critical", status: "acked" }),
      alert({ id: "new-warn", opened_at: at(-1) }),
      alert({ id: "crit", severity: "critical", opened_at: at(-30) }),
      alert({ id: "info", severity: "info" }),
    ];
    expect(list.sort(byUrgency).map((a) => a.id)).toEqual(["crit", "crit-acked", "new-warn", "old-warn", "info"]);
  });

  test("the top bar counts unacked alerts and turns red with a critical one", () => {
    expect(openCount([])).toEqual({ n: 0, critical: false });
    expect(openCount([alert({ id: "a" }), alert({ id: "b", status: "acked", severity: "critical" })])).toEqual({ n: 1, critical: false });
    expect(openCount([alert({ id: "a" }), alert({ id: "c", severity: "critical" })])).toEqual({ n: 2, critical: true });
  });

  test("a live event replaces the alert and a resolved one leaves", () => {
    let list = upsert([], alert({ id: "a" }));
    list = upsert(list, alert({ id: "a", status: "acked" }));
    expect(list.map((a) => a.status)).toEqual(["acked"]);
    list = upsert(list, alert({ id: "a", status: "resolved" }));
    expect(list).toEqual([]);
  });

  test("a herd's panel rows leave out other herds and waiting decisions", () => {
    const list = [
      alert({ id: "mine" }),
      alert({ id: "other", herd_id: "herd_2" }),
      alert({ id: "decision", kind: "decision_waiting" }),
    ];
    expect(herdRows(list, "herd_1").map((a) => a.id)).toEqual(["mine"]);
  });
});

describe("words", () => {
  test("ages step like the texts", () => {
    const now = Date.parse(T);
    expect(ageText(now - 20_000, now)).toBe("1m");
    expect(ageText(now - 6 * 60_000, now)).toBe("6m");
    expect(ageText(now - 185 * 60_000, now)).toBe("3h");
    expect(ageText(now - 3 * 86400_000, now)).toBe("3d");
  });

  test("since is the rule's own time when it has one", () => {
    expect(since(alert({ id: "a", data: { since: at(-25) } }))).toBe(Date.parse(at(-25)));
    expect(since(alert({ id: "a", data: {} }))).toBe(Date.parse(T));
  });

  test("a sentence splits around its number", () => {
    expect(splitSentence("Collar silent for {n} min")).toEqual(["Collar silent for ", " min"]);
    expect(splitSentence("An animal gets out")).toBeNull();
  });

  test("history says who handled it or how it ended", () => {
    expect(statusText(alert({ id: "a" }))).toBe("open");
    expect(statusText(alert({ id: "a", status: "acked", acked_by: { via: "text", name: "Sam" } }))).toBe("acked by Sam");
    expect(statusText(alert({ id: "a", status: "resolved", resolved_at: at(5) }))).toBe("cleared");
    expect(statusText(alert({ id: "a", status: "resolved", resolved_at: at(5), rolled_into: "alr_r" }))).toBe("rolled up");
    expect(statusText(alert({ id: "a", status: "resolved", resolved_at: at(5), resolved_by: { via: "local" } }))).toBe("resolved");
    expect(openFor(alert({ id: "a", status: "resolved", resolved_at: at(25) }), Date.parse(at(90)))).toBe("25m");
  });
});

describe("the map", () => {
  const pos: Record<string, [number, number]> = { c1: [-93.62, 42.03], c2: [-93.621, 42.031], c3: [-93.622, 42.032] };
  const where = (id: string) => pos[id];

  test("an unacked critical rollup rings every member; acked ones don't ring", () => {
    const roll = alert({ id: "r", kind: "escaped", severity: "critical", targets: [["collar", "c1"], ["animal", "a1"], ["collar", "c2"]] });
    expect(collarsOf(roll)).toEqual(["c1", "c2"]);
    expect(drawn([roll], where).rings).toEqual([pos.c1, pos.c2]);
    expect(drawn([{ ...roll, status: "acked" }], where).rings).toEqual([]);
    // No positions: the alert's own place.
    expect(drawn([{ ...roll, targets: [], at: [-93.6, 42.0] }], where).rings).toEqual([[-93.6, 42.0]]);
  });

  test("silent collars get a square at their last fix, weak GPS a circle", () => {
    const s = alert({ id: "s", kind: "silent", targets: [["collar", "c3"]] });
    const g = alert({ id: "g", kind: "gps_degraded", severity: "info", targets: [["collar", "c1"]], data: { accuracy_m: 18 } });
    const d = drawn([s, g], where);
    expect(d.silent).toEqual([pos.c3]);
    expect(d.accuracy.length).toBe(1);
    const ring = (d.accuracy[0].geometry as GeoJSON.Polygon).coordinates[0];
    expect(ring[0]).toEqual(ring[ring.length - 1]);
    // A rollup's members carry their own accuracy.
    const roll = alert({ id: "gr", kind: "gps_degraded", targets: [["collar", "c1"], ["collar", "c2"]], data: { members: [{ label: "1", accuracy_m: 12 }, { label: "2" }] } });
    expect([...memberFacts(roll).keys()]).toEqual(["c1", "c2"]);
    expect(drawn([roll], where).accuracy.length).toBe(1);
  });

  test("an accuracy circle is that many metres across the middle", () => {
    const r = circle([-93.62, 42.03], 20);
    const east = r[0];
    const metres = (east[0] + 93.62) * 111_320 * Math.cos((42.03 * Math.PI) / 180);
    expect(Math.abs(metres - 20)).toBeLessThan(0.01);
  });

  test("the ring grows and fades over its period", () => {
    expect(pulse(0)).toEqual({ radius: 7, opacity: 0.7 });
    const late = pulse(2300);
    expect(late.radius).toBeGreaterThan(20);
    expect(late.opacity).toBeLessThan(0.1);
    expect(pulse(2400)).toEqual(pulse(0));
  });
});
