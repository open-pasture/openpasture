// Settings > Advanced: whether the Fleet view is in the rail. Kept per browser.

import { createSlice } from "../../store/slice";

const KEY = "op.fleet";
const stored = () => {
  try {
    return localStorage.getItem(KEY) === "on";
  } catch {
    return false;
  }
};

export const advanced = createSlice(stored());
advanced.subscribe(() => {
  try {
    localStorage.setItem(KEY, advanced.get() ? "on" : "off");
  } catch {
    /* private mode: the toggle lasts this visit */
  }
});
