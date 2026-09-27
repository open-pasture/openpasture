// Protocol v1 on the server (E-srv): the collar's firmware and held boundaries on the animal
// page, and the collars' report cadence in Settings > Collars.

import { animalPage, settingsSections } from "../../registry";
import { CollarSlotsLine } from "./CollarSlots";
import { Reporting } from "./Reporting";
import "../../styles/e-srv.css";

animalPage.register({ id: "e-srv.slots", order: 30, when: (p) => !!p.collar, Section: CollarSlotsLine });
settingsSections.register({ id: "e-srv.reporting", group: "Collars", label: "Reporting", order: 80, minRole: "manager", Section: Reporting });
