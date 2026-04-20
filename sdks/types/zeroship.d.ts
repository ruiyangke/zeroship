/**
 * Type stubs for the user-facing `zeroship` ESM module.
 *
 * The runtime synthesizes a virtual module named `"zeroship"` that exports
 * three things the SDK packages consume:
 *
 *   - `env`: the composite handler env (plugin namespaces + app secrets),
 *     same reference as the 2nd arg of `fetch(request, env, ctx)`.
 *   - `waitUntil(p)`: extend the request lifetime past its response.
 *   - `getRequest()`: look up the current Request from nested modules.
 *
 * This declaration lets `import { env } from "zeroship"` resolve during
 * TypeScript build. At runtime, the module is provided by the V8 kernel
 * (see `crates/runtime/src/init.rs::ZEROSHIP_MODULE_JS`).
 *
 * The `env` type is deliberately loose: plugin namespaces are attached at
 * runtime based on which plugins the worker registered, and scalar secrets
 * vary per app. Typed subsurfaces (`ZeroshipDb`, `ZeroshipAuth`) live in the
 * sibling .d.ts files — the `env` entries use them as optional hints.
 */

/// <reference path="shared.d.ts" />
/// <reference path="db.d.ts" />
/// <reference path="auth.d.ts" />

declare module "zeroship" {
  /**
   * Composite per-request env — same object as the `env` arg of
   * `fetch(request, env, ctx)`. Plugin namespaces appear under their
   * declared keys ("db", "kv", "storage", ...); app-scoped secrets and
   * variables appear as string keys at the top level.
   *
   * Frozen: direct assignment to properties throws in strict mode.
   */
  export const env: {
    db?: ZeroshipDb;
    auth?: ZeroshipAuth;
    [key: string]: unknown;
  };

  /**
   * Register a promise the runtime should await before the request's
   * isolate slot is released. Lets fire-and-forget work (log flush,
   * webhook retry) finish past the response body write.
   */
  export function waitUntil(promise: Promise<unknown>): void;

  /**
   * Return the Request currently being handled. Throws if called
   * outside a request (e.g. at module-init time).
   */
  export function getRequest(): Request;
}
