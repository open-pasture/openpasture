// Reports (stream I): Data > Reports rows, the printable report page, and the landowner
// line on the paddock sheet. Each part loads when first shown.

import { createElement, lazy, Suspense, type ComponentType } from "react";
import { dataSections, paddockSheet, printPages } from "../../registry";
import "../../styles/i.css";

// A lazily loaded part inside its own Suspense, so loading it doesn't blank the view around it.
function later<P extends object>(load: () => Promise<{ default: ComponentType<P> }>): ComponentType<P> {
  const L = lazy(load);
  return (p: P) => createElement(Suspense, { fallback: null }, createElement(L, p));
}

dataSections.register({ id: "reports", label: "Reports", order: 80, Section: later(() => import("./Reports")) });
printPages.register({ id: "report", Page: later(() => import("./ReportPage")) });
paddockSheet.register({ id: "lease", order: 50, Section: later(() => import("./Lease")) });
