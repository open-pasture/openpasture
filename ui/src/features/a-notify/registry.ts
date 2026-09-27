// Rows other streams add below the channel form in Settings > Texting (A3: how replies
// come in, the morning brief time).

import { createRegistry, type Section } from "../../registry";

export type TextingRowProps = Record<string, never>;
export const textingRows = createRegistry<Section<TextingRowProps>>("textingRows");
