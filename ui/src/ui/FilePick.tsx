import { useRef, useState, type ReactNode } from "react";
import { Button } from "./Button";

// A button that opens the file chooser, and takes a file dropped on it.
export function FilePick({ accept, onPick, children, disabled, small = true }: {
  accept?: string; // e.g. ".csv,text/csv"
  onPick: (file: File) => void;
  children: ReactNode;
  disabled?: boolean;
  small?: boolean;
}) {
  const input = useRef<HTMLInputElement>(null);
  const [over, setOver] = useState(false);
  return (
    <span className={"filepick" + (over ? " over" : "")}
      onDragOver={(e) => { if (disabled) return; e.preventDefault(); setOver(true); }}
      onDragLeave={() => setOver(false)}
      onDrop={(e) => {
        e.preventDefault();
        setOver(false);
        const f = e.dataTransfer.files[0];
        if (f && !disabled) onPick(f);
      }}>
      <Button small={small} disabled={disabled} onClick={() => input.current?.click()}>{children}</Button>
      <input ref={input} type="file" accept={accept} hidden onChange={(e) => {
        const f = e.target.files?.[0];
        e.target.value = ""; // the same file again still fires
        if (f) onPick(f);
      }} />
    </span>
  );
}
