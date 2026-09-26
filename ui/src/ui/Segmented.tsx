import { useRef, type KeyboardEvent, type ReactNode } from "react";

// The website's .seg switch. Arrow keys move the choice.
export function Segmented<T extends string>({ value, options, onChange, label }: {
  value: T; options: { value: T; label: ReactNode; title?: string }[]; onChange: (v: T) => void; label: string;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const onKey = (e: KeyboardEvent) => {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    e.preventDefault();
    const i = options.findIndex((o) => o.value === value);
    const n = options[(i + (e.key === "ArrowRight" ? 1 : options.length - 1)) % options.length];
    onChange(n.value);
    requestAnimationFrame(() => ref.current?.querySelector<HTMLButtonElement>("[aria-checked=true]")?.focus());
  };
  return (
    <div className="seg" role="radiogroup" aria-label={label} ref={ref} onKeyDown={onKey}>
      {options.map((o) => (
        <button key={o.value} type="button" role="radio" aria-checked={o.value === value}
          tabIndex={o.value === value ? 0 : -1} title={o.title} onClick={() => onChange(o.value)}>
          {o.label}
        </button>
      ))}
    </div>
  );
}
