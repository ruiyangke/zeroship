// sdks/vite-plugin/src/dev-server.ts
//
// configureServer hook that replaces the old child-process+proxy approach
// with the Vite Environment API.

import type { Plugin, ViteDevServer } from "vite";
import { resolve, dirname } from "node:path";
import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { ChildProcess, spawn } from "node:child_process";
import http from "node:http";
import { WebSocketServer } from "ws";
import type { WebSocket } from "ws";
import {
  WS_PATH,
  ENV_DEV,
  ENV_VITE_WS,
  ENV_ENTRY,
  DEFAULT_DEV_PORT,
} from "./constants.js";
import {
  ZeroshipDevEnvironment,
  createZeroshipEnvironmentOptions,
} from "./environment.js";
import { findServerEntry } from "./build.js";
import type { TransformState } from "./transform.js";
import { resolveDevDatabase, type DevDatabase } from "./dev-db.js";

// ── Types ──────────────────────────────────────────────────────────────────

export interface DevServerOptions {
  devServerPort?: number;
  serverEntry?: string;
}

// ── Plugin factory ─────────────────────────────────────────────────────────

export function devServerPlugin(
  options: DevServerOptions,
  state: TransformState
): Plugin[] {
  const devPort = options.devServerPort ?? DEFAULT_DEV_PORT;

  let root = "";
  let isDev = false;
  let serverProcess: ChildProcess | null = null;
  let devDb: DevDatabase | null = null;

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

      // 1. WebSocket bridge ─────────────────────────────────────────────────
      //
      // Create a noServer WSS so we can intercept upgrade events manually.
      // Vite also uses WebSocket for its own HMR — we must only intercept
      // requests targeting WS_PATH and leave the rest to Vite.

      const wss = new WebSocketServer({ noServer: true });

      server.httpServer?.on(
        "upgrade",
        (req: http.IncomingMessage, socket: import("stream").Duplex, head: Buffer) => {
          const url = new URL(req.url ?? "", "http://localhost");
          if (url.pathname !== WS_PATH) return; // let Vite handle its own upgrades

          wss.handleUpgrade(req, socket, head, (ws: WebSocket) => {
            wss.emit("connection", ws, req);

            const zeroshipEnv = server.environments[
              "zeroship"
            ] as ZeroshipDevEnvironment | undefined;

            if (zeroshipEnv) {
              zeroshipEnv.setWebSocket(ws);
            } else {
              console.warn(
                "[zeroship] WS connection received but zeroship environment not found"
              );
            }
          });
        }
      );

      // 2. Module fetch endpoint ─────────────────────────────────────────
      //
      // The runtime's ModuleRunner calls this to fetch transformed modules
      // from Vite's environment. This replaces WebSocket-based invoke since
      // the V8 runtime doesn't support outbound WebSocket connections.

      server.middlewares.use(
        async (
          req: http.IncomingMessage,
          res: http.ServerResponse,
          next: () => void
        ) => {
          if (req.url !== "/__zeroship_fetch" || req.method !== "POST") {
            return next();
          }

          const zeroshipEnv = server.environments[
            "zeroship"
          ] as ZeroshipDevEnvironment | undefined;

          if (!zeroshipEnv) {
            res.writeHead(500, { "Content-Type": "application/json" });
            res.end(JSON.stringify({ error: { message: "zeroship environment not found" } }));
            return;
          }

          // Read POST body
          const chunks: Buffer[] = [];
          for await (const chunk of req) chunks.push(chunk as Buffer);
          const body = Buffer.concat(chunks).toString();

          try {
            const data = JSON.parse(body);
            // The ModuleRunner sends vite:invoke calls via the transport.
            // Format: { type: "custom", event: "vite:invoke",
            //           data: { id: correlationId, name: methodName, data: args[] } }
            const invoke = data.data ?? data;
            const { name: methodName, data: args } = invoke;

            let result: any;
            if (methodName === "fetchModule") {
              // args = [id, importer, options?]
              result = await zeroshipEnv.fetchModule(args[0], args[1], args[2]);
            } else if (methodName === "getBuiltins") {
              // Return EMPTY builtins — our V8 runtime can't import node: modules
              // natively. By returning [], the ModuleRunner will always call
              // fetchModule() for every import, which lets our fetchModule override
              // intercept node:* and return polyfill code.
              result = [];
            } else {
              // Dispatch other methods to the environment if they exist
              const fn = (zeroshipEnv as any)[methodName];
              if (typeof fn === "function") {
                result = await fn.apply(zeroshipEnv, args ?? []);
              } else {
                // Unknown methods return empty result rather than error —
                // the runner may probe for optional capabilities.
                result = null;
              }
            }

            // Return in the format the runner expects: { result } or { error }
            res.writeHead(200, { "Content-Type": "application/json" });
            res.end(JSON.stringify({ result }));
          } catch (e: any) {
            res.writeHead(200, { "Content-Type": "application/json" });
            res.end(JSON.stringify({ error: { message: e.message ?? String(e) } }));
          }
        }
      );

      // 3. HMR poll endpoint ──────────────────────────────────────────────
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
          if (req.url !== "/__zeroship_hmr_check" || req.method !== "GET") {
            return next();
          }

          const changed = [...pendingHmrChanges];
          pendingHmrChanges.clear();

          res.writeHead(200, {
            "Content-Type": "application/json",
            "Cache-Control": "no-store",
          });
          res.end(JSON.stringify({ changed }));
        }
      );

      // 4. Spawn zeroship runtime ────────────────────────────────────────────
      //
      // Deferred until Vite's HTTP server is actually listening. The bootstrap
      // opens a WebSocket back to Vite, which fails if the server isn't ready.

      const __filename = fileURLToPath(import.meta.url);
      const __dirname = dirname(__filename);
      const bootstrapPath = resolve(__dirname, "dev-bootstrap.js");
      const binPath = resolve(root, "node_modules/.bin/zeroship");
      const cmd = existsSync(binPath) ? binPath : "zeroship";

      const serverEntry =
        options.serverEntry ?? findServerEntry(root) ?? undefined;

      if (!existsSync(bootstrapPath)) {
        console.warn(
          "[zeroship] dev-bootstrap.js not found — skipping runtime spawn (run the bootstrap bundler first)"
        );
      } else {
        const spawnRuntime = async () => {
          // Resolve the actual listening port from the HTTP server.
          const addr = server.httpServer?.address();
          const vitePort =
            addr && typeof addr === "object" ? addr.port
              : typeof server.config.server?.port === "number"
                ? server.config.server.port
                : 5173;

          // Load .env file if present (like dotenv)
          const dotenvVars: Record<string, string> = {};
          const envPath = resolve(root, ".env");
          if (existsSync(envPath)) {
            for (const line of readFileSync(envPath, "utf-8").split("\n")) {
              const trimmed = line.trim();
              if (!trimmed || trimmed.startsWith("#")) continue;
              const eq = trimmed.indexOf("=");
              if (eq === -1) continue;
              const key = trimmed.slice(0, eq).trim();
              let val = trimmed.slice(eq + 1).trim();
              // Strip surrounding quotes
              if ((val.startsWith('"') && val.endsWith('"')) || (val.startsWith("'") && val.endsWith("'"))) {
                val = val.slice(1, -1);
              }
              dotenvVars[key] = val;
            }
          }

          // Default to a project-local SQLite file when the creator
          // hasn't provided DATABASE_URL via .env or the parent
          // environment. The runtime dispatches by scheme, so the same
          // `DATABASE_URL` escape hatch still works for real Postgres.
          const hasUserDbUrl = !!(dotenvVars.DATABASE_URL || process.env.DATABASE_URL);
          if (!hasUserDbUrl && !devDb) {
            try {
              devDb = resolveDevDatabase(root);
              console.log(
                `[zeroship] dev db ready (sqlite) — ${devDb.databaseUrl}`
              );
            } catch (err) {
              console.warn(
                `[zeroship] failed to prepare sqlite dev db: ${(err as Error).message} — zeroship.db.* will be unavailable`
              );
            }
          }

          // Stage 5c: schema discovery is unified — dev-bootstrap
          // reads `mod.default.schema` lazily on first request (via
          // `maybeRegisterSchema`). No more split-file env-var path;
          // the entry-default convention is the only path.

          const childEnv: NodeJS.ProcessEnv = {
            ...process.env,
            ...dotenvVars,
            ...(devDb && !hasUserDbUrl ? { DATABASE_URL: devDb.databaseUrl } : {}),
            [ENV_DEV]: "1",
            [ENV_VITE_WS]: `ws://localhost:${vitePort}${WS_PATH}`,
            ...(serverEntry ? { [ENV_ENTRY]: serverEntry } : {}),
          };

          try {
            serverProcess = spawn(
              cmd,
              ["serve", bootstrapPath, `--port=${devPort}`, "--workers=1"],
              {
                cwd: root,
                stdio: ["ignore", "pipe", "pipe"],
                env: childEnv,
              }
            );

            serverProcess.stdout?.on("data", (d: Buffer) => {
              const msg = d.toString().trim();
              if (msg) console.log(`[zeroship:api] ${msg}`);
            });

            serverProcess.stderr?.on("data", (d: Buffer) => {
              const msg = d.toString().trim();
              if (msg) console.log(`[zeroship:api] ${msg}`);
            });

            console.log(`[zeroship] API server starting on :${devPort}`);
          } catch {
            console.warn(
              "[zeroship] Failed to start API server — zeroship CLI not found"
            );
          }
        };

        // Defer spawn until server is listening. spawnRuntime is async —
        // wrap with a void handler so unhandled rejections surface in logs.
        const runSpawn = () => { spawnRuntime().catch((err) => {
          console.warn(`[zeroship] runtime spawn failed: ${(err as Error).message}`);
        }); };
        if (server.httpServer?.listening) {
          runSpawn();
        } else {
          server.httpServer?.once("listening", runSpawn);
        }

        // Clean up child process + dev db on Vite exit (SIGINT, SIGTERM, process.exit)
        const killChild = () => {
          if (serverProcess && !serverProcess.killed) {
            serverProcess.kill("SIGTERM");
            setTimeout(() => {
              if (serverProcess && !serverProcess.killed) {
                serverProcess.kill("SIGKILL");
              }
            }, 3000).unref();
          }
          devDb = null;
        };

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
        server.httpServer?.on("close", () => { killChild(); cleanupListeners(); });

        // Restart on unexpected exit (crash recovery).
        // A single timer reference prevents concurrent spawn attempts
        // when the child crash-loops faster than the restart delay.
        let restartTimer: ReturnType<typeof setTimeout> | null = null;

        const setupRestartHandler = () => {
          if (!serverProcess) return;
          serverProcess.on("exit", (code, signal) => {
            if (signal === "SIGTERM" || signal === "SIGKILL") return;
            console.warn(
              `[zeroship] runtime exited unexpectedly (code=${code}, signal=${signal}) — restarting in 1s`
            );
            if (restartTimer) clearTimeout(restartTimer);
            restartTimer = setTimeout(() => {
              restartTimer = null;
              runSpawn();
              setupRestartHandler();
            }, 1000);
          });
        };

        setupRestartHandler();
      }

      // 4. Proxy middleware (returned as pre-middleware) ─────────────────────
      //
      // Returning a function from configureServer registers it as pre-middleware,
      // so it runs BEFORE Vite's built-in middleware (including the SPA fallback).
      // This ensures the runtime's RPC + API paths are proxied to it rather
      // than being caught by Vite's index.html fallback. Forwarded path
      // prefixes:
      //   - /_zs/v1/<id>   ← spec wire (production + dev parity)
      //   - /api/*         ← raw HTTP routes the user app exposes
      //   - /rpc, /_rpc    ← legacy wires kept for in-flight migrations
      return () => {
        server.middlewares.use(
          (
            req: http.IncomingMessage,
            res: http.ServerResponse,
            next: () => void
          ) => {
            const url = req.url ?? "";
            if (
              !url.startsWith("/_zs/v1/") &&
              !url.startsWith("/_rpc") &&
              !url.startsWith("/rpc") &&
              !url.startsWith("/api/")
            ) {
              return next();
            }

            // Forward path as-is — the dev-bootstrap's `default.fetch`
            // dispatches /_zs/v1/<id> through `default.rpc`, mirroring
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
      };
    },

    hotUpdate({ file }: { file: string }) {
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
      if (serverProcess) {
        serverProcess.kill("SIGTERM");
        serverProcess = null;
      }
    },
  };

  return [environmentPlugin, devServerPluginImpl];
}
