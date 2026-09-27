// K-files: a position file waiting in its preview (picked on the map, shown in Data),
// the imports so far, and which one Data replays.

import type { PositionImport, PositionPreview } from "../api/k-files";
import { files } from "../api/k-files";
import { createSlice } from "./slice";

export interface KFiles { pending?: PositionPreview; imports?: PositionImport[]; selected?: string }

export const kfiles = createSlice<KFiles>({});

export async function loadImports() {
  try {
    const imports = await files.imports();
    const sel = kfiles.get().selected;
    kfiles.patch({ imports, selected: imports.some((i) => i.id === sel) ? sel : imports[0]?.id });
  } catch {
    kfiles.patch({ imports: [] });
  }
}
