// A failed write, said where it happened, with Try again.

import type { Action } from "../store/action";

export function ActionError({ action, className = "" }: { action: Pick<Action<never[]>, "busy" | "error" | "retry">; className?: string }) {
  if (!action.error) return null;
  return (
    <p className={`acterr ${className}`} role="alert">
      <span>{action.error}</span>
      <button type="button" className="acterr-retry" disabled={action.busy} onClick={() => void action.retry()}>Try again</button>
    </p>
  );
}
