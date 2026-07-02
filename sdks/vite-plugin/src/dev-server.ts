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
  DEFAULT_DEV_PORT,
} from "./constants.js";
import { resolveDevAuthEnv, type DevAuthOption } from "./dev-auth-config.js";
import {
  ZeroshipDevEnvironment,
  createZeroshipEnvironmentOptions,
} from "./environment.js";
import { findServerEntry } from "./build.js";
import type { TransformState } from "./transform.js";
import { resolveDevDatabase, type DevDatabase } from "./dev-db.js";
import {
  GEN_TYPES_OUT_DEFAULT,
  RUNTIME_DESCRIPTOR_FILE,
  genTypesViaCli,
} from "./migrations.js";

// ── Types ──────────────────────────────────────────────────────────────────

export interface DevServerOptions {
  devServerPort?: number;
  serverEntry?: string;
  /**
   * Dev-tier auth config. `undefined` defaults to ON with the built-in dev
   * user; `false` disables. See `ZeroshipOptions.devAuth`.
   */
  devAuth?: DevAuthOption;
  /** Migration-first gen-types (P3). See `ZeroshipOptions.migrations`. */
  migrations?: {
    dir?: string;
    genTypesOut?: string;
    cliPath?: string;
  };
}

type FetchMethod = "fetchModule" | "getBuiltins";
type DatabaseUrlSource = "shell" | "dotenv" | "default";
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
 * Migration-first gen-types (P3) — REGENERATE the typed `env.db` surface from
 * the migration set in DEV. Fire-and-forget: any failure is LOGGED, never
 * thrown (a bad migration must not crash the dev server). The graceful
 * binary-absence path (warn-once + no-op) lives in `genTypesViaCli`.
 *
 * Dev always WRITES (no `--check`; that is a CI/build generated-artifact concern).
 */
function readGeneratedRuntimeDescriptor(
  root: string,
  migrations: DevServerOptions["migrations"],
): string | undefined {
  const descriptorPath = resolve(
    root,
    migrations?.genTypesOut ?? GEN_TYPES_OUT_DEFAULT,
    RUNTIME_DESCRIPTOR_FILE,
  );
  try {
    const json = readFileSync(descriptorPath, "utf8").trim();
    return json.length > 0 ? json : undefined;
  } catch {
    return undefined;
  }
}

