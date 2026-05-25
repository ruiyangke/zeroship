/**
 * Node-side shim for the "zeroship" virtual module.
 *
 * Inside the V8 runtime the kernel synthesizes a real module from
 * `ZEROSHIP_MODULE_JS` that wires env/waitUntil/getRequest to native
 * callbacks. That module is never resolved via Node's resolver.
 *
 * Node-based SDK unit tests, however, do go through the resolver — so
 * we ship this tiny file-linked stub to keep `import { env } from "zeroship"`
 * from erroring with ERR_MODULE_NOT_FOUND at test time.
 *
 * ## Mutability contract
 *
 * Production `env` is frozen (via `Object.freeze` in ZEROSHIP_MODULE_JS).
 * This test-time stub is intentionally mutable — tests set
 * `env.auth = { getUser: ... }` to simulate a registered AuthPlugin,
 * then clear it. SDK code must only READ env, not mutate it, so this
 * divergence from prod semantics is safe in practice.
 */

/** Test-time env. Mutable so tests can inject plugin namespaces. */
export const env = {};

/** Test-time waitUntil — accepts a promise, drops it on the floor. */
export function waitUntil(_promise) {
  // No-op in test environment. Real implementation calls __zs_wait_until().
}

/** Test-time getRequest — always throws. Tests never call this. */
export function getRequest() {
  throw new Error(
    "getRequest is not available outside the zeroship V8 runtime — " +
    "this stub exists only so SDK unit tests can resolve the 'zeroship' module."
  );
}

function unavailable(name) {
  return () => {
    throw new Error(
      `${name} is not available outside the zeroship V8 runtime — ` +
      "this stub exists only so SDK unit tests and Node-side builds can resolve the 'zeroship' module."
    );
  };
}

export const runQuery = unavailable("runQuery");
export const runMutation = unavailable("runMutation");
export const currentUser = unavailable("currentUser");
export const currentRequestId = unavailable("currentRequestId");
export const currentTraceId = unavailable("currentTraceId");
export const currentSignal = unavailable("currentSignal");
export const currentHeaders = unavailable("currentHeaders");
export const currentIdempotencyKey = unavailable("currentIdempotencyKey");
