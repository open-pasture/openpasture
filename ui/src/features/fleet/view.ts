// The Fleet view's rail entry, added when Settings > Advanced turns it on.

import { lazy } from "react";
import { views } from "../../registry";

const fleetView = () => import("./FleetView");

export function showFleet() {
  if (views.has("fleet")) return;
  views.register({
    id: "fleet", label: "Fleet", key: "f", order: 35, minRole: "manager", side: false, preload: fleetView,
    View: lazy(() => fleetView().then((m) => ({ default: m.FleetView }))),
  });
}
