// sdks/vite-plugin/src/dev-server.ts
//
// configureServer hook that replaces the old child-process+proxy approach
// with the Vite Environment API.

import type { Plugin, ViteDevServer } from "vite";
import { resolve, dirname } from "node:path";
import { existsSync } from "node:fs";
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

  // ── Plugin 1: zeroship:environment ──────────────────────────────────────

  const environmentPlugin: Plugin = {
    name: "zeroship:environment",

    config() {
      return {
        environments: {
          zeroship: createZeroshipEnvironmentOptions(),
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
              // Return serialized resolve.builtins from the environment config.
              // Vite 8's ModuleRunner calls this to know which imports to externalize.
              const builtins = (zeroshipEnv as any).config?.resolve?.builtins ?? [];
              result = builtins.map((b: any) =>
                typeof b === "string"
                  ? { type: "string", value: b }
                  : { type: "RegExp", source: b.source, flags: b.flags }
              );
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

      // 3. Proxy middleware (pre-middleware) ───────────────────────────────
      //
      // Registered directly inside configureServer (not returned) so it runs
      // BEFORE Vite's built-in middleware. This ensures /api/* and /_rpc are
      // proxied to the runtime instead of being caught by Vite's SPA fallback.

      server.middlewares.use(
        (
          req: http.IncomingMessage,
          res: http.ServerResponse,
          next: () => void
        ) => {
          const url = req.url ?? "";
          if (!url.startsWith("/_rpc") && !url.startsWith("/api/")) {
            return next();
          }

          const proxyReq = http.request(
            `http://localhost:${devPort}${url}`,
            { method: req.method, headers: req.headers },
            (proxyRes) => {
              res.writeHead(proxyRes.statusCode ?? 502, proxyRes.headers);
              proxyRes.pipe(res);
            }
          );

          req.pipe(proxyReq);

          proxyReq.on("error", () => {
            res.writeHead(503, { "Content-Type": "application/json" });
            res.end('{"error":"zeroship API not ready"}');
          });
        }
      );

      // 3. Spawn zeroship runtime ────────────────────────────────────────────
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
        const spawnRuntime = () => {
          // Resolve the actual listening port from the HTTP server.
          const addr = server.httpServer?.address();
          const vitePort =
            addr && typeof addr === "object" ? addr.port
              : typeof server.config.server?.port === "number"
                ? server.config.server.port
                : 5173;

          const childEnv: NodeJS.ProcessEnv = {
            ...process.env,
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

        // Defer spawn until server is listening.
        if (server.httpServer?.listening) {
          spawnRuntime();
        } else {
          server.httpServer?.once("listening", spawnRuntime);
        }
      }
    },

    handleHotUpdate({ file }: { file: string }) {
      // Clear cached server-module status for the changed file so the next
      // transform re-evaluates whether it is a server file.
      state.serverModuleCache.delete(file);
      // ModuleRunner handles module graph invalidation via HMR messages.
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
