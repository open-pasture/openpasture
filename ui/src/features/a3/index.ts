// A3: how texts reach the farm (Settings > Texting rows), the morning brief time beside
// Daily, and each person's Morning brief. Texts in show in A-notify's Data > Messages. The
// parts load with Settings, so the first bundle doesn't carry them.

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { peopleRow, settingsSections } from "../../registry";
import { textingRows } from "../a-notify/registry";
import "./a3.css";

function later<P extends object>(load: () => Promise<ComponentType<P>>): ComponentType<P> {
  const Part = lazy(() => load().then((C) => ({ default: C })));
  return (props: P) => createElement(Suspense, { fallback: null }, createElement(Part, props));
}

// /api/texting* is the owner's.
textingRows.register({ id: "a3-replies", order: 10, minRole: "owner", Section: later(() => import("./Replies").then((m) => m.Replies)) });
settingsSections.register({
  id: "a3-brief", group: "Daily", label: "Daily", order: 21, minRole: "owner",
  Section: later(() => import("./Brief").then((m) => m.BriefTime)),
});
peopleRow.register({ id: "a3-brief", order: 70, minRole: "owner", Section: later(() => import("./PersonBrief").then((m) => m.PersonBrief)) });
