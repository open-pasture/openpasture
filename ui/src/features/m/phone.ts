// When the app is on a phone: under 760 px wide (phone.css uses the same width), and when the
// screen is touched rather than hovered (no hover: a tap picks an animal instead of opening it).

import { useSyncExternalStore } from "react";

export const PHONE_MAX_PX = 759;
const PHONE = `(max-width: ${PHONE_MAX_PX}px)`;
const TOUCH = "(hover: none)";

const query = (q: string) => (typeof matchMedia === "function" ? matchMedia(q) : undefined);

export const isPhone = () => !!query(PHONE)?.matches;
// A touch screen without a mouse: nothing can be hovered.
export const isTouch = () => !!query(TOUCH)?.matches;
// Phone layout or a touch screen: the map's tap-to-pick, the "you" dot, bigger drawing handles.
export const touchUI = () => isPhone() || isTouch();

function watch(q: string) {
  return (fn: () => void) => {
    const m = query(q);
    m?.addEventListener("change", fn);
    return () => m?.removeEventListener("change", fn);
  };
}

export const usePhone = () => useSyncExternalStore(watch(PHONE), isPhone, () => false);
export const useTouchUI = () => useSyncExternalStore((fn) => {
  const a = watch(PHONE)(fn), b = watch(TOUCH)(fn);
  return () => (a(), b());
}, touchUI, () => false);
