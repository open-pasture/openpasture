// One way for a button to write: busy while it runs, the server's sentence in place when it
// fails, and Try again. Nothing a farmer presses fails without a word.
//   const stop = useAction(() => api.stopMove(herdId), { after: () => store.refreshHerd() });
//   <Button disabled={stop.busy} onClick={() => stop.run()}>Stop the move</Button>
//   <ActionError action={stop} />   (shell/ActionError.tsx)

import { useMemo, useRef, useState } from "react";
import { isAbort } from "../api/http";

export interface ActionState { busy: boolean; error?: string }

export interface Action<A extends unknown[]> extends ActionState {
  // Resolves true when the write went through, false when it failed (the error is kept).
  run: (...args: A) => Promise<boolean>;
  // The last call again.
  retry: () => Promise<boolean>;
  clear: () => void;
}

// What a failure says: the server's own sentence, or why there was no answer.
export function errorText(e: unknown): string {
  if (e instanceof Error && e.message) return e.message;
  return "That didn't work. Try again.";
}

// The state machine without React, so it can be tested and shared.
export function createAction<A extends unknown[], R>(
  f: (...args: A) => Promise<R>,
  onChange: (s: ActionState) => void,
  after?: (r: R, ...args: A) => unknown,
) {
  let last: A | undefined;
  let running = 0;
  const run = async (...args: A): Promise<boolean> => {
    last = args;
    const mine = ++running;
    onChange({ busy: true });
    try {
      const r = await f(...args);
      await after?.(r, ...args);
      if (mine === running) onChange({ busy: false });
      return true;
    } catch (e) {
      if (mine === running) onChange(isAbort(e) ? { busy: false } : { busy: false, error: errorText(e) });
      return false;
    }
  };
  return {
    run,
    retry: () => (last ? run(...last) : Promise.resolve(false)),
    clear: () => onChange({ busy: false }),
  };
}

export function useAction<A extends unknown[], R>(f: (...args: A) => Promise<R>, opts: { after?: (r: R, ...args: A) => unknown } = {}): Action<A> {
  const [state, setState] = useState<ActionState>({ busy: false });
  // The latest f and after, so run() keeps one identity while the component re-renders.
  const latest = useRef({ f, after: opts.after });
  latest.current = { f, after: opts.after };
  const machine = useMemo(
    () => createAction<A, R>((...a) => latest.current.f(...a), setState, (r, ...a) => latest.current.after?.(r, ...a)),
    [],
  );
  return { ...state, ...machine };
}
