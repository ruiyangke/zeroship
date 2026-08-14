// RpcError classification, and nothing else.
//
// This file used to open with "No react-query in this example (it is not a
// dependency of this package), so this is the whole data layer" and carried a
// bespoke AsyncState union plus a deps-driven useAsync. Both are gone: state
// lives in the query cache now (src/lib/queries.ts), and what remains here is
// the question the cache does NOT answer -- what a given RpcError MEANS, which
// is how a 401 gets told apart from a 500 at every layer above.
import { isRpcError } from "@zeroship/rpc/client";


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
