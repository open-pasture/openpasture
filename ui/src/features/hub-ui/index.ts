// The app's own views and map tools, registered like any feature's.

import { lazy } from "react";
import { views } from "../../registry";
import { tools } from "../../map/tools";
import "../../styles/hub-ui.css";

// Map views pull in MapLibre and terra-draw; every view loads on demand.
views.register({ id: "map", label: "Map", key: "m", order: 10, icon: "navmap", View: lazy(() => import("../../views/MapView").then((m) => ({ default: m.MapView }))) });
views.register({ id: "data", label: "Data", key: "d", order: 30, icon: "navdata", View: lazy(() => import("../../views/Data").then((m) => ({ default: m.DataView }))) });
views.register({ id: "settings", label: "Settings", key: "s", order: 40, icon: "navgear", View: lazy(() => import("../../views/Settings").then((m) => ({ default: m.SettingsView }))) });

// Boundaries go to collars, so a herd without any has nothing to send one to.
tools.register({
  id: "boundary", label: "Boundary", key: "b", group: "bar", order: 10, minRole: "manager",
  when: (c) => c.collars.length > 0,
  Tool: lazy(() => import("../../map/tools/boundary").then((m) => ({ default: m.BoundaryTool }))),
});
tools.register({
  id: "paddock", label: "Paddock", key: "p", group: "draw", order: 10, minRole: "manager",
  Tool: lazy(() => import("../../map/tools/paddock").then((m) => ({ default: m.PaddockTool }))),
});
