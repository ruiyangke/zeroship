// Small fetch helper + RpcError formatting. No react-query in this example
// (it isn't a dependency of this package), so this is the whole data layer:
// a deps-driven fetch-on-mount hook plus consistent error classification.
import {
  useCallback,
  useEffect,
  useRef,
  useState,
  type DependencyList,
} from "react";
import { isRpcError } from "@zeroship/rpc/client";

export type AsyncState<T> =
  | { status: "loading" }
  | { status: "error"; error: unknown }
  | { status: "ready"; data: T };

export function errorCode(error: unknown): string | undefined {
  return isRpcError(error) ? error.code : undefined;
}

export function errorMessage(error: unknown): string {
  if (isRpcError(error)) return error.message;
  if (error instanceof Error) return error.message;
  return String(error);
}

/** A fail-closed procedure rejected because there is no signed-in identity. */
export function isUnauthenticated(error: unknown): boolean {
  return isRpcError(error) && error.code === "UNAUTHENTICATED";
}

export function isPermissionDenied(error: unknown): boolean {
  return isRpcError(error) && error.code === "PERMISSION_DENIED";
}

/**
 * RPC procedures are typed `Output | Promise<Output>`; every browser call
 * is actually async. Wrap a bare call before chaining `.then`/`.catch`
 * directly on it (an `await` inside an async function needs no wrapper).
 */
export function toPromise<T>(value: T | Promise<T>): Promise<T> {
  return Promise.resolve(value);
}

/**
 * Fetch `fn` on mount and whenever `deps` changes. This is intentionally
 * the plainest possible data hook: no cache, no refetch-on-focus, no
 * request de-duplication. `reload()` re-runs the same `fn`, guarding
 * against out-of-order responses with a request sequence number.
 */
export function useAsync<T>(
  // Generated RPC procedures are typed `Output | Promise<Output>` (the
  // union covers a hypothetical synchronous server-side call); every real
  // browser call is asynchronous, so this always wraps with Promise.resolve.
  fn: () => T | Promise<T>,
  deps: DependencyList,
): { state: AsyncState<T>; reload: () => void } {
  const [state, setState] = useState<AsyncState<T>>({ status: "loading" });
  const seq = useRef(0);
  const fnRef = useRef(fn);
  fnRef.current = fn;

  const reload = useCallback(() => {
    const id = ++seq.current;
    setState({ status: "loading" });
    Promise.resolve(fnRef.current())
      .then((data) => {
        if (seq.current === id) setState({ status: "ready", data });
      })
      .catch((error: unknown) => {
        if (seq.current === id) setState({ status: "error", error });
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  useEffect(() => {
    reload();
  }, [reload]);

  return { state, reload };
}
