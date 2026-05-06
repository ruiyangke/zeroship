import {
  type AppDefinition,
  type DefinedApp,
  DEFINE_APP_MARKER,
} from "./types.js";

/**
 * Declare an app's resource tree and RPC defaults.
 *
 * Lives at exactly **one** path: `<projectRoot>/src/server/config.ts`.
 * The vite-plugin reads only this file. There is no
 * `zeroship.config.ts` at the project root, no `zeroship.toml` for
 * RPC defaults, no per-directory `$config.ts` — every app-level
 * setting is declared here.
 *
 * The returned object is opaque — its only public guarantee is that
 * it carries the {@link DEFINE_APP_MARKER} symbol so the vite-plugin's
 * manifest emitter can recognize it during AST extraction. The current
 * build reads the literal AST without executing user code; the symbol
 * leaves room for future module-driven loading.
 *
 * @example
 * ```ts
 * // src/server/config.ts
 * import { defineApp } from "@zeroship/server";
 *
 * export default defineApp({
 *   rpc: {
 *     defaults: {
 *       auth:      "user",
 *       rateLimit: { rpm: 600, per: "user" },
 *       timeout:   { ms: 30000 },
 *     },
 *   },
 *   resources: {
 *     "*": { auth: "admin", rateLimit: { rpm: 60, per: "ip" } },
 *     "rpc:todos.delete": { auth: "admin", override: ["auth"] },
 *   },
 * });
 * ```
 */
export function defineApp(definition: AppDefinition = {}): DefinedApp {
  return {
    [DEFINE_APP_MARKER]: true,
    definition,
  };
}
