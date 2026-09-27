// Which layers have anything to show: each is in the Layers menu only then. Kept apart from the
// fills (ramp.ts) so the app's first bundle carries only this.

import type { PaddockLayer } from "../../api/b";

export type LayerKind = "rest" | "ndvi" | "drought" | "flood";

// Whether a layer has anything to show: it is in the Layers menu only then.
export function hasData(kind: LayerKind, rows: PaddockLayer[]): boolean {
  switch (kind) {
    case "rest": return rows.some((r) => r.grazing || r.rest_days !== undefined);
    case "ndvi": return rows.some((r) => r.ndvi !== undefined);
    case "drought": return rows.some((r) => r.drought !== undefined);
    case "flood": return rows.some((r) => r.flood !== undefined);
  }
}
