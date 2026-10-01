// A sidebar that lists the view's sections: every element in the view marked data-outline="Label"
// that shows (an empty section hides, and drops out here too). Clicking one scrolls to it; the one
// being read is marked as the view scrolls.

import { useEffect, useState } from "react";

interface Entry { label: string; el: HTMLElement }

// The section being read sits within this many px of the top of the scrolling view.
const READ_PX = 96;

export function OutlineSidebar() {
  const [list, setList] = useState<Entry[]>([]);
  const [cur, setCur] = useState<string>();

  useEffect(() => {
    const main = document.querySelector<HTMLElement>(".bmain > main");
    if (!main) return;
    let frame = 0;
    const scan = () => {
      frame = 0;
      const found = [...main.querySelectorAll<HTMLElement>("[data-outline]")]
        .filter((el) => el.getClientRects().length > 0 && el.offsetHeight > 0)
        .map((el) => ({ label: el.dataset.outline!, el }));
      setList((prev) => (prev.length === found.length && prev.every((p, i) => p.el === found[i].el && p.label === found[i].label) ? prev : found));
      spy();
    };
    const later = () => {
      if (!frame) frame = requestAnimationFrame(scan);
    };
    const spy = () => {
      const els = [...main.querySelectorAll<HTMLElement>("[data-outline]")].filter((el) => el.offsetHeight > 0);
      if (!els.length) return setCur(undefined);
      const top = main.getBoundingClientRect().top + READ_PX;
      // The last section whose top has passed the reading line; at the very end, the last one.
      const box = scroller(els[0]);
      const atEnd = box && box.scrollTop + box.clientHeight >= box.scrollHeight - 2;
      const passed = els.filter((el) => el.getBoundingClientRect().top <= top);
      setCur((atEnd ? els[els.length - 1] : passed[passed.length - 1] ?? els[0]).dataset.outline);
    };
    scan();
    const mo = new MutationObserver(later);
    mo.observe(main, { childList: true, subtree: true });
    main.addEventListener("scroll", spy, true);
    window.addEventListener("resize", later);
    return () => {
      mo.disconnect();
      main.removeEventListener("scroll", spy, true);
      window.removeEventListener("resize", later);
      if (frame) cancelAnimationFrame(frame);
    };
  }, []);

  if (!list.length) return null;
  return (
    <ul className="olist" aria-label="Sections">
      {list.map((e) => (
        <li key={e.label}>
          <button type="button" aria-current={cur === e.label ? "true" : undefined}
            onClick={() => e.el.scrollIntoView({ behavior: "smooth", block: "start" })}>
            {e.label}
          </button>
        </li>
      ))}
    </ul>
  );
}

// The nearest ancestor that scrolls.
function scroller(el: HTMLElement): HTMLElement | undefined {
  for (let p = el.parentElement; p; p = p.parentElement) {
    const o = getComputedStyle(p).overflowY;
    if ((o === "auto" || o === "scroll") && p.scrollHeight > p.clientHeight) return p;
  }
  return undefined;
}
