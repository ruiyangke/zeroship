// sdks/vite-plugin/src/dev-auth-config.ts
//
// Resolve the plugin's `devAuth` option into the env-var pair the spawned
// `zeroship serve` child reads:
//
//   ZEROSHIP_DEV_AUTH        — JSON dev-user config (`@zeroship/bootstrap`'s
//                              `parseDevAuthConfig` consumes it).
//   ZEROSHIP_DEV_AUTH_SECRET — per-dev-server HMAC secret. Both the JS dev-auth
//                              provider (cookie signing) and the runtime's
//                              `dev_auth.rs` (cookie verification → server-side
//                              identity) read it. Generated fresh per dev server.
//
// Dev-only by construction: these env vars are set only on the dev `serve`
// child; the production `.zship` build never sees them, and the dev-auth
// provider lives in `@zeroship/bootstrap/dev` (absent from the shipped worker).

import type { DevAuthUser } from "./index.js";

/** The plugin-facing `devAuth` option shape. */
export type DevAuthOption =
  | boolean
  | DevAuthUser
  | { user: DevAuthUser }
  | { users: DevAuthUser[]; defaultUserId?: string };

/** Resolved env-var pair to merge into the child `spawn` env. */
export interface ResolvedDevAuthEnv {
  /** `null` when dev-auth is disabled — caller omits both env vars. */
  config: string | null;
  /** `null` when disabled. */
  secret: string | null;
}

/**
 * Serialize the `devAuth` option into the `ZEROSHIP_DEV_AUTH` JSON the
 * bootstrap dev-auth provider parses, and mint a fresh HMAC secret.
 *
 * - `undefined` / `true` → ON with the built-in default user (`config = "1"`).
 * - `false` → OFF (`config = null`, no secret) — `/__zeroship/auth/*` falls through
 *   to the user module and `env.auth.getUser()` is anonymous.
 * - a single user object / `{ user }` → one configured user.
 * - `{ users, defaultUserId? }` → multi-user (the dev picker renders for >1).
 *
 * `generateSecret` is injectable for deterministic tests; defaults to a
 * 32-byte hex random.
 */
export function resolveDevAuthEnv(
  option: DevAuthOption | undefined,
  generateSecret: () => string,
): ResolvedDevAuthEnv {
  if (option === false) return { config: null, secret: null };

  const secret = generateSecret();

  if (option === undefined || option === true) {
    // Default ON: the built-in dev user. `"1"` is the bootstrap sentinel.
    return { config: "1", secret };
  }

  // `{ users: [...] }`
  if (typeof option === "object" && "users" in option && Array.isArray(option.users)) {
    const payload: { users: DevAuthUser[]; defaultUserId?: string } = { users: option.users };
    if (option.defaultUserId) payload.defaultUserId = option.defaultUserId;
    return { config: JSON.stringify(payload), secret };
  }

  // `{ user: {...} }`
  if (typeof option === "object" && "user" in option && option.user) {
    return { config: JSON.stringify({ user: option.user }), secret };
  }

  // A bare single-user object.
  return { config: JSON.stringify({ user: option as DevAuthUser }), secret };
}
