import type { KeyboardEvent, MouseEvent, ReactNode } from "react";
import { Icon } from "./icons";

// An on/off box, with its words beside it when given children. `label` names it for
// screen readers when there are no words.
export function Check({ checked, onChange, children, label, disabled, title, tabIndex }: {
  checked: boolean;
  onChange: (next: boolean, e: MouseEvent | KeyboardEvent) => void;
  children?: ReactNode;
  label?: string;
  disabled?: boolean;
  title?: string;
  tabIndex?: number;
}) {
  return (
    <button type="button" role="checkbox" aria-checked={checked} aria-label={children ? undefined : label} className="check"
      disabled={disabled} title={title} tabIndex={tabIndex}
      onClick={(e) => { e.stopPropagation(); onChange(!checked, e); }}>
      <span className="box">{checked && <Icon name="check" size={8} />}</span>
      {children && <span>{children}</span>}
    </button>
  );
}
