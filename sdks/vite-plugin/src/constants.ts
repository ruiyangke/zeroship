// sdks/vite-plugin/src/constants.ts

/** WebSocket path for HMR channel, appended to Vite's HTTP server URL. */
export const WS_PATH = "/__zeroship_hmr";

/** Environment variable names passed to the zeroship child process. */
export const ENV_DEV = "ZEROSHIP_DEV";
export const ENV_VITE_WS = "ZEROSHIP_VITE_WS";
export const ENV_ENTRY = "ZEROSHIP_ENTRY";

/** Default port for the zeroship dev runtime. */
export const DEFAULT_DEV_PORT = 3001;

/** Default RPC endpoint path. */
export const DEFAULT_RPC_ENDPOINT = "/_rpc";
