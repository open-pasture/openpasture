// Picking an animal on a touch screen: a tap on it (the map view), a search hit, an alert about
// one animal or its row in the panel. The pick shows as a ring and the one line "214  340 m NE";
// the sheet drops to its peek so the map shows. A mouse keeps hover names and click-to-open.

import { picked, sheet } from "../../store/m";
import { touchUI } from "./phone";

// Let the map show: the herd panel back to its peek (a no-op off the phone layout).
export function showMap() {
  if (sheet.get().state !== "peek") sheet.patch({ state: "peek" });
}

export function pickAnimal(collarId: string | null) {
  if (!touchUI()) return;
  picked.set(collarId);
  if (collarId) showMap();
}
