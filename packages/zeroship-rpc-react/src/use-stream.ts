// packages/zeroship-rpc-react/src/use-stream.ts
//
// `useStream({ key, stream })` — accumulates yields from an
// AsyncIterable into a `chunks: T[]` state. Used by procedures
// declared `kind: "stream"` (the `__makeProcedure` getter wires it
// up automatically).
//
// Lifecycle:
//
//   - On mount: kick off the iterator, push each yield into a stable
//     array, re-render after each yield.
//   - On unmount: call iterator.return() so the upstream connection
//     closes promptly.
//   - On `cancel()`: same as unmount — iterator.return() + flip
//     `isStreaming` to false.
//
// We intentionally don't use React Query's experimental stream support
// because (a) it's not stable yet, and (b) our wire is the AI-SDK Data
// Stream Protocol, not React Query's chunk format.

import { useEffect, useRef, useState } from "react";

export interface UseStreamConfig<T> {
  /** Stable key for this stream — matches `proc.queryKey(input)`. */
  key: unknown[];
  /** Factory invoked once on mount (and on key change). */
  stream: () => AsyncIterable<T> | AsyncIterableIterator<T>;
}

export interface UseStreamResult<T> {
  /** Yields collected so far. Stable identity across renders for the same stream. */
  chunks: T[];
  /** True until the iterator returns done OR throws. */
  isStreaming: boolean;
  /** Set when the iterator throws. Survives across renders until the key changes. */
  error: Error | null;
  /** Cancel the upstream connection. Idempotent. */
  cancel: () => void;
}

/**
 * Hook implementation. The accumulated chunks live in a ref + state
 * pair: the ref is for write-throughput (no double-buffering on each
 * yield), the state is for triggering a re-render. We push to the
 * array AND immediately replace the state-array reference so consumers
 * who depend on `chunks` see a fresh reference each yield.
 */
export function useStream<T>(config: UseStreamConfig<T>): UseStreamResult<T> {
  const [chunks, setChunks] = useState<T[]>([]);
  const [isStreaming, setIsStreaming] = useState<boolean>(true);
  const [error, setError] = useState<Error | null>(null);

  // Hold onto the active iterator so `cancel()` and unmount can call
  // `return()` on it.
  const iterRef = useRef<AsyncIterator<T> | null>(null);
  // Track the key as a JSON string so we restart the stream when
  // it changes. Comparing arrays directly with === always re-fires.
  const keyJson = stableKey(config.key);

  const cancel = () => {
    const it = iterRef.current;
    iterRef.current = null;
    setIsStreaming(false);
    if (it && typeof it.return === "function") {
      // Best-effort. The iterator may have already finished; we don't
      // surface a return-error to the consumer.
      it.return().catch(() => {});
    }
  };

  useEffect(() => {
    let aborted = false;
    setChunks([]);
    setError(null);
    setIsStreaming(true);

    const iterable = config.stream();
    const it: AsyncIterator<T> =
      Symbol.asyncIterator in iterable
        ? (iterable as AsyncIterable<T>)[Symbol.asyncIterator]()
        : (iterable as AsyncIterableIterator<T>);
    iterRef.current = it;

    (async () => {
      try {
        while (!aborted) {
          const step = await it.next();
          if (aborted) return;
          if (step.done) {
            setIsStreaming(false);
            return;
          }
          setChunks((prev) => {
            // New array each yield so consumers depending on `chunks`
            // (e.g. via dep arrays) see a fresh reference.
            const next = prev.slice();
            next.push(step.value);
            return next;
          });
        }
      } catch (err) {
        if (aborted) return;
        const e = err instanceof Error ? err : new Error(String(err));
        setError(e);
        setIsStreaming(false);
      }
    })();

    return () => {
      aborted = true;
      const live = iterRef.current;
      iterRef.current = null;
      if (live && typeof live.return === "function") {
        live.return().catch(() => {});
      }
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keyJson]);

  return { chunks, isStreaming, error, cancel };
}

/** JSON-serialize the key with a stable property order for deps. */
function stableKey(key: unknown[]): string {
  try {
    return JSON.stringify(key, (_k, v) => {
      if (v && typeof v === "object" && !Array.isArray(v)) {
        const sorted: Record<string, unknown> = {};
        for (const k of Object.keys(v as object).sort()) {
          sorted[k] = (v as Record<string, unknown>)[k];
        }
        return sorted;
      }
      return v;
    });
  } catch {
    // Fall back to a structural-ish key — same identity ⇒ same string.
    return Array.isArray(key) ? key.map((k) => String(k)).join("|") : String(key);
  }
}
