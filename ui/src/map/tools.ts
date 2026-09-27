// The map's tools bar. Group "bar" tools are buttons; group "draw" tools share one slot
// that is a plain button while it holds one tool and the Draw menu from two on. A tool's
// key starts it; Esc leaves it. While a tool is on, the bar shows its component instead.

import { createElement, useEffect, useState, type ComponentType } from "react";
import type { Map as MLMap } from "maplibre-gl";
import type { AppState, Collar, Herd, Role } from "../api";
import { createRegistry, interleave, sectionNodes, toolFooter, useSections, type ToolFooterProps } from "../registry";
import type { Draw, DrawKind } from "./draw";
import { currentGeometry, type DrawGeometry } from "./drawn";
import type { OverlayCtx } from "./overlays";

// What `when` sees: the farm, the selected herd and that herd's collars.
export interface ToolCtx { state: AppState; herdId?: string; herd?: Herd; collars: Collar[] }

export interface ToolProps {
  map: MLMap;
  draw: Draw;
  herdId?: string;
  ctx: OverlayCtx;
  // Leave the tool: the drawing is cleared and the bar comes back.
  done(): void;
}

export interface ToolItem {
  id: string;
  label: string;
  key?: string;
  group: "bar" | "draw";
  order: number;
  minRole: Role;
  when?: (ctx: ToolCtx) => boolean;
  Tool: ComponentType<ToolProps>;
}

export const tools = createRegistry<ToolItem>("tools");

// A bar tool's order places it in the bar (Boundary 10, Strip 20, Lasso 40); the draw group
// sits at DRAW_ORDER, and a draw tool's order places it in the Draw menu (Paddock 10).
export const DRAW_ORDER = 30;

// Start drawing in `mode` and follow the shape. After the first finish the shape is
// selected for editing (unless edit is false), and every edit updates `geometry`, at most
// once a frame. Leaving clears the drawing.
export function useDrawing(draw: Draw, mode: DrawKind, opts: { edit?: boolean } = {}) {
  const edit = opts.edit ?? true;
  const [drawn, setDrawn] = useState(false);
  const [geometry, setGeometry] = useState<DrawGeometry>();
  useEffect(() => {
    draw.clear();
    draw.setMode(mode);
    setDrawn(false);
    setGeometry(undefined);
    let raf = 0;
    const read = () => {
      raf = 0;
      setGeometry(currentGeometry(draw));
    };
    const onFinish = (id: string | number, ctx: { action: string }) => {
      if (ctx.action !== "draw") return;
      if (edit) {
        draw.setMode("edit");
        draw.selectFeature(id);
      }
      setDrawn(true);
      read();
    };
    const onChange = () => {
      if (!raf) raf = requestAnimationFrame(read);
    };
    draw.on("finish", onFinish);
    draw.on("change", onChange);
    return () => {
      cancelAnimationFrame(raf);
      try {
        draw.off("finish", onFinish);
        draw.off("change", onChange);
        draw.clear();
        draw.setMode("static");
      } catch {
        /* the map went first (leaving the view) */
      }
    };
  }, [draw, mode, edit]);
  return { drawn, geometry };
}

// toolFooter sections for a tool's current shape, in a box under the bar; nothing when none apply.
export function ToolFooter(props: ToolFooterProps) {
  const list = useSections(toolFooter, props);
  return list.length ? createElement("div", { className: "toolfoot" }, interleave([], sectionNodes(list, props))) : null;
}
