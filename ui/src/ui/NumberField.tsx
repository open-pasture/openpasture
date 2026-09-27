import { useEffect, useState } from "react";
import { useUnits, type Quantity } from "../units";

// A number in the farm's units with its unit after it: "[16] ft". Takes and gives SI
// (m, ha, cm, kg, m²/hd); saves on Enter or blur, Esc puts the last value back. A typed
// unit wins, so "5 m" works on an imperial farm.
export function NumberField({ value, quantity, onChange, min, max, label, disabled, width = 6 }: {
  value: number | undefined;
  quantity: Quantity;
  onChange: (si: number) => void;
  min?: number; // SI
  max?: number; // SI
  label: string;
  disabled?: boolean;
  width?: number; // characters
}) {
  const u = useUnits();
  const shown = value === undefined ? "" : String(u.toDisplay(value, quantity));
  const [text, setText] = useState(shown);
  useEffect(() => setText(shown), [shown]);

  const commit = () => {
    const si = u.parse(text, quantity);
    if (si === undefined || (min !== undefined && si < min) || (max !== undefined && si > max)) return setText(shown);
    if (value === undefined || u.toDisplay(si, quantity) !== u.toDisplay(value, quantity)) onChange(si);
  };

  return (
    <span className="numf">
      <input className="input sm mono" inputMode="decimal" spellCheck={false} autoComplete="off" aria-label={label}
        style={{ width: `calc(${width}ch + 26px)` }} value={text} disabled={disabled}
        onChange={(e) => setText(e.target.value)} onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") { e.preventDefault(); commit(); }
          if (e.key === "Escape") { e.stopPropagation(); setText(shown); }
        }} />
      <span className="mono dim">{u.unitLabel(quantity)}</span>
    </span>
  );
}
