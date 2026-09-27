// A-notify: Settings > Texting, Verify in each person's row, Data > Messages, and relaying
// texts in Settings > Hosting. The parts load with the view that shows them, so the first
// bundle doesn't carry them.

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { dataSections, peopleRow, settingsSections } from "../../registry";
import "../../styles/a-notify.css";

export { textingRows } from "./registry";

// A section that loads on first render and shows nothing until then.
function later<P extends object>(load: () => Promise<ComponentType<P>>): ComponentType<P> {
  const Part = lazy(() => load().then((C) => ({ default: C })));
  return (props: P) => createElement(Suspense, { fallback: null }, createElement(Part, props));
}

// Channels and secrets are the owner's (/api/notify/* is owner-only).
settingsSections.register({
  id: "a-notify-texting", group: "Texting", label: "Texting", order: 70, minRole: "owner",
  Section: later(() => import("./Texting").then((m) => m.Texting)),
});
settingsSections.register({
  id: "a-notify-hosting", group: "Hosting", label: "Relay", order: 31, minRole: "owner",
  Section: later(() => import("./Hosting").then((m) => m.RelayHosting)),
});
peopleRow.register({ id: "a-notify-verify", order: 60, minRole: "owner", Section: later(() => import("./Verify").then((m) => m.Verify)) });
// The log shows phone numbers: managers and up.
dataSections.register({
  id: "a-notify-messages", label: "Messages", order: 50, minRole: "manager",
  Section: later(() => import("./Messages").then((m) => m.Messages)),
});
