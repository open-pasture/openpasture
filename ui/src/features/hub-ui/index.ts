// The app's own views and map tools, registered like any feature's.

import { lazy } from "react";
import { sidebars, views } from "../../registry";
import { OutlineSidebar } from "../../shell/Outline";
import { tools } from "../../map/tools";
import "../../styles/hub-ui.css";

// Map views pull in MapLibre and terra-draw; every view loads on demand.
const mapView = () => import("../../views/MapView");
const dataView = () => import("../../views/Data");
const settingsView = () => import("../../views/Settings");
views.register({ id: "map", label: "Map", key: "m", order: 10, icon: "navmap", preload: mapView, View: lazy(() => mapView().then((m) => ({ default: m.MapView }))) });
views.register({ id: "data", label: "Data", key: "d", order: 30, icon: "navdata", preload: dataView, View: lazy(() => dataView().then((m) => ({ default: m.DataView }))) });
views.register({ id: "settings", label: "Settings", key: "s", order: 40, icon: "navgear", side: false, preload: settingsView, View: lazy(() => settingsView().then((m) => ({ default: m.SettingsView }))) });

// On the desktop, beside Data and Settings: their sections, to jump between. The map keeps the
// farm's sidebar (the default).
sidebars.register({ id: "data", Sidebar: OutlineSidebar });
sidebars.register({ id: "settings", Sidebar: OutlineSidebar });

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
