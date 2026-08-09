"use server";

// env-probe - the ENVIRONMENT-SURFACE leg of the dev-vs-deployed seam
// comparison.
//
// Sibling of `examples/error-probe` / `examples/auth-probe`: a deliberately
// boring app whose only job is to let `tests/e2e_dev_vs_deployed_env.sh` run ONE
// identical procedure against `pnpm dev` and against the same `.zship` deployed
// behind the gateway, and diff the RESULTS.
//
// THE QUESTION. When a creator's app code runs, what environment variables can
// it observe? Specifically: can it read a variable that exists in the *shell
// that launched the server* but was never deployed to the app?
//
// WHY THIS MUST BE MEASURED AND NOT READ. There are three distinct surfaces,
// and reading any one of them in isolation gives the wrong answer:
//
//   process.env   the Node-compat polyfill (crates/runtime/src/core/init.rs).
//                 Built from `RuntimeState::env_vars` + opt-in exposed secrets
//                 + app vars. WHO FILLS `env_vars` DIFFERS PER TIER, and that
//                 is the whole question.
//   globalThis.__env__
//                 the SAME object aliased for unenv's `node:process` polyfill.
//                 Aliased, not copied - but a probe that only reads
//                 `process.env` would not notice if that ever stopped being
//                 true, so it is read separately here.
//   env           the app-facing `zeroship` module export (also `fetch`'s 2nd
//                 arg). Populated from an `EnvSnapshot`. This is the surface
//                 the platform DOCUMENTS as the creator's env.
//
// A leak on ANY of the three is a leak, so `envp.report` reports all three from
// one call, rather than the harness making three round trips whose results
// might not describe the same process state.
//
// EVERY PROCEDURE IS `auth: "anon"` (src/server/config.ts). That is load-
// bearing, not laziness: a gated procedure is answered by the GATEWAY before
// dispatch, so it never reaches the worker at all and the deployed row would
// describe the gateway's refusal rather than the worker's environment - which
// would read as a clean "no leak".

import { query } from "@zeroship/rpc/server";
import { env } from "zeroship";

/**
 * The name of the CANARY variable. The harness exports this in the shell that
 * launches each tier and NEVER deploys it as an app var. If app code can read
 * it, the host environment reached untrusted code.
 *
 * The VALUE is not baked in here on purpose: the harness generates a fresh
 * random value per run and asserts on that, so a stale value compiled into a
 * fixture can never make a leak assertion vacuously green.
 */
export const CANARY_KEY = "ZS_LEAK_PROBE";

/**
 * The name of the POSITIVE CONTROL variable - one that IS legitimately
 * delivered to the app (control-plane app var on the deployed tier, `ZS_VAR_`
 * prefix on the dev tier). It must be VISIBLE in the same response that reports
 * the canary absent.
 *
 * Without it, "canary absent" is indistinguishable from "this procedure is
 * broken", "the env is empty", or "the probe never ran".
 */
export const CONTROL_KEY = "ZS_ENV_CONTROL";

/**
 * A literal compiled into the bundle and echoed back verbatim. The TRANSPORT
 * control, separate from `CONTROL_KEY`: it proves this exact procedure's body
 * reached the client through the whole dev-server / gateway -> worker path,
 * independently of anything the environment machinery does.
 */
export const FIXTURE_MARKER = "ZSENVP-3c9d-fixture";

/**
 * Host variables worth naming individually. Their PRESENCE on the deployed tier
 * would be a platform-secret leak rather than a canary artefact, and their
 * absence on a tier that leaks the canary would say the exposure is filtered
 * rather than blanket. `DATABASE_URL` is the sharpest of the four: the worker
 * process genuinely has it (`--db` is passed on its command line), so if the
 * deployed app can read it, an app can reach the platform's own database
 * credentials.
 */
const HOST_VAR_NAMES = ["DATABASE_URL", "HOME", "PATH", "PWD"] as const;

/**
 * Worker-INTERNAL variables. Unlike `HOST_VAR_NAMES`, these are supposed to be
 * present in a deployed app - `crates/worker/src/cache.rs` injects them - so
 * they are reported, never asserted against.
 *
 * They are read by name because creator vars are layered OVER them
 * (`crates/runtime/src/core/init.rs`), which means a creator var called
 * `APP_ID` should shadow the platform's own. Only reading the value says
 * whether the documented precedence is the real one.
 */
const WORKER_VAR_NAMES = ["APP_ID", "ZEROSHIP_DEPLOY_ID"] as const;

type Surface = Record<string, unknown> | undefined;

/** Sorted own enumerable keys of a surface, or `null` if the surface is absent. */
function keysOf(o: Surface): string[] | null {
  if (!o || typeof o !== "object") return null;
  return Object.keys(o).sort();
}

/**
 * Read one name off a surface WITHOUT enumerating it.
 *
 * This is deliberately not derived from `keysOf`. `process.env` may be replaced
 * by a Proxy (unenv's `node:process` polyfill installs one over
 * `globalThis.__env__`), and a Proxy can answer a `get` for a name that
 * `ownKeys` never reports. Enumeration alone would therefore miss exactly the
 * kind of exposure this probe exists to detect.
 */
