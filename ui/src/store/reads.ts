// A read tied to a component: started when its deps change, cancelled when they change again
// or the component goes, so a view left (or a range changed) stops holding a connection.
//   const rows = useRead(() => api.behaviour({ herd_id, collar_id }), [herd_id, collar_id]);
//   rows.data, rows.error ("couldn't load" is not "nothing to show"), rows.loading

import { useEffect, useState, type DependencyList } from "react";
import { isAbort, scoped, type Lane } from "../api/http";
import { errorText } from "./action";

export interface Read<T> { data?: T; error?: string; loading: boolean }

export function useRead<T>(f: (() => Promise<T>) | null, deps: DependencyList, lane?: Lane): Read<T> {
  const [r, setR] = useState<Read<T>>({ loading: !!f });
  useEffect(() => {
    if (!f) {
      setR({ loading: false });
      return;
    }
    const ac = new AbortController();
    setR((prev) => ({ data: prev.data, loading: true }));
    scoped({ signal: ac.signal, lane }, f).then(
      (data) => !ac.signal.aborted && setR({ data, loading: false }),
      (e) => !ac.signal.aborted && !isAbort(e) && setR((prev) => ({ data: prev.data, error: errorText(e), loading: false })),
    );
    return () => ac.abort();
    // eslint-disable-next-line react-hooks/exhaustive-deps -- the caller's deps are f's inputs
  }, deps);
  return r;
}