function regenTypesDev(
  root: string,
  migrations: DevServerOptions["migrations"],
  warnedNoBinaryRef: { value: boolean }
): string | undefined {
  try {
    const result = genTypesViaCli({
      root,
      migrationsDir: migrations?.dir,
      genTypesOut: migrations?.genTypesOut,
      cliPath: migrations?.cliPath,
      check: false,
      requireBinary: false,
    });
    if (result.status === "skipped") {
      // Warn only ONCE per dev-server lifetime — not on every keystroke.
      if (!warnedNoBinaryRef.value) {
        warnedNoBinaryRef.value = true;
        console.warn(`[zeroship] gen-types skipped in dev — ${result.reason}`);
      }
    } else {
      console.log(
        "[zeroship] gen-types: regenerated env.db.ts + schema.runtime.json from the migrations"
      );
    }
  } catch (e) {
    // Dev: never throw — a malformed migration must not take down the server.
    console.error(`[zeroship] gen-types failed (dev): ${(e as Error).message}`);
  }
  return readGeneratedRuntimeDescriptor(root, migrations);
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

function parseDotenvVars(root: string): Record<string, string> {
  const envPath = resolve(root, ".env");
  if (!existsSync(envPath)) return {};

  const dotenvVars: Record<string, string> = {};
  for (const line of readFileSync(envPath, "utf-8").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const eq = trimmed.indexOf("=");
    if (eq === -1) continue;
    const key = trimmed.slice(0, eq).trim();
    let val = trimmed.slice(eq + 1).trim();
    if ((val.startsWith("\"") && val.endsWith("\"")) || (val.startsWith("'") && val.endsWith("'"))) {
      val = val.slice(1, -1);
    }
    dotenvVars[key] = val;
  }
  return dotenvVars;
}

function resolveDatabaseUrl(
  parentEnv: NodeJS.ProcessEnv,
  dotenvVars: Record<string, string>,
  defaultDatabaseUrl: string,
): { databaseUrl: string; source: DatabaseUrlSource } {
  if (parentEnv.DATABASE_URL) {
    return { databaseUrl: parentEnv.DATABASE_URL, source: "shell" };
  }
  if (dotenvVars.DATABASE_URL) {
    return { databaseUrl: dotenvVars.DATABASE_URL, source: "dotenv" };
  }
  return { databaseUrl: defaultDatabaseUrl, source: "default" };
}

function logDatabaseUrlSource(source: DatabaseUrlSource, databaseUrl: string): void {
  if (source === "shell") {
    console.log("[zeroship] using DATABASE_URL from shell environment");
    return;
  }
  if (source === "dotenv") {
    console.log("[zeroship] using DATABASE_URL from .env");
    return;
  }
  console.log(`[zeroship] using default DATABASE_URL ${databaseUrl}`);
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
  state: TransformState
): Plugin[] {
  const devPort = options.devServerPort ?? DEFAULT_DEV_PORT;

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

  // Migration-first gen-types (P3). The absolute migrations dir is resolved in
  // configureServer (once `root` is known) so the `hotUpdate` branch can match
  // changed files against it. `warnedNoBinary` keeps the binary-absence warning
  // to ONCE per dev-server lifetime.
  let migrationsAbs: string | null = null;
  const warnedNoBinary = { value: false };
  let runtimeDescriptorJson: string | undefined;
  let pendingRuntimeDescriptorJson: string | null | undefined;

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
      const entry =
        options.serverEntry ?? findServerEntry(detectedRoot) ?? undefined;
      return {
        environments: {
          zeroship: createZeroshipEnvironmentOptions(entry),
        },
      };
    },

    configResolved(config) {
      root = config.root;
      isDev = config.command === "serve";
    },
  };

  // ── Plugin 2: zeroship:dev-server ────────────────────────────────────────

  const devServerPluginImpl: Plugin = {
    name: "zeroship:dev-server",

    configureServer(server: ViteDevServer) {
      if (!isDev) return;

      // 0. Migration-first gen-types (P3) — ensure the migrations dir is
      //    WATCHED so a change there fires `hotUpdate` (Vite only watches the
      //    module graph + root by default; a migrations dir holding `.ts`
      //    sources not imported by app code may not be covered). The
      //    `hotUpdate` branch below regenerates `env.db.ts` on a change.
      migrationsAbs = resolve(root, options.migrations?.dir ?? "migrations");
      if (existsSync(migrationsAbs)) {
        server.watcher.add(migrationsAbs);
        // Initial regen on boot: migrations may have changed while the dev
        // server was down (`hotUpdate` only fires on a *subsequent* change, so
        // without this a fresh `pnpm dev` leaves env.db.ts stale). Fire-and-forget
        // — `regenTypesDev` logs on error and NEVER throws.
        runtimeDescriptorJson = regenTypesDev(root, options.migrations, warnedNoBinary);
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
        options.serverEntry ?? findServerEntry(root) ?? undefined;

      if (!existsSync(bootstrapPath)) {
        console.warn(
          "[zeroship] dev-bootstrap.js not found — skipping runtime spawn (run the bootstrap bundler first)"
        );
      } else {
        let restartTimer: ReturnType<typeof setTimeout> | null = null;
        let tornDown = false;

        const attachRestartHandler = (child: ChildProcess) => {
          child.once("exit", (code, signal) => {
            if (tornDown || signal === "SIGTERM" || signal === "SIGKILL") return;
            console.warn(
              `[zeroship] runtime exited unexpectedly (code=${code}, signal=${signal}) — restarting in 1s`
            );
            if (restartTimer) clearTimeout(restartTimer);
            restartTimer = setTimeout(() => {
              restartTimer = null;
              runSpawn();
            }, 1000);
          });
        };

        const killChild = () => {
          tornDown = true;
          if (restartTimer) {
            clearTimeout(restartTimer);
            restartTimer = null;
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
              if (msg) console.log(`[zeroship:api] ${msg}`);
            });

            child.stderr?.on("data", (d: Buffer) => {
              const msg = d.toString().trim();
              if (msg) console.log(`[zeroship:api] ${msg}`);
            });

            attachRestartHandler(child);
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
              res.writeHead(503, { "Content-Type": "application/json" });
              res.end('{"error":"zeroship API not ready"}');
            }
          });
        }
      );
    },

    hotUpdate({ file }: { file: string }) {
      // Migration-first gen-types (P3): a change under the migrations dir
      // regenerates the typed `env.db` surface. Fire-and-forget — the helper
      // logs on error and NEVER throws (a bad migration must not crash dev).
      if (migrationsAbs != null && isUnderMigrationsDir(file, migrationsAbs)) {
        runtimeDescriptorJson = regenTypesDev(root, options.migrations, warnedNoBinary);
        pendingRuntimeDescriptorJson = runtimeDescriptorJson ?? null;
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
