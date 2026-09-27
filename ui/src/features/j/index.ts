// J: people, roles and sign-in. Settings > People for the owner, Settings > You for everyone
// else; the #/join/<code> page is App's (it shows before any sign-in).

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { settingsSections } from "../../registry";
import "../../styles/j.css";

// Loaded when Settings first shows them, without holding up the rest of Settings.
const later = (load: () => Promise<ComponentType>): ComponentType => {
  const C = lazy(() => load().then((c) => ({ default: c })));
  return () => createElement(Suspense, { fallback: null }, createElement(C));
};

settingsSections.register({ id: "you", group: "You", label: "You", order: 45, Section: later(() => import("./You").then((m) => m.You)) });
settingsSections.register({
  id: "people", group: "People", label: "People", order: 50, minRole: "owner",
  Section: later(() => import("./People").then((m) => m.People)),
});
