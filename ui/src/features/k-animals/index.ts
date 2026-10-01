// K-animals: the Herd view (h), animal pages, collar cards, and the Herd table's own
// bulk actions. Animals changing anywhere refreshes the store.

import { lazy } from "react";
import { herdBulk, printPages, sidebars, views } from "../../registry";
import { store } from "../../store";
import { kAnimalsSlice } from "../../store/k-animals";
import { kAnimals } from "../../api/k-animals";
import { each } from "./herd";
import "../../styles/k-animals.css";

const herdView = () => import("./HerdView");
views.register({ id: "herd", label: "Herd", key: "h", order: 20, icon: "navherd", preload: herdView, View: lazy(() => herdView().then((m) => ({ default: m.HerdView }))) });
// On the desktop, beside the Herd view: the animals, to step between their pages.
const herdSidebar = () => import("./Sidebar");
sidebars.register({ id: "herd", preload: herdSidebar, Sidebar: lazy(() => herdSidebar().then((m) => ({ default: m.AnimalsSidebar }))) });
printPages.register({ id: "cards", Page: lazy(() => import("./Cards").then((m) => ({ default: m.CardsPage }))) });

// Move and park need a choice (which herd, why); the Herd view asks for it.
herdBulk.register({ id: "move", label: "Move to herd", order: 10, minRole: "manager", run: async (rows) => kAnimalsSlice.patch({ pending: { kind: "move", rows } }) });
herdBulk.register({ id: "park", label: "Park collar", order: 20, minRole: "hand", run: async (rows) => kAnimalsSlice.patch({ pending: { kind: "park", rows } }) });
herdBulk.register({
  id: "unpark", label: "Unpark", order: 30, minRole: "hand",
  run: (rows) => each(rows.filter((r) => r.collar?.parked_at), (r) => kAnimals.unpark(r.collar!.id)),
});

// Imports, removals and links elsewhere (another tab, a text) show here soon after.
let timer: ReturnType<typeof setTimeout> | undefined;
store.on("animals_changed", () => {
  clearTimeout(timer);
  timer = setTimeout(() => void store.refresh(), 400);
});
