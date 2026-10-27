// Farm rules for the farm agent (Settings > Farm rules), the safety gate's hold in the send
// tools (useGate), and the owner's app token on request. The parts load with the view that
// shows them.

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { settingsSections } from "../../registry";
import "./rules.css";

export { Held, useGate } from "./gate";

const later = (load: () => Promise<ComponentType>): ComponentType => {
  const C = lazy(() => load().then((c) => ({ default: c })));
  return () => createElement(Suspense, { fallback: null }, createElement(C));
};

settingsSections.register({ id: "rules", group: "Farm rules", label: "Farm rules", order: 25, Section: later(() => import("./Rules").then((m) => m.Rules)) });
