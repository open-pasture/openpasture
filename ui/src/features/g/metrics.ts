// The maps Layers > Coverage switches between. G registers accuracy and fixes; another stream
// adds one (H: fix rate, cell signal) with `coverageMetrics.register(...)` from its own feature
// once /api/coverage serves it, and `available()` false keeps it out of the switch until it
// has cells.

import { createRegistry } from "../../registry";
import { ACCURACY, FIXES_CAME, type MetricItem } from "./model";

export interface CoverageMetricItem extends MetricItem {
  available?(): boolean;
}

export const coverageMetrics = createRegistry<CoverageMetricItem>("coverageMetrics");
coverageMetrics.register(ACCURACY);
coverageMetrics.register(FIXES_CAME);

// Those to offer now, by order.
export const offered = () => coverageMetrics.list().filter((m) => !m.available || m.available());
