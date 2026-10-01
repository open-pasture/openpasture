// M: the phone. Under 760 px the map fills the screen and the herd panel is a bottom sheet (the
// grab, below); tools fold into Draw + Boundary (MapView); tap an animal to pick it and walk to it
// ("you" dot, one mono line); drawing takes a long press to finish; the last known farm shows
// when the server is out of reach. The app installs (manifest, service worker) and takes alerts as
// Web Push notifications when served over https (Settings > You).

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { overlays, type OverlayHandle } from "../../map/overlays";
import { herdPanel, settingsSections, topbar } from "../../registry";
import "../../styles/phone.css";
import { OfflineAge } from "./Age";
import { startOffline } from "./offline-run";
import { startOpen } from "./open";

const later = <P extends object>(C: ComponentType<P>) => (p: P) => createElement(Suspense, { fallback: null }, createElement(C, p));

// Before anything else draws: the grab sits above the herd name (phone only; it loads on use).
const SheetGrab = lazy(() => import("./Sheet").then((m) => ({ default: m.SheetGrab })));
herdPanel.register({ id: "m-sheet", order: 0, Section: later(SheetGrab) });
topbar.register({ id: "m-age", order: 90, at: "status", Item: OfflineAge });

const PushHere = lazy(() => import("./Push").then((m) => ({ default: m.PushHere })));
settingsSections.register({ id: "m-push", group: "You", label: "Alerts on this phone", order: 47, Section: later(PushHere) });

overlays.register({
  id: "m-you",
  slot: "slot-top",
  mount(ctx) {
    let h: OverlayHandle | undefined;
    let gone = false;
    void import("./you").then((m) => {
      if (!gone) h = m.mountYou(ctx);
    });
    return {
      update: () => h?.update?.(),
      destroy() {
        gone = true;
        h?.destroy();
      },
    };
  },
});

startOffline();
startOpen();

// The service worker: the app shell for a phone without signal, and push. Only on a secure page
// (https, or this machine); it never caches /api.
if (typeof navigator !== "undefined" && "serviceWorker" in navigator && window.isSecureContext) {
  addEventListener("load", () => {
    navigator.serviceWorker
      .register("/sw.js", { scope: "/" })
      .then(() => import("./subscribe"))
      .then((m) => m.keepCurrent())
      .catch(() => {});
  });
}
