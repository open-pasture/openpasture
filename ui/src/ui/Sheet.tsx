import { useEffect, type ReactNode } from "react";

// A side sheet over the map. Esc closes it.
export function Sheet({ open, onClose, children, label }: { open: boolean; onClose: () => void; children: ReactNode; label: string }) {
  useEffect(() => {
    if (!open) return;
    const k = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", k);
    return () => window.removeEventListener("keydown", k);
  }, [open, onClose]);
  if (!open) return null;
  return (
    <aside className="sheet" role="dialog" aria-label={label}>
      {children}
    </aside>
  );
}
