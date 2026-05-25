// sdks/vite-plugin/src/constants.ts

/** HTTP endpoints used by the dev bootstrap for module fetch + HMR polling. */
export const MODULE_FETCH_PATH = "/__zeroship_fetch";
export const HMR_POLL_PATH = "/__zeroship_hmr_check";

/** Environment variable names passed to the zeroship child process. */
export const ENV_DEV = "ZEROSHIP_DEV";
export const ENV_VITE_ORIGIN = "ZEROSHIP_VITE_ORIGIN";
export const ENV_ENTRY = "ZEROSHIP_ENTRY";

/** Default port for the zeroship dev runtime. */
export const DEFAULT_DEV_PORT = 3001;

/** Default RPC endpoint path. */
export const DEFAULT_RPC_ENDPOINT = "/_rpc";
