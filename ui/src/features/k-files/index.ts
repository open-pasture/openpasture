// K-files: paddock files and position history. Draw > Import file on the map, FSA numbers on
// the paddock sheet, Data > Imports (preview, replay, undo).

import { lazy } from "react";
import { dataSections, paddockSheet } from "../../registry";
import { tools } from "../../map/tools";
import { store } from "../../store";
import { kfiles, loadImports } from "../../store/k-files";
import { FsaLines } from "./Fsa";
import { fsaLines } from "./logic";
import "../../styles/k-files.css";

// Last in the Draw menu, after the shapes you draw by hand.
tools.register({
  id: "import-file", label: "Import file", group: "draw", order: 90, minRole: "manager",
  Tool: lazy(() => import("./ImportTool").then((m) => ({ default: m.ImportTool }))),
});

// Under the paddock's area line, above its notes.
paddockSheet.register({ id: "k-files-fsa", order: 12, when: (p) => fsaLines(p.paddock.props).length > 0, Section: FsaLines });

// Between Replay and Pasture. Renders nothing until there is an import or a file in preview.
dataSections.register({
  id: "k-files", label: "Imports", order: 25,
  Section: lazy(() => import("./Imports").then((m) => ({ default: m.ImportsSection }))),
});

// Another tab's import or undo.
store.on("animals_changed", () => {
  if (kfiles.get().imports) void loadImports();
});
