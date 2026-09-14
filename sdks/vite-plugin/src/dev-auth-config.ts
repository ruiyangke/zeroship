// sdks/vite-plugin/src/dev-auth-config.ts
//
// Resolve the plugin's `devAuth` option into Vite-owned provider config and a
// cookie HMAC secret shared with the spawned `zeroship serve` process. The
// production build never imports this module.

import type { DevAuthUser } from "./index.js";

/** The plugin-facing `devAuth` option shape. */
export type DevAuthOption =
  | boolean
  | DevAuthUser
  | { user: DevAuthUser }
  | { users: DevAuthUser[]; defaultUserId?: string };

/** Resolved env-var pair to merge into the child `spawn` env. */
export interface ResolvedDevAuth {
  /** `null` when dev-auth is disabled — caller omits both env vars. */
  config: string | null;
  /** `null` when disabled. */
  secret: string | null;
}

/**
 * Serialize the `devAuth` option for the Vite auth provider and mint its HMAC
 * secret. The secret is also passed to the runtime for cookie verification.
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
export function resolveDevAuth(
  option: DevAuthOption | undefined,
  generateSecret: () => string,
): ResolvedDevAuth {
  if (option === false) return { config: null, secret: null };

  const secret = generateSecret();

  if (option === undefined || option === true) {
    // Default ON: the provider expands `"1"` to its built-in user.
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
