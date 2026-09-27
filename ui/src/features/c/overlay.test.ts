import { afterAll, describe, expect, test } from "bun:test";
import type { Map as MLMap } from "maplibre-gl";
import type { OverlayCtx } from "../../map/overlays";
import type { Role } from "../../api";
import { me } from "../../store/me";
import { mountShiftLasso } from "./overlay";

// Just enough map for the overlay: box zoom and mouse handlers.
function fakeMap() {
  let on = true;
  const map = {
    boxZoom: { enable: () => void (on = true), disable: () => void (on = false), isEnabled: () => on },
    on() {}, off() {}, getLayer: () => undefined, getSource: () => undefined, removeLayer() {}, removeSource() {},
  };
  return { map: map as unknown as MLMap, boxZoom: () => on };
}
const ctxFor = (map: MLMap) => ({ map, beforeId: () => "slot-top", positions: () => new Map() }) as unknown as OverlayCtx;
const as = (role: Role) => me.set({ role, via: "token" } as never);
const frame = () => Bun.sleep(300); // the me slice tells subscribers once a frame
afterAll(() => me.set(null));

describe("shift-drag on the map", () => {
  test("is the lasso for managers and box zoom for everyone who can't lasso", async () => {
    for (const [role, zoom] of [["viewer", true], ["hand", true], ["manager", false], ["owner", false]] as const) {
      as(role);
      const { map, boxZoom } = fakeMap();
      const h = mountShiftLasso(ctxFor(map));
      expect([role, boxZoom()]).toEqual([role, zoom]);
      h.destroy();
      expect(boxZoom()).toBe(true);
    }
  });

  test("follows the role when it changes while the map is up", async () => {
    as("viewer");
    const { map, boxZoom } = fakeMap();
    const h = mountShiftLasso(ctxFor(map));
    expect(boxZoom()).toBe(true);
    as("manager");
    await frame();
    expect(boxZoom()).toBe(false);
    as("hand");
    await frame();
    expect(boxZoom()).toBe(true);
    h.destroy();
  });
});
