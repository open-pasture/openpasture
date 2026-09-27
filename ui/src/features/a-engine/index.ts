// Alerts: the top bar count and list (key a), the herd panel's first rows, the map overlay,
// Settings > Alerts, how alerts reach each person, Data > Alerts.

import { dataSections, herdPanel, peopleRow, settingsSections, shortcuts, topbar } from "../../registry";
import { overlays } from "../../map/overlays";
import { store } from "../../store";
import { alertList, alerts, applyAlert, loadAlerts } from "../../store/a-engine";
import "../../styles/a-engine.css";
import { AlertHistory } from "./History";
import { alertOverlay } from "./overlay";
import { HerdAlerts } from "./Panel";
import { PersonAlerts } from "./Person";
import { AlertSettings } from "./Settings";
import { AlertsTopbar } from "./Topbar";

store.on("alert", (e) => applyAlert(e.alert));
store.on("resync", () => void loadAlerts());

topbar.register({ id: "alerts", order: 10, Item: AlertsTopbar });
shortcuts.register({
  id: "alerts", key: "a",
  when: () => alerts.get().list.length > 0,
  run: () => alertList.set({ open: !alertList.get().open }),
});
herdPanel.register({ id: "alerts", order: 10, Section: HerdAlerts });
overlays.register(alertOverlay);
settingsSections.register({ id: "alerts", group: "Alerts", label: "Alerts", order: 60, minRole: "manager", Section: AlertSettings });
peopleRow.register({ id: "alerts", order: 20, minRole: "owner", Section: PersonAlerts });
dataSections.register({ id: "alerts", label: "Alerts", order: 35, Section: AlertHistory });
