// Fleet (op-fleet): an advanced area for building and running collars. Units from the factory
// to the farm, factory tests and yield, test plans, firmware rollout, the herd network and
// station tokens, in its own view once Settings > Advanced turns it on; the collar console on
// the animal page; first contact in the herd panel; labels on paper; firmware updates in
// Settings > Collars.

import { lazy } from "react";
import { animalPage, herdPanel, HERD_PANEL, printPages, settingsSections } from "../../registry";
import { advanced } from "./advanced";
import { Console } from "./Console";
import { AdvancedToggle, FirmwareUpdates } from "./Settings";
import { showFleet } from "./view";
import { Waiting } from "./Waiting";
import "../../styles/fleet.css";

if (advanced.get()) showFleet();

settingsSections.register({ id: "fleet.firmware", group: "Collars", label: "Firmware updates", order: 85, minRole: "manager", Section: FirmwareUpdates });
settingsSections.register({ id: "fleet.advanced", group: "Advanced", label: "Fleet", order: 10, minRole: "manager", Section: AdvancedToggle });
animalPage.register({ id: "fleet.console", order: 35, minRole: "manager", when: (p) => !!p.collar, Section: Console });
herdPanel.register({ id: "fleet.waiting", order: HERD_PANEL.addCollar + 1, minRole: "manager", Section: Waiting });
printPages.register({ id: "fleet-labels", Page: lazy(() => import("./Labels").then((m) => ({ default: m.LabelsPage }))) });