function lookup(o: Surface, name: string): string | null {
  if (!o || typeof o !== "object") return null;
  const v = (o as Record<string, unknown>)[name];
  return typeof v === "string" ? v : v === undefined ? null : String(v);
}

interface SurfaceReport {
  /** `null` when the surface itself does not exist in this runtime. */
  keys: string[] | null;
  count: number;
  /** The canary's VALUE if readable by direct lookup, else `null`. */
  canary: string | null;
  /** The positive control's VALUE if readable by direct lookup, else `null`. */
  control: string | null;
  /** Named host vars, by direct lookup. Value or `null` per name. */
  hostVars: Record<string, string | null>;
  /** Worker-internal vars, by direct lookup. Reported, never asserted. */
  workerVars: Record<string, string | null>;
}

function reportSurface(o: Surface): SurfaceReport {
  const keys = keysOf(o);
  const hostVars: Record<string, string | null> = {};
  for (const n of HOST_VAR_NAMES) hostVars[n] = lookup(o, n);
  const workerVars: Record<string, string | null> = {};
  for (const n of WORKER_VAR_NAMES) workerVars[n] = lookup(o, n);
  return {
    keys,
    count: keys?.length ?? -1,
    canary: lookup(o, CANARY_KEY),
    control: lookup(o, CONTROL_KEY),
    hostVars,
    workerVars,
  };
}

/**
 * `process.env` read EXACTLY as creator code writes it - a bare `process.env`
 * member chain.
 *
 * This spelling is load-bearing. The production `.zship` build runs Vite/rolldown
 * with `ssr.target: "webworker"`, which STATICALLY REWRITES `process.env` to
 * `{}`; `sdks/vite-plugin/src/build.ts` carries `define: { "process.env":
 * "process.env" }` specifically to defeat that rewrite. The define recognises
 * this chain. It does NOT recognise `globalThis.process?.env` - an earlier
 * revision of this fixture used that spelling and the build folded it to a
 * literal `{}`, so the probe reported an empty `process.env` on the deployed
 * tier while `globalThis.__env__` - the SAME V8 object - reported three keys.
 * That is a measurement artefact that reads exactly like a security property,
 * which is why both spellings are now reported side by side.
 */
function bareProcessEnv(): Surface {
  return typeof process === "undefined"
    ? undefined
    : (process.env as unknown as Surface);
}

/**
 * The same object again, reached through a computed key so that no build-time
 * `define` or target rewrite can match it statically.
 *
 * This is the ONE-VARIABLE PARTNER of `bareProcessEnv`: same underlying object,
 * same runtime, only the SPELLING differs. If the two disagree, the difference
 * was introduced by the bundler and not by the platform - a distinction no
 * single reading can make.
 */
function indirectProcessEnv(): Surface {
  const g = globalThis as unknown as Record<string, { env?: unknown } | undefined>;
  const name = ["pro", "cess"].join("");
  return g[name]?.env as Surface;
}

/**
 * Identity of the `process` global, reported so a surface that reads as EMPTY
 * can be told apart from a surface that reads as ABSENT-because-replaced.
 *
 * `crates/runtime/src/core/init.rs` sets `process.env` and `globalThis.__env__`
 * to THE SAME V8 object in one block, and that is the only write to `__env__`
 * in the runtime. So if a tier ever reports different contents for the two, the
 * `process` global it is reading is NOT the one the runtime installed --
 * something replaced it after boot. Without this field that shows up only as an
 * unexplained empty `process.env`, which is easy to misread as "the platform
 * deliberately withholds it".
 */
function runtimeShape() {
  const g = globalThis as unknown as {
    process?: { env?: unknown };
    __env__?: unknown;
  };
  return {
    processType: typeof g.process,
    processEnvType: typeof g.process?.env,
    /** The invariant: true iff `process` is still the runtime's own object. */
    processEnvIsGlobalEnv: g.process?.env === g.__env__,
    processOwnKeys: Object.getOwnPropertyNames(g.process ?? {}).sort(),
  };
}

/**
 * `envp.report` - the one procedure, run identically on both tiers.
 *
 * Returns the FULL key list for each surface, not just a yes/no on the canary.
 * The key list is what distinguishes "nothing leaked" from "a curated subset
 * leaked" from "the entire host environment leaked", and those are three
 * different findings that a boolean would collapse into one.
 */
export const report = query(
  async () => ({
    fixtureMarker: FIXTURE_MARKER,
    canaryKey: CANARY_KEY,
    controlKey: CONTROL_KEY,
    runtimeShape: runtimeShape(),
    // The Node-compat polyfill, read EXACTLY as creator code writes it.
    processEnv: reportSurface(bareProcessEnv()),
    // The same object, reached through an expression no bundler can fold.
    processEnvIndirect: reportSurface(indirectProcessEnv()),
    // The alias unenv's polyfill reads through.
    globalEnv: reportSurface(
      (globalThis as { __env__?: Record<string, unknown> }).__env__,
    ),
    // The documented, app-facing surface.
    appEnv: reportSurface(env as unknown as Record<string, unknown>),
  }),
  { id: "envp.report" },
);
