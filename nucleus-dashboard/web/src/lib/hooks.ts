import { useCallback, useEffect, useRef, useState } from "react";

// Shared hooks. Anything fetched in more than one place lives here so
// the dashboard tiles (which come last per the agreed build order) can
// reuse the same patterns rather than re-implementing them.

/** State returned by `useFetch` / `usePolling`. */
export type FetchState<T> = {
  data: T | null;
  error: string | null;
  /** True before the first response (success or failure). Goes back to
   *  false on every subsequent refetch — UI should show stale data
   *  rather than flicker between loading states. */
  loading: boolean;
  refetch: () => void;
};

/** Run `fetcher` once on mount and again whenever `deps` change. Errors
 *  surface as strings (caller doesn't need to remember the error
 *  shape). Returns a `refetch` that's safe to wire to a button.
 *
 *  The fetcher receives an `AbortSignal` (ADR-020) — pass it through to
 *  the client helpers (`jsonGet(path, signal)`) and unmount/refetch
 *  genuinely cancels the in-flight request instead of just discarding
 *  the result. Existing closures that ignore the arg keep working; they
 *  just don't get network-level cancellation. */
export function useFetch<T>(
  fetcher: (signal?: AbortSignal) => Promise<T>,
  deps: ReadonlyArray<unknown> = [],
): FetchState<T> {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);

  // Stable handle on the latest fetcher so the effect doesn't re-run when
  // a caller passes an inline closure.
  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;

  useEffect(() => {
    let cancelled = false;
    const ctrl = new AbortController();
    fetcherRef
      .current(ctrl.signal)
      .then((d) => {
        if (cancelled) return;
        setData(d);
        setError(null);
      })
      .catch((e) => {
        if (cancelled) return;
        // Abort is cleanup, not an error state.
        if ((e as Error)?.name === "AbortError") return;
        setError(String(e));
      })
      .finally(() => {
        if (cancelled) return;
        setLoading(false);
      });
    return () => {
      cancelled = true;
      ctrl.abort();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tick, ...deps]);

  const refetch = useCallback(() => setTick((t) => t + 1), []);
  return { data, error, loading, refetch };
}

/** Like `useFetch` but re-runs every `intervalMs`. Pauses while the tab
 *  is hidden (via `document.visibilityState`). Use for surfaces where
 *  the data drifts on its own — health checks, fetch runs, reminder
 *  queues. */
export function usePolling<T>(
  fetcher: () => Promise<T>,
  intervalMs: number,
  deps: ReadonlyArray<unknown> = [],
): FetchState<T> {
  const state = useFetch(fetcher, deps);

  useEffect(() => {
    let timer: ReturnType<typeof setInterval> | null = null;

    const start = () => {
      if (timer !== null) return;
      timer = setInterval(state.refetch, intervalMs);
    };
    const stop = () => {
      if (timer === null) return;
      clearInterval(timer);
      timer = null;
    };

    const onVisibility = () => {
      if (document.visibilityState === "visible") start();
      else stop();
    };

    if (document.visibilityState === "visible") start();
    document.addEventListener("visibilitychange", onVisibility);

    return () => {
      stop();
      document.removeEventListener("visibilitychange", onVisibility);
    };
    // refetch is stable; intervalMs change restarts the interval
  }, [intervalMs, state.refetch]);

  return state;
}

/** Today's date in `YYYY-MM-DD` (local). Recomputed on each render but
 *  cheap; if a surface needs to react to date-rollovers, wire a 1-minute
 *  polling hook around it. */
export function todayLocal(): string {
  const d = new Date();
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}
