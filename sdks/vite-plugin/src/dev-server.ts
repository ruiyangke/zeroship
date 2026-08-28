// sdks/vite-plugin/src/dev-server.ts
//
// configureServer hook that replaces the old child-process+proxy approach
// with the Vite Environment API.

import type { Plugin, ViteDevServer } from "vite";
import { resolve, dirname } from "node:path";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { ChildProcess, spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { DatabaseSync } from "node:sqlite";
import http from "node:http";
import {
  MODULE_FETCH_PATH,
  HMR_POLL_PATH,
  ENV_DEV,
  ENV_VITE_ORIGIN,
  ENV_ENTRY,
  ENV_RUNTIME_DESCRIPTOR,
  ENV_DEV_AUTH,
  ENV_DEV_AUTH_SECRET,
  ENV_DIE_WITH_PARENT,
  DEFAULT_DEV_PORT,
  ENV_DEV_PORT,
  RUNTIME_HEALTHY_MS,
  MAX_RAPID_RESTARTS,
  RUNTIME_RESTART_BASE_MS,
  RUNTIME_RESTART_MAX_MS,
  RUNTIME_LOG_TAIL_LINES,
} from "./constants.js";
import { resolveDevAuthEnv, type DevAuthOption } from "./dev-auth-config.js";
import {
  ZeroshipDevEnvironment,
  createZeroshipEnvironmentOptions,
} from "./environment.js";
import { findServerEntry } from "./build.js";
import {
  defaultProjectConfig,
  type ProjectConfigHolder,
  type ResolvedProjectConfig,
} from "./project-config/index.js";
import type { TransformState } from "./transform.js";
import { resolveDevDatabase, type DevDatabase } from "./dev-db.js";
import {
  RUNTIME_DESCRIPTOR_FILE,
  genTypesFromMigrations,
  isMigrationSourceError,
} from "./gen-types/index.js";
import { devSqlitePaths, DEV_APP_ID } from "./gen-types/dev-apply.js";
import {
  collectionNamesFrom,
  readGeneratedRuntimeDescriptorAt,
} from "./gen-types/read-descriptor.js";
import {
  DevDatabaseUrlSchemeError,
  logDatabaseUrlSource,
  parseDotenvVars,
  resolveDatabaseUrl,
} from "./dev-database-url.js";

// ── Types ──────────────────────────────────────────────────────────────────

export interface DevServerOptions {
  devServerPort?: number;
  /**
   * Dev-tier auth config. `undefined` defaults to ON with the built-in dev
   * user; `false` disables. See `ZeroshipOptions.devAuth`.
   */
  devAuth?: DevAuthOption;
}

/**
 * The migration paths, resolved from `zeroship.jsonc` (or its schema defaults).
 *
 * Both members are REQUIRED here rather than optional-with-a-fallback. Four
 * places in this file used to spell `?? GEN_TYPES_OUT_DEFAULT`, and a fifth
 * spelling of the same fallback lived in the Rust CLI where it could not agree
 * with any of them. The type is what stops a sixth.
 */
type MigrationPaths = { dir: string; out: string };

type FetchMethod = "fetchModule" | "getBuiltins";

/**
 * What the supervisor currently believes about the `zeroship serve` child.
 *
 * - `ok`      - never crashed, or last child ran past `RUNTIME_HEALTHY_MS`.
 *               Requests are proxied.
 * - `failing` - at least one sub-healthy exit since the last healthy run; a
 *               restart is pending. Requests are NOT proxied (see below).
 * - `fatal`   - `MAX_RAPID_RESTARTS` consecutive sub-healthy exits. No further
 *               restart is scheduled.
 *
 * WHY `failing` STOPS PROXYING RATHER THAN LETTING THE PROXY FAIL NATURALLY.
 * The dev runtime listens on a port of its OWN (`devServerPort`), separate from
 * vite's. When our child cannot bind that port it is because some OTHER process
 * holds it - in practice a second example's dev runtime, since several examples
 * share the 3001 default. Proxying anyway does not produce a connection error:
 * it produces a SUCCESSFUL connection to somebody else's app. Measured on
 * examples/starter + examples/error-probe sharing one runtime port, the starter
 * dev server answered its own `getMessages` with
 * `404 {"message":"Method not found: getMessages"}` - error-probe's runtime
 * replying about a procedure it has never heard of. Two instances of the SAME
 * example are worse still: HTTP 200 carrying the other instance's data.
 * So the guard is not belt-and-braces around a connection refusal; it is the
 * only thing standing between a creator and another app's answers.
 *
 * WHAT THIS DOES NOT CLOSE. The state starts at `ok`, so between vite accepting
 * its first request and the first child exit (measured at 7-65ms for a bind
 * failure, though vite may answer sooner) a request CAN still be forwarded to
 * whoever holds the port. That window is bounded by one process spawn and ends
 * for good at the first exit; the defect this replaces had no end. Closing it
 * entirely would mean proving our own child owns the socket before every
 * proxy - a per-request syscall against a race that resolves itself in
 * milliseconds. Named here so the next reader does not mistake the guard for
 * total.
 */
type RuntimeHealth = "ok" | "failing" | "fatal";

interface RuntimeStatus {
  health: RuntimeHealth;
  /** Consecutive exits faster than `RUNTIME_HEALTHY_MS`. */
  rapidFailures: number;
  /** Tail of the current/last child's own stdout+stderr. */
  logTail: string[];
  /** Port the child was told to bind, for the operator-facing message. */
  port: number;
}

function pushRuntimeLog(status: RuntimeStatus, chunk: string): void {
  for (const line of chunk.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    status.logTail.push(trimmed);
  }
  if (status.logTail.length > RUNTIME_LOG_TAIL_LINES) {
    status.logTail.splice(0, status.logTail.length - RUNTIME_LOG_TAIL_LINES);
  }
}

function restartDelayMs(rapidFailures: number): number {
  if (rapidFailures <= 0) return RUNTIME_RESTART_BASE_MS;
  return Math.min(
    RUNTIME_RESTART_BASE_MS * 2 ** (rapidFailures - 1),
    RUNTIME_RESTART_MAX_MS,
  );
}

/**
 * The request-time answer when the runtime is not running.
 *
 * SHAPED TO `@zeroship/rpc`'s ERROR ENVELOPE ON PURPOSE. The client
 * (`sdks/rpc/src/error.ts`) lifts only `code`, `message`, `details`,
 * `retryable` and `trace_id` off a JSON error body and DISCARDS everything
 * else, falling back to `RPC error: 503 Service Unavailable`. An envelope that
 * put the explanation under a key of its own invention would therefore be
 * thrown away between here and the creator's browser console - the log would
 * say why, and the only surface they were actually looking at would not. So
 * the actionable sentence goes in `message`, and the structured extras ride in
 * `details` where the client preserves them.
 */
function runtimeDownEnvelope(status: RuntimeStatus): Record<string, unknown> {
  const fatal = status.health === "fatal";
  // The runtime's own words are the specific half ("port N is already in use");
  // the frame around them is the generic half. Both are needed: one names the
  // cause, the other says who is reporting it and what to do.
  //
  // Lines the DEV SERVER printed (`[zeroship] Loaded ...`, `[zeroship] Starting
  // server on port ...`) are dropped first. They are boot chatter that is
  // present on every attempt including successful ones, so they carry no
  // information about the failure -- and taking the last line blindly picks up
  // whichever of them happened to come last. The real cause is usually two
  // lines ("port N is already in use" + "Hint: use --port=<N>"), so this keeps
  // the run of unprefixed lines rather than just the final one.
  const causeLines = status.logTail.filter((l) => !l.startsWith("[zeroship]"));
  const cause = (causeLines.length > 0 ? causeLines : status.logTail).join(" ")
    || "no output captured";
  const message = fatal
    ? `zeroship dev runtime failed to start on port ${status.port} and was given up on `
      + `after ${MAX_RAPID_RESTARTS} attempts. The runtime said: ${cause} `
      + "Fix that, then restart `pnpm dev`. If the port is taken by another app, "
      + "set zeroship({ devServerPort: <N> }) in vite.config.ts."
    : `zeroship dev runtime exited immediately on port ${status.port} and is being `
      + `retried (attempt ${status.rapidFailures}/${MAX_RAPID_RESTARTS}). `
      + `The runtime said: ${cause}`;
  return {
    code: "UNAVAILABLE",
    message,
    retryable: !fatal,
    details: {
      state: status.health,
      devServerPort: status.port,
      attempts: status.rapidFailures,
      runtimeOutput: status.logTail,
    },
  };
}

/**
 * The terminal message. It is deliberately loud and deliberately quotes the
 * CHILD's own words: `zeroship serve` already prints the actionable line
 * ("port N is already in use / Hint: use --port=<N>"), and in the failure this
 * exists for, that line scrolled past interleaved with a dozen identical boot
 * banners. Re-stating it once, at the end, next to "giving up", is the whole
 * point - a generic "runtime failed" would leave the creator exactly where the
 * infinite loop did.
 */
/**
 * A runtime whose state dir is locked by an earlier, un-reaped run.
 *
 * This matters because the two remedies are mutually exclusive and the banner
 * used to print the wrong one unconditionally. A state-dir lock is NOT a port
 * clash: the contended resource is `.zeroship/`, so moving `devServerPort`
 * changes nothing (task #221 - four orphaned `zeroship serve` processes held
 * `.zeroship/kv.redb` across four different ports).
 *
 * Keyed on a marker the PLATFORM emits, not on redb's prose. This used to match
 * /Database already open|Cannot acquire lock/ - a third-party library's wording,
 * matched in a different language, with nothing holding the two together. redb
 * could reword that in a patch release and both test suites would stay green
 * while this silently reverted to advising a port change.
 *
 * The prose match now lives in `crates/plugin-kv/src/backend/redb.rs`, next to
 * the crate that produces the prose, and what crosses the language boundary is
 * `STATE_DIR_LOCK_MARKER` - a constant we own on both sides.
 *
 * STILL UNGATED, and worth knowing before trusting this: nothing enforces that
 * the literal below equals the Rust constant. The Rust side asserts the marker
 * reaches a real second-open error
 * (`backend::redb::tests::second_open_names_the_holding_process`); this side
 * asserts the banner keys off it. If someone edits one string only, both suites
 * pass. That is a smaller drift surface than a library's sentence, not a closed
 * one.
 */
const STATE_DIR_LOCK_MARKER = "zs-state-dir-lock";

function looksLikeStateDirLock(tail: readonly string[]): boolean {
  return tail.some((l) => l.includes(STATE_DIR_LOCK_MARKER));
}

export function formatFatalBanner(status: RuntimeStatus): string {
  const rule = "=".repeat(72);
  const lines = [
    rule,
    `  [zeroship] DEV RUNTIME FAILED TO START - giving up after ${MAX_RAPID_RESTARTS} attempts`,
    "",
    `  The runtime exited ${MAX_RAPID_RESTARTS} times in a row without staying up for`,
    `  ${RUNTIME_HEALTHY_MS / 1000}s. Vite is still serving this page, but NOTHING`,
    "  server-side works: every server function now returns HTTP 503.",
    "",
    "  What the runtime itself said:",
  ];
  const tail = status.logTail.length > 0 ? status.logTail : ["(no output captured)"];
  for (const line of tail) lines.push(`    | ${line}`);
  // Exactly one remedy, chosen by what the runtime actually said. Printing the
  // port advice next to a lock error contradicts the runtime's own output,
  // which states that moving the port will NOT help.
  if (looksLikeStateDirLock(tail)) {
    lines.push(
      "",
      "  That is a STATE DIR lock, not a port clash. An earlier run's runtime is",
      "  still holding `.zeroship/` - changing `devServerPort` will NOT help, and",
      "  neither will running on a different port.",
      "",
      "  The line above names the holding process when this user can see it. Kill",
      "  it and restart. If no pid was named, the holder belongs to another user",
      "  or is already gone and the lock file is stale.",
    );
  } else {
    lines.push(
      "",
      `  The dev runtime binds :${status.port}, which is SEPARATE from vite's port -`,
      "  `vite --port N` does not move it. Two apps sharing it collide. Set a",
      "  different one in vite.config.ts:  zeroship({ devServerPort: <N> })",
    );
  }
  lines.push("", "  Fix the cause above, then restart `pnpm dev`.", rule);
  return lines.join("\n");
}
interface FetchInvokePayload {
  name: string;
  data: unknown;
}

const MAX_FETCH_BODY_BYTES = 64 * 1024;
const ALLOWED_FETCH_METHODS = new Set<FetchMethod>(["fetchModule", "getBuiltins"]);

function requestPath(req: http.IncomingMessage): string {
  return new URL(req.url ?? "", "http://localhost").pathname;
}

/**
 * Is `file` inside the migrations dir? Used by `hotUpdate` to decide whether a
 * change should trigger a gen-types regeneration. `file` is an absolute path
 * (Vite normalises to forward slashes); `migrationsAbs` is the resolved dir.
 */
function isUnderMigrationsDir(file: string, migrationsAbs: string): boolean {
  const prefix = migrationsAbs.endsWith("/") ? migrationsAbs : migrationsAbs + "/";
  return file === migrationsAbs || file.startsWith(prefix);
}

/**
 * Migration-first gen-types — REGENERATE the typed `env.db` surface from the
 * migration set in DEV via the in-process gen-types library (no subprocess).
 * Never THROWS; it classifies instead. A fault in the creator's own migration
 * source is logged and survived; anything else (missing addon, engine panic,
 * unwritable out dir) is a PLATFORM fault, printed as such, and fatal at boot.
 * See `MigrationSourceError` in `gen-types/index.ts` and task #269.
 *
 * Dev always WRITES (no `--check`; that is a CI/build generated-artifact concern).
 */
function readGeneratedRuntimeDescriptor(
  root: string,
  migrations: MigrationPaths,
): string | undefined {
  return readGeneratedRuntimeDescriptorAt(resolve(root, migrations.out));
}

async function regenTypesDev(
  root: string,
  migrations: MigrationPaths,
  fatal: boolean,
): Promise<string | undefined> {
  const migrationsDir = resolve(root, migrations.dir);
  const outDir = resolve(root, migrations.out);
  try {
    await genTypesFromMigrations(migrationsDir, outDir, { check: false });
    console.log(
      "[zeroship] gen-types: regenerated env.db.ts + schema.runtime.json from the migrations"
    );
  } catch (e) {
    if (isMigrationSourceError(e)) {
      // Dev: never throw on a MALFORMED MIGRATION. The creator just broke their
      // own source and knows it; a message plus a live server is what they want.
      console.error(`[zeroship] gen-types failed (dev): ${(e as Error).message}`);
    } else {
      // Everything else is OUR bug or their environment: a missing native
      // addon, an unparseable descriptor, an unwritable generated/ dir. Treating
      // any of those like a creator's typo leaves them serving a STALE OR ABSENT
      // descriptor with one line of warning, so every type error afterwards is
      // a lie.
      //
      // AN ENGINE PANIC IS NOT ONE OF THEM AT OUR PIN, and the first version of
      // this comment said it was. `third_party/zero-migrate` @ cb1bcb59 has ZERO
      // `catch_unwind` in the addon crate, so a panic aborts the process before
      // this arm can run; the upstream fix that would change that is not an
      // ancestor of our pin. Verified, not assumed - see the long note on
      // `MigrationSourceError` in gen-types/index.ts and task #271.
      //
      // At boot nothing has been served yet, so refusing to start costs the
      // creator nothing and names the fault while it is still the only thing on
      // screen. On a hot update there is a live session and a running app, so
      // this stays loud but non-fatal — the descriptor already in memory is the
      // one that was serving a moment ago.
      console.error(
        `[zeroship] gen-types PLATFORM FAULT (not your migrations): ${(e as Error).message}`,
      );
      if ((e as { stack?: string }).stack) console.error((e as Error).stack);
      if (fatal) {
        console.error(
          "[zeroship] refusing to start the dev server — env.db.ts and " +
            "schema.runtime.json would be stale, so every type error after this " +
            "point would be a lie. Fix the fault above and re-run `pnpm dev`.",
        );
        process.exit(1);
      }
    }
  }
  return readGeneratedRuntimeDescriptor(root, migrations);
}

/**
 * REPORT — never apply — the dev schema state.
 *
 * Migrating is a SEPARATE, explicit step (`zeroship-dev-migrate`, wired as the
 * example apps' `pnpm migrate`), deliberately not folded into `pnpm dev`. The
 * dev server starts a runtime; it does not mutate the developer's database as a
 * side effect of being started. That mirrors the platform, where `migrated`
 * applies at deploy and the worker only ever reads the schema — and it means a
 * half-written migration cannot be applied by the mere act of running the dev
 * server, nor re-applied by every file-watch restart.
 *
 * What is left here is the diagnostic the coupling used to provide for free: if
 * the app declares collections the dev database does not have, say so, name the
 * command that fixes it, and keep serving. The failure this guards against is
 * silent — a 500 on every `env.db` call with no indication the schema was never
 * created.
 *
 * READ-ONLY by construction: it opens the app file `readonly` and touches
 * nothing else.
 */
function reportDevSchemaState(
  root: string,
  migrations: MigrationPaths,
  descriptorJson: string | undefined,
  databaseUrl: string,
): void {
  const migrationsDir = resolve(root, migrations.dir);
  if (!existsSync(migrationsDir)) return; // not a migration-first app

  // The collections the app expects to exist, from the descriptor gen-types
  // just wrote.
  let expected: string[];
  try {
    expected = collectionNamesFrom(descriptorJson);
  } catch {
    return; // an unreadable descriptor is gen-types' error to report, not ours
  }
  if (expected.length === 0) return;

  const { appPath } = devSqlitePaths(root, DEV_APP_ID, databaseUrl);

  let present: Set<string>;
  try {
    // `node:sqlite` in readonly mode. A missing file throws rather than being
    // created — which is the "never migrated" case, handled below.
    const db = new DatabaseSync(appPath, { readOnly: true });
    try {
      const rows = db.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all() as {
        name: string;
      }[];
      present = new Set(rows.map((r) => r.name));
    } finally {
      db.close();
    }
  } catch {
    present = new Set();
  }

  const missing = expected.filter((name) => !present.has(name));
  if (missing.length === 0) {
    console.log(
      `[zeroship] dev schema present (${expected.length} collection(s) in ${appPath})`
    );
    return;
  }

  console.error(
    `[zeroship] dev schema NOT applied — env.db will fail for: ${missing.join(", ")}\n` +
      `[zeroship]   the database is migrated by a separate, explicit step:\n` +
      `[zeroship]       pnpm migrate\n` +
      `[zeroship]   (${appPath})`
  );
}

function writeJson(
  res: http.ServerResponse,
  statusCode: number,
  payload: unknown,
): void {
  res.writeHead(statusCode, { "Content-Type": "application/json" });
  res.end(JSON.stringify(payload));
}

function httpError(statusCode: number, message: string): Error & { statusCode: number } {
  return Object.assign(new Error(message), { statusCode });
}

function headerValue(value: string | string[] | undefined): string | undefined {
  return Array.isArray(value) ? value[0] : value;
}

function isAllowedFetchMethod(methodName: string): methodName is FetchMethod {
  return ALLOWED_FETCH_METHODS.has(methodName as FetchMethod);
}

function assertJsonRequest(req: http.IncomingMessage): void {
  const contentType = headerValue(req.headers["content-type"]);
  if (!contentType || contentType.split(";", 1)[0].trim().toLowerCase() !== "application/json") {
    throw httpError(415, "zeroship fetch requests must use Content-Type: application/json");
  }
}

function assertFetchBodyLength(req: http.IncomingMessage): void {
  const raw = headerValue(req.headers["content-length"]);
  if (!raw) return;

  const bytes = Number(raw);
  if (!Number.isFinite(bytes) || bytes < 0) {
    throw httpError(400, "invalid Content-Length header");
  }
  if (bytes > MAX_FETCH_BODY_BYTES) {
    throw httpError(413, `zeroship fetch body exceeds ${MAX_FETCH_BODY_BYTES} bytes`);
  }
}

function parseFetchInvoke(body: string): FetchInvokePayload {
  if (body.trim() === "") {
    throw httpError(400, "zeroship fetch body is empty");
  }

  let payload: unknown;
  try {
    payload = JSON.parse(body);
  } catch {
    throw httpError(400, "invalid zeroship fetch JSON");
  }

  if (!payload || typeof payload !== "object") {
    throw httpError(400, "invalid zeroship fetch payload");
  }

  const envelope = payload as {
    type?: unknown;
    event?: unknown;
    data?: unknown;
  };
  if (
    envelope.type !== "custom" ||
    envelope.event !== "vite:invoke" ||
    !envelope.data ||
    typeof envelope.data !== "object"
  ) {
    throw httpError(400, "invalid zeroship fetch payload");
  }

  const invoke = envelope.data as { name?: unknown; data?: unknown };
  if (typeof invoke.name !== "string") {
    throw httpError(400, "invalid zeroship fetch payload");
  }

  return {
    name: invoke.name,
    data: invoke.data,
  };
}

// ── Plugin factory ─────────────────────────────────────────────────────────

export function devServerPlugin(
  options: DevServerOptions,
  state: TransformState,
  project: ProjectConfigHolder,
): Plugin[] {
  const devPort =
    options.devServerPort ??
    (Number(process.env[ENV_DEV_PORT]) || undefined) ??
    DEFAULT_DEV_PORT;
  let projectConfig: ResolvedProjectConfig = defaultProjectConfig();

  // Resolve the dev-tier auth env pair ONCE per dev-server lifetime. The secret
  // is stable across child restarts (the crash-restart handler re-spawns the
  // runtime) so cookies minted before a restart still verify afterward.
  const devAuthEnv = resolveDevAuthEnv(options.devAuth, () =>
    randomBytes(32).toString("hex"),
  );

  let root = "";
  let isDev = false;
  let serverProcess: ChildProcess | null = null;
  let devDb: DevDatabase | null = null;
  let disposeRuntime: (() => void) | null = null;

  // Migration-first gen-types. The absolute migrations dir is resolved in
  // configureServer (once `root` is known) so the `hotUpdate` branch can match
  // changed files against it.
  let migrationsAbs: string | null = null;
  let runtimeDescriptorJson: string | undefined;
  let pendingRuntimeDescriptorJson: string | null | undefined;
  // The boot-time gen-types regen (async, in-process). `spawnRuntime` awaits it
  // so the runtime is spawned WITH a fresh descriptor (the pre-in-process CLI
  // path was synchronous; awaiting here preserves that ordering).
  let bootRegenDone: Promise<unknown> = Promise.resolve();

  // Accumulates file paths changed since the last HMR poll. The V8 runtime
  // polls GET /__zeroship_hmr_check every 500ms via setInterval + fetch().
  // We can't push via WebSocket (V8 has no outbound WS client) or hold a
  // streaming response open (wall timeout would kill it). Polling is the
  // simplest mechanism that works within the runtime's constraints.
  const pendingHmrChanges = new Set<string>();

  // ── Plugin 1: zeroship:environment ──────────────────────────────────────

  const environmentPlugin: Plugin = {
    name: "zeroship:environment",

    config(userConfig) {
      // Detect server entry at config time so optimizeDeps.entries can
      // pre-crawl it. Otherwise Vite's dep optimizer discovers deps lazily
      // as modules import, which causes re-optimization mid-request and
      // "file does not exist" errors from the ModuleRunner on stale URLs.
      const detectedRoot = resolve(userConfig.root ?? process.cwd());
      projectConfig = project.load(detectedRoot);
      const entry =
        projectConfig.build.serverEntry ?? findServerEntry(detectedRoot) ?? undefined;
      return {
        environments: {
          zeroship: createZeroshipEnvironmentOptions(entry),
        },
      };
    },

    configResolved(config) {
      root = config.root;
      isDev = config.command === "serve";
      projectConfig = project.load(root);
    },
  };

  // ── Plugin 2: zeroship:dev-server ────────────────────────────────────────

  const devServerPluginImpl: Plugin = {
    name: "zeroship:dev-server",

    configureServer(server: ViteDevServer) {
      if (!isDev) return;

      // Supervisor state, shared between the crash-restart handler (section 3)
      // and the proxy middleware (section 4) so a known-dead runtime is visible
      // AT REQUEST TIME and not only in a log the creator has scrolled past.
      const runtimeStatus: RuntimeStatus = {
        health: "ok",
        rapidFailures: 0,
        logTail: [],
        port: devPort,
      };

      // 0. Migration-first gen-types — ensure the migrations dir is WATCHED so a
      //    change there fires `hotUpdate` (Vite only watches the module graph +
      //    root by default; a migrations dir holding `.ts` sources not imported
      //    by app code may not be covered). The `hotUpdate` branch below
      //    regenerates `env.db.ts` on a change.
      migrationsAbs = resolve(root, projectConfig.migrations.dir);
      if (existsSync(migrationsAbs)) {
        server.watcher.add(migrationsAbs);
        // Seed the descriptor from the committed artifact so the very first
        // request has it even before the async regen lands.
        runtimeDescriptorJson = readGeneratedRuntimeDescriptor(root, projectConfig.migrations);
        // Initial regen on boot: migrations may have changed while the dev server
        // was down (`hotUpdate` only fires on a *subsequent* change, so without
        // this a fresh `pnpm dev` leaves env.db.ts stale). `spawnRuntime` awaits
        // `bootRegenDone` so the runtime is injected WITH the fresh descriptor.
        // `regenTypesDev` never throws: a malformed migration is logged and
        // survived, a PLATFORM fault exits the process here (`fatal: true`)
        // rather than serving a stale descriptor for the rest of the session.
        bootRegenDone = regenTypesDev(root, projectConfig.migrations, true).then(async (json) => {
          runtimeDescriptorJson = json;
          // Report — do NOT apply. Migrating is `pnpm migrate`, a separate step
          // run ahead of `pnpm dev`; see `reportDevSchemaState`.
          //
          // The DATABASE_URL is resolved HERE using the same
          // `resolveDatabaseUrl` + the same three inputs that `spawnRuntime`
          // uses below, so the file we inspect is the file the worker opens.
          // `DATABASE_URL` is overridable (shell, then `.env`, then the dev
          // default); a hardcoded `.zeroship` would inspect the wrong file
          // whenever a caller redirected it -- see `devSqliteDir`.
          if (!devDb) devDb = resolveDevDatabase(root);
          // A non-SQLite DATABASE_URL is a creator misconfiguration, not a
          // platform fault — report it the same way an invalid migration is
          // reported just above (loud, non-fatal) rather than letting it
          // reject `bootRegenDone` and surface as a bare unhandled rejection.
          // `spawnRuntime` (below) hits the SAME rejection independently and
          // is what actually stops a runtime from being spawned against it.
          let databaseUrl: string;
          try {
            ({ databaseUrl } = resolveDatabaseUrl(process.env, parseDotenvVars(root), devDb.databaseUrl));
          } catch (e) {
            if (!(e instanceof DevDatabaseUrlSchemeError)) throw e;
            console.error(`[zeroship] ${(e as Error).message}`);
            return;
          }
          reportDevSchemaState(root, projectConfig.migrations, json, databaseUrl);
        });
      }

      // 1. Module fetch endpoint ─────────────────────────────────────────
      //
      // The runtime's ModuleRunner calls this to fetch transformed modules
      // from Vite's environment. V8 can't open the bidirectional transport
      // Vite uses for browser HMR, so the dev bootstrap uses plain HTTP.

      server.middlewares.use(
        async (
          req: http.IncomingMessage,
          res: http.ServerResponse,
          next: () => void
        ) => {
          if (requestPath(req) !== MODULE_FETCH_PATH || req.method !== "POST") {
            return next();
          }

          const zeroshipEnv = server.environments[
            "zeroship"
          ] as ZeroshipDevEnvironment | undefined;

          if (!zeroshipEnv) {
            writeJson(res, 500, { error: { message: "zeroship environment not found" } });
            return;
          }

          try {
            assertJsonRequest(req);
            assertFetchBodyLength(req);

            const chunks: Buffer[] = [];
            let bodySize = 0;
            for await (const chunk of req) {
              const buf = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
              bodySize += buf.length;
              if (bodySize > MAX_FETCH_BODY_BYTES) {
                writeJson(res, 413, {
                  error: {
                    message: `zeroship fetch body exceeds ${MAX_FETCH_BODY_BYTES} bytes`,
                  },
                });
                return;
              }
              chunks.push(buf);
            }

            const body = Buffer.concat(chunks).toString("utf8");
            // The ModuleRunner sends vite:invoke calls via the transport.
            // Format: { type: "custom", event: "vite:invoke",
            //           data: { id: correlationId, name: methodName, data: args[] } }
            const invoke = parseFetchInvoke(body);
            const methodName = invoke.name;
            const args = invoke.data;
            if (typeof methodName !== "string" || !isAllowedFetchMethod(methodName)) {
              throw httpError(400, `unsupported zeroship fetch method: ${String(methodName)}`);
            }

            let result: unknown;
            if (methodName === "fetchModule") {
              if (!Array.isArray(args) || typeof args[0] !== "string") {
                throw httpError(400, "fetchModule expects [id, importer?, options?]");
              }
              // args = [id, importer, options?]
              result = await zeroshipEnv.fetchModule(
                args[0],
                typeof args[1] === "string" ? args[1] : undefined,
                args[2] ?? undefined,
              );
            } else {
              // Return EMPTY builtins — our V8 runtime can't import node: modules
              // natively. By returning [], the ModuleRunner will always call
              // fetchModule() for every import, which lets our fetchModule override
              // intercept node:* and return polyfill code.
              result = [];
            }

            // Return in the format the runner expects: { result } or { error }
            writeJson(res, 200, { result });
          } catch (e: any) {
            const statusCode =
              typeof e?.statusCode === "number" ? e.statusCode : 500;
            writeJson(res, statusCode, {
              error: { message: e?.message ?? String(e) },
            });
          }
        }
      );

      // 2. HMR poll endpoint ──────────────────────────────────────────────
      //
      // The V8 runtime polls this every 500ms to discover changed files.
      // Returns the pending set and clears it atomically. Empty array = no
      // changes. The runtime uses the paths to invalidate its ModuleRunner
      // evaluated-modules cache so the next import() re-fetches from Vite.

      server.middlewares.use(
        (
          req: http.IncomingMessage,
          res: http.ServerResponse,
          next: () => void
        ) => {
          if (requestPath(req) !== HMR_POLL_PATH || req.method !== "GET") {
            return next();
          }

          const changed = [...pendingHmrChanges];
          pendingHmrChanges.clear();
          const descriptorJson = pendingRuntimeDescriptorJson;
          pendingRuntimeDescriptorJson = undefined;

          const payload: {
            changed: string[];
            runtimeDescriptorJson?: string | null;
          } = { changed };
          if (descriptorJson !== undefined) {
            payload.runtimeDescriptorJson = descriptorJson;
          }

          res.writeHead(200, { "Content-Type": "application/json", "Cache-Control": "no-store" });
          res.end(JSON.stringify(payload));
        }
      );

      // 3. Spawn zeroship runtime ────────────────────────────────────────────
      //
      // Deferred until Vite's HTTP server is actually listening so the child
      // gets a stable origin for module fetches and HMR polling.

      const __filename = fileURLToPath(import.meta.url);
      const __dirname = dirname(__filename);
      const bootstrapPath = resolve(__dirname, "dev-bootstrap.js");
      const binPath = resolve(root, "node_modules/.bin/zeroship");
      const cmd = process.env.ZEROSHIP_BIN
        || (existsSync(binPath) ? binPath : "zeroship");

      const serverEntry =
        projectConfig.build.serverEntry ?? findServerEntry(root) ?? undefined;

      if (!existsSync(bootstrapPath)) {
        console.warn(
          "[zeroship] dev-bootstrap.js not found — skipping runtime spawn (run the bootstrap bundler first)"
        );
      } else {
        let restartTimer: ReturnType<typeof setTimeout> | null = null;
        let healthyTimer: ReturnType<typeof setTimeout> | null = null;
        let tornDown = false;

        /**
         * Distinguish "never came up" from "ran, then died".
         *
         * The uptime of the child at the moment it exits is the whole
         * discriminator. A runtime that could not bind its port dies in
         * milliseconds, every time; a runtime that served requests for minutes
         * and then crashed is a different event and MUST still be restarted -
         * that is a working feature, not collateral.
         *
         * `healthyTimer` is what stops a single early crash from wedging the
         * dev server for the rest of the session: without it, a status set to
         * `failing` at second 1 would only ever be cleared by the NEXT exit, so
         * a child that then ran happily for an hour would still be refusing
         * requests. The timer clears the flag from the live child instead.
         */
        const markHealthy = () => {
          healthyTimer = null;
          if (runtimeStatus.health === "fatal") return;
          if (runtimeStatus.rapidFailures > 0) {
            console.log("[zeroship] runtime is up and stable again");
          }
          runtimeStatus.rapidFailures = 0;
          runtimeStatus.health = "ok";
        };

        const attachRestartHandler = (child: ChildProcess, spawnedAt: number) => {
          child.once("exit", (code, signal) => {
            if (healthyTimer) {
              clearTimeout(healthyTimer);
              healthyTimer = null;
            }
            if (tornDown || signal === "SIGTERM" || signal === "SIGKILL") return;

            const uptimeMs = Date.now() - spawnedAt;
            const reason = `code=${code}, signal=${signal}`;

            if (uptimeMs >= RUNTIME_HEALTHY_MS) {
              // Ran, then died. A genuine mid-session crash: restart, and do
              // NOT count it toward the give-up budget.
              runtimeStatus.rapidFailures = 0;
              runtimeStatus.health = "ok";
              console.warn(
                `[zeroship] runtime exited after ${Math.round(uptimeMs / 1000)}s (${reason}) - `
                  + `restarting in ${restartDelayMs(0) / 1000}s`,
              );
            } else {
              runtimeStatus.rapidFailures += 1;
              runtimeStatus.health = "failing";

              if (runtimeStatus.rapidFailures >= MAX_RAPID_RESTARTS) {
                runtimeStatus.health = "fatal";
                console.error(formatFatalBanner(runtimeStatus));
                return; // terminal: no further restart is scheduled
              }

              console.warn(
                `[zeroship] runtime exited after ${uptimeMs}ms without starting (${reason}) - `
                  + `attempt ${runtimeStatus.rapidFailures}/${MAX_RAPID_RESTARTS}, `
                  + `retrying in ${restartDelayMs(runtimeStatus.rapidFailures) / 1000}s`,
              );
            }

            if (restartTimer) clearTimeout(restartTimer);
            restartTimer = setTimeout(() => {
              restartTimer = null;
              runSpawn();
            }, restartDelayMs(runtimeStatus.rapidFailures));
          });
        };

        // STILL THE PRIMARY PATH, and not made redundant by ENV_DIE_WITH_PARENT.
        //
        // The kernel guard fires on OUR death; this fires while we are alive,
        // which is the case it exists for - `process.on("exit")` during a
        // normal vite shutdown, where the child must be gone before we return.
        // The two do not double-kill: on a clean shutdown the child is already
        // dead by the time we exit, and a signal to a dead pid goes nowhere; on
        // an unclean one this function never runs at all.
        //
        // The 3s SIGTERM->SIGKILL escalation is also why the kernel guard asks
        // for SIGKILL rather than SIGTERM: a teardown that already resorts to
        // SIGKILL has no graceful window left to protect.
        const killChild = () => {
          tornDown = true;
          if (restartTimer) {
            clearTimeout(restartTimer);
            restartTimer = null;
          }
          if (healthyTimer) {
            clearTimeout(healthyTimer);
            healthyTimer = null;
          }
          const child = serverProcess;
          serverProcess = null;
          if (child && !child.killed) {
            child.kill("SIGTERM");
            setTimeout(() => {
              if (!child.killed) {
                child.kill("SIGKILL");
              }
            }, 3000).unref();
          }
          devDb = null;
        };

        const spawnRuntime = async () => {
          if (tornDown) return;

          // Wait for the boot-time gen-types regen so the child is spawned WITH a
          // fresh runtime descriptor (the pre-in-process CLI path was synchronous).
          await bootRegenDone;
          if (tornDown) return;

          // Resolve the actual listening port from the HTTP server.
          const addr = server.httpServer?.address();
          const vitePort =
            addr && typeof addr === "object" ? addr.port
              : typeof server.config.server?.port === "number"
                ? server.config.server.port
                : 5173;

          const dotenvVars = parseDotenvVars(root);
          if (!devDb) {
            devDb = resolveDevDatabase(root);
          }
          const { databaseUrl, source } = resolveDatabaseUrl(
            process.env,
            dotenvVars,
            devDb.databaseUrl,
          );
          logDatabaseUrlSource(source, databaseUrl);

          const childEnv: NodeJS.ProcessEnv = {
            ...dotenvVars,
            ...process.env,
            DATABASE_URL: databaseUrl,
            [ENV_DEV]: "1",
            // Reaping of last resort. `killChild` below covers every teardown
            // vite gets to run; this covers the ones it does not - SIGKILL, a
            // crash, the OOM killer - after which the runtime would otherwise
            // hold this project's `.zeroship/kv.redb` until the machine is
            // rebooted, and no dev server for it could boot on ANY port.
            // See constants.ts and crates/cli/src/parent_death.rs.
            [ENV_DIE_WITH_PARENT]: String(process.pid),
            [ENV_VITE_ORIGIN]: `http://localhost:${vitePort}`,
            ...(serverEntry ? { [ENV_ENTRY]: serverEntry } : {}),
            ...(runtimeDescriptorJson !== undefined
              ? { [ENV_RUNTIME_DESCRIPTOR]: runtimeDescriptorJson }
              : {}),
            // Dev-tier auth: when enabled, hand the child the dev-user config +
            // the cookie HMAC secret. The runtime's `dev_auth.rs` reads the
            // secret to verify the `__zeroship_dev_session` cookie → server-side
            // identity; the bootstrap dev-auth provider reads both to serve
            // `/__zeroship/auth/*` + sign the cookie. Omitted entirely when disabled.
            ...(devAuthEnv.config !== null && devAuthEnv.secret !== null
              ? {
                  [ENV_DEV_AUTH]: devAuthEnv.config,
                  [ENV_DEV_AUTH_SECRET]: devAuthEnv.secret,
                }
              : {}),
          };

          try {
            // Reset the captured tail so a terminal verdict quotes THIS
            // attempt's output, not a mixture of every attempt's boot banner.
            runtimeStatus.logTail = [];
            const spawnedAt = Date.now();
            const child = spawn(
              cmd,
              ["serve", bootstrapPath, `--port=${devPort}`, "--workers=1"],
              {
                cwd: root,
                stdio: ["ignore", "pipe", "pipe"],
                env: childEnv,
              }
            );
            serverProcess = child;

            child.stdout?.on("data", (d: Buffer) => {
              const msg = d.toString().trim();
              if (!msg) return;
              pushRuntimeLog(runtimeStatus, msg);
              console.log(`[zeroship:api] ${msg}`);
            });

            child.stderr?.on("data", (d: Buffer) => {
              const msg = d.toString().trim();
              if (!msg) return;
              pushRuntimeLog(runtimeStatus, msg);
              console.log(`[zeroship:api] ${msg}`);
            });

            attachRestartHandler(child, spawnedAt);
            if (healthyTimer) clearTimeout(healthyTimer);
            healthyTimer = setTimeout(markHealthy, RUNTIME_HEALTHY_MS);
            healthyTimer.unref?.();
            console.log(`[zeroship] API server starting on :${devPort}`);
          } catch {
            console.warn(
              "[zeroship] Failed to start API server — zeroship CLI not found"
            );
          }
        };

        // Defer spawn until server is listening. spawnRuntime is async —
        // wrap with a void handler so unhandled rejections surface in logs.
        const runSpawn = () => {
          if (tornDown) return;
          spawnRuntime().catch((err) => {
            console.warn(`[zeroship] runtime spawn failed: ${(err as Error).message}`);
          });
        };
        if (server.httpServer?.listening) {
          runSpawn();
        } else {
          server.httpServer?.once("listening", runSpawn);
        }

        // Signal handlers — stored so they can be removed on server close
        // to prevent listener leaks when Vite is restarted programmatically.
        const onExit   = () => killChild();
        const onSigint = () => { killChild(); process.exit(0); };
        const onSigterm = () => { killChild(); process.exit(0); };
        process.on("exit", onExit);
        process.on("SIGINT", onSigint);
        process.on("SIGTERM", onSigterm);

        const cleanupListeners = () => {
          process.removeListener("exit", onExit);
          process.removeListener("SIGINT", onSigint);
          process.removeListener("SIGTERM", onSigterm);
        };
        let disposed = false;
        const dispose = () => {
          if (disposed) return;
          disposed = true;
          killChild();
          cleanupListeners();
          disposeRuntime = null;
        };
        disposeRuntime = dispose;
        server.httpServer?.once("close", dispose);
      }

      // 4. Proxy middleware (returned as pre-middleware) ─────────────────────
      //
      // Register directly during configureServer so this runs BEFORE
      // Vite's built-in middleware (including the SPA fallback).
      // This ensures the runtime's RPC + API paths are proxied to it rather
      // than being caught by Vite's index.html fallback. Forwarded path
      // prefixes:
      //   - /__zeroship/v1/<id>   ← spec wire (production + dev parity)
      //   - /__zeroship/auth/*    ← platform BFF login (authorize,
      //                     popup-callback, session[?mint=1], signout),
      //                     answered by the child runtime's dev-auth
      //                     provider (sdks/bootstrap/src/dev-auth.ts) — the
      //                     same same-origin contract the gateway owns in
      //                     prod. Without this the SDK's session mint hits
      //                     Vite's SPA fallback and fails.
      //   - /api/*         ← raw HTTP routes the user app exposes
      //   - /rpc, /_rpc    ← legacy wires kept for in-flight migrations
      server.middlewares.use(
        (
          req: http.IncomingMessage,
          res: http.ServerResponse,
          next: () => void
        ) => {
          const url = req.url ?? "";
          if (
            !url.startsWith("/__zeroship/v1/") &&
            !url.startsWith("/__zeroship/auth/") &&
            !url.startsWith("/_rpc") &&
            !url.startsWith("/rpc") &&
            !url.startsWith("/api/")
          ) {
            return next();
          }

          // The runtime is known not to be running. Answer HERE rather than
          // forwarding: see the `RuntimeHealth` comment - forwarding to a port
          // our child failed to bind reaches whoever DID bind it, and that
          // process answers, so the creator gets a confident wrong answer
          // (another app's data, or `Method not found` about their own
          // procedure) instead of a symptom pointing at the real cause.
          if (runtimeStatus.health !== "ok") {
            writeJson(res, 503, runtimeDownEnvelope(runtimeStatus));
            return;
          }

          // Forward path as-is — the dev-bootstrap's `default.fetch`
          // dispatches /__zeroship/v1/<id> through `default.rpc`, mirroring
          // production.
          const proxyReq = http.request(
            `http://localhost:${devPort}${url}`,
            { method: req.method, headers: req.headers },
            (proxyRes) => {
              res.writeHead(proxyRes.statusCode ?? 502, proxyRes.headers);
              proxyRes.pipe(res);
            }
          );

          req.on("error", () => proxyReq.destroy());
          req.pipe(proxyReq);

          proxyReq.on("error", () => {
            req.destroy();
            if (!res.headersSent) {
              // Same envelope contract as `runtimeDownEnvelope` - see its note
              // on why `message` is the only field that survives the trip to
              // the creator. This arm is the ordinary "still booting" case: the
              // supervisor believes the runtime is healthy, it just is not
              // accepting connections yet.
              writeJson(res, 503, {
                code: "UNAVAILABLE",
                message:
                  `zeroship dev runtime on port ${devPort} is not accepting connections yet`,
                retryable: true,
              });
            }
          });
        }
      );
    },

    async hotUpdate({ file }: { file: string }) {
      // Migration-first gen-types: a change under the migrations dir regenerates
      // the typed `env.db` surface. Awaited so the HMR poll that follows sees the
      // re-injected descriptor. `regenTypesDev` never throws, and is NOT fatal
      // here: a bad migration must not crash dev, and on a hot update even a
      // platform fault leaves a live session whose in-memory descriptor was
      // serving a moment ago.
      if (migrationsAbs != null && isUnderMigrationsDir(file, migrationsAbs)) {
        const json = await regenTypesDev(root, projectConfig.migrations, false);
        runtimeDescriptorJson = json;
        pendingRuntimeDescriptorJson = json ?? null;
        // Don't return — a migration `.ts` is still a `.ts`; fall through to the
        // HMR-queue path below so the runtime re-fetches if it imported one.
      }

      if (
        file.endsWith(".ts") || file.endsWith(".tsx") ||
        file.endsWith(".js") || file.endsWith(".jsx")
      ) {
        // Server-module discovery is now path-based (no caches to
        // invalidate). Queue the change for HMR delivery to the V8
        // runtime — the runtime polls /__zeroship_hmr_check and
        // invalidates its ModuleRunner cache for each path returned.
        // The next import() re-fetches from Vite.
        pendingHmrChanges.add(file);
      }
    },

    buildEnd() {
      disposeRuntime?.();
    },
  };

  return [environmentPlugin, devServerPluginImpl];
}
