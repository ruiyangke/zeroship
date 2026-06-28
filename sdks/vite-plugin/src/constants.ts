// sdks/vite-plugin/src/constants.ts

/** HTTP endpoints used by the dev bootstrap for module fetch + HMR polling. */
export const MODULE_FETCH_PATH = "/__zeroship_fetch";
export const HMR_POLL_PATH = "/__zeroship_hmr_check";

/** Environment variable names passed to the zeroship child process. */
export const ENV_DEV = "ZEROSHIP_DEV";
export const ENV_VITE_ORIGIN = "ZEROSHIP_VITE_ORIGIN";
export const ENV_ENTRY = "ZEROSHIP_ENTRY";
export const ENV_RUNTIME_DESCRIPTOR = "ZEROSHIP_RUNTIME_DESCRIPTOR";

/**
 * Dev-tier auth env vars passed to the spawned `zeroship serve` child.
 *
 * - `ENV_DEV_AUTH`        — JSON dev-user config the bootstrap dev-auth provider
 *   parses (`@zeroship/bootstrap` `parseDevAuthConfig`). `"0"`/`"false"`
 *   disables; absent/`"1"` is the built-in default user.
 * - `ENV_DEV_AUTH_SECRET` — per-dev-server HMAC secret. Both the JS dev-auth
 *   provider (cookie signing) and the runtime's `dev_auth.rs` (cookie
 *   verification → server-side identity) read it. Generated fresh per dev
 *   server; never persisted.
 */
export const ENV_DEV_AUTH = "ZEROSHIP_DEV_AUTH";
export const ENV_DEV_AUTH_SECRET = "ZEROSHIP_DEV_AUTH_SECRET";

/** Default port for the zeroship dev runtime. */
export const DEFAULT_DEV_PORT = 3001;

/** Default RPC endpoint path. */
export const DEFAULT_RPC_ENDPOINT = "/_rpc";
