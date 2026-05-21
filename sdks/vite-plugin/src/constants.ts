// sdks/vite-plugin/src/constants.ts

/** WebSocket path for HMR channel, appended to Vite's HTTP server URL. */
export const WS_PATH = "/__zeroship_hmr";

/** Environment variable names passed to the zeroship child process. */
export const ENV_DEV = "ZEROSHIP_DEV";
export const ENV_VITE_WS = "ZEROSHIP_VITE_WS";
export const ENV_ENTRY = "ZEROSHIP_ENTRY";
/**
 * Absolute path to the user's DB schema module, when Stage 1's resolver
 * picked a split-file convention (`src/schema.ts` / `src/schema/index.ts`)
 * or the user supplied an explicit `schema:` plugin option. Unset when
 * the resolver fell back to "look at the entry's default.schema at
 * runtime" — dev-bootstrap then reads `_zsUser.default?.schema`.
 *
 * Mirrors `manifest.exports.schema` in production: same resolver, same
 * convention chain, so the dev path stays byte-equivalent to the prod
 * path one level above the I/O.
 */
export const ENV_SCHEMA = "ZEROSHIP_SCHEMA_PATH";

/** Default port for the zeroship dev runtime. */
export const DEFAULT_DEV_PORT = 3001;

/** Default RPC endpoint path. */
export const DEFAULT_RPC_ENDPOINT = "/_rpc";
