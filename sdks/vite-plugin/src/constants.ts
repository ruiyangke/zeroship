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

/**
 * Tell the spawned runtime to die when THIS process dies. Value is our pid.
 *
 * `killChild` below is not enough and never can be. It runs in vite; a vite
 * killed by pid - a harness, a crash, the OOM killer - runs no code at all, and
 * the `zeroship serve` child it forked survives holding its listening socket
 * AND an exclusive redb lock on the project's `.zeroship/kv.redb`. Task #221
 * measured four such survivors, aged 10 to 34 minutes, after which no dev
 * server for that example could boot on ANY port, because the contended
 * resource is the state dir and not the port.
 *
 * So the reaping is delegated to the kernel, which is the only party still able
 * to act once we are gone (`PR_SET_PDEATHSIG`, armed by the child - see
 * `crates/cli/src/parent_death.rs`). The pid in the value is what lets the
 * child notice we died BEFORE it got as far as arming.
 *
 * OPT-IN BY CONSTRUCTION: unset means unchanged behaviour, so a `zeroship
 * serve` typed into a terminal does not die because a shell exited.
 */
export const ENV_DIE_WITH_PARENT = "ZEROSHIP_DIE_WITH_PARENT";

/** Default port for the zeroship dev runtime. */
export const DEFAULT_DEV_PORT = 3001;

/**
 * Dev-runtime supervisor thresholds (`dev-server.ts`).
 *
 * The dev server re-spawns the `zeroship serve` child when it exits
 * unexpectedly. That is a real feature - a runtime that ran for minutes and
 * then crashed on a bad request must come back - but an UNBOUNDED restart loop
 * turns a runtime that never starts at all into a silent one, because vite
 * keeps serving HTTP the whole time. These two numbers are what separates the
 * two cases.
 *
 * - `RUNTIME_HEALTHY_MS` - a child that stays up at least this long is treated
 *   as having genuinely started; its later death is a mid-session crash and the
 *   rapid-failure counter resets.
 * - `MAX_RAPID_RESTARTS` - consecutive sub-`RUNTIME_HEALTHY_MS` exits tolerated
 *   before the supervisor gives up and goes terminal.
 *
 * The backoff is 1s, 2s, 4s, 8s (capped), so the terminal verdict lands ~15s
 * after the first failure - long enough that a port being released by a dying
 * previous dev server still recovers, short enough that a creator sees the
 * banner while they are still looking at the terminal.
 */
export const RUNTIME_HEALTHY_MS = 5_000;
export const MAX_RAPID_RESTARTS = 4;
export const RUNTIME_RESTART_BASE_MS = 1_000;
export const RUNTIME_RESTART_MAX_MS = 8_000;

/** Lines of the child runtime's own output retained to explain a failure. */
export const RUNTIME_LOG_TAIL_LINES = 20;

/** Default RPC endpoint path. */
export const DEFAULT_RPC_ENDPOINT = "/_rpc";
