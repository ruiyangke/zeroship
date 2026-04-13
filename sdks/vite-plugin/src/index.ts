/**
 * @zeroship/vite-plugin — full-stack Vite integration for zeroship.
 *
 * Usage:
 *   import { zeroship } from '@zeroship/vite-plugin'
 *   export default defineConfig({ plugins: [react(), zeroship()] })
 *
 * What it does:
 *   - Detects "use server" modules and functions via @swc/core AST analysis
 *   - Replaces server function exports with RPC stubs in client code
 *   - Builds server code as .appbundle via zeroship CLI
 *   - In dev: runs a zeroship runtime alongside Vite, proxies /_rpc
 *   - In build: outputs dist/public/ (client) + dist/server/ (.appbundle)
 */

import type { Plugin, ResolvedConfig, ViteDevServer } from "vite";
import { readFileSync, existsSync } from "node:fs";
import { resolve, relative, extname } from "node:path";
import { analyzeModule, isServerModuleQuick } from "./analyze.js";
import { transformToClient } from "./stub.js";
import { buildServerBundle } from "./server-bundle.js";
import { startDevServer, stopDevServer, restartDevServer } from "./dev-server.js";

export interface ZeroshipOptions {
  /** RPC endpoint path (default: "/_rpc") */
  rpcEndpoint?: string;
  /** Server entry point (auto-detected if not specified) */
  serverEntry?: string;
  /** Port for the zeroship dev server (default: 3001) */
  devServerPort?: number;
}

/**
 * Vite plugin for zeroship full-stack apps.
 *
 * Handles "use server" convention: server functions are automatically
 * replaced with RPC stubs in the client build, and bundled separately
 * for the zeroship runtime.
 */
export function zeroship(options: ZeroshipOptions = {}): Plugin[] {
  const rpcEndpoint = options.rpcEndpoint ?? "/_rpc";
  const devPort = options.devServerPort ?? 3001;

  let config: ResolvedConfig;
  let isDev = false;
  let root = "";

  // Track which modules are server modules (have "use server" at file level)
  const serverModuleCache = new Map<string, boolean>();
  // Track server functions per module for the build report
  const serverFunctionMap = new Map<string, string[]>();

  /** Check if a resolved module is a server module (cached) */
  function isServerModule(id: string): boolean {
    if (serverModuleCache.has(id)) return serverModuleCache.get(id)!;

    try {
      const code = readFileSync(id, "utf-8");
      const result = isServerModuleQuick(code);
      serverModuleCache.set(id, result);
      return result;
    } catch {
      return false;
    }
  }

  /** Resolve a module specifier to an absolute path (for node_modules) */
  function resolveServerModule(source: string): boolean {
    // Check node_modules
    const pkgDir = resolve(root, "node_modules", source);
    if (!existsSync(pkgDir)) return false;

    const pkgJson = resolve(pkgDir, "package.json");
    if (!existsSync(pkgJson)) return false;

    try {
      const pkg = JSON.parse(readFileSync(pkgJson, "utf-8"));
      // Find entry file
      const entry =
        pkg.exports?.["."]?.import ??
        pkg.exports?.["."]?.default ??
        pkg.exports?.["."] ??
        pkg.module ??
        pkg.main;

      if (!entry) return false;
      const entryPath = resolve(pkgDir, entry);
      return isServerModule(entryPath);
    } catch {
      return false;
    }
  }

  // Collect server modules from imports
  const knownServerSources = new Set<string>();

  return [
    // Plugin 1: Transform — detect "use server" and generate stubs
    {
      name: "zeroship:transform",
      enforce: "pre",

      configResolved(resolvedConfig) {
        config = resolvedConfig;
        isDev = config.command === "serve";
        root = config.root;
      },

      transform(code, id) {
        // Only process TS/TSX/JS/JSX files in the project
        const ext = extname(id);
        if (![".ts", ".tsx", ".js", ".jsx"].includes(ext)) return null;
        if (id.includes("node_modules")) return null;

        // Scan imports for server modules (resolve once)
        if (knownServerSources.size === 0) {
          // Bootstrap: check @zeroship/* packages
          const nmDir = resolve(root, "node_modules", "@zeroship");
          if (existsSync(nmDir)) {
            try {
              const packages = require("node:fs").readdirSync(nmDir);
              for (const pkg of packages) {
                const source = `@zeroship/${pkg}`;
                if (resolveServerModule(source)) {
                  knownServerSources.add(source);
                }
              }
            } catch {
              // ignore
            }
          }
        }

        // Analyze the module
        const relPath = relative(root, id);
        const analysis = analyzeModule(code, id, knownServerSources);

        if (analysis.serverFunctions.length === 0) {
          return null; // No server code — pass through unchanged
        }

        // Track for build report
        serverFunctionMap.set(relPath, analysis.serverFunctions);

        // Replace server functions with RPC stubs
        const clientCode = transformToClient(code, analysis.serverFunctions, rpcEndpoint);

        return {
          code: clientCode,
          map: null, // TODO: source maps
        };
      },
    },

    // Plugin 2: Dev server — run zeroship runtime for API
    {
      name: "zeroship:dev-server",

      configureServer(server: ViteDevServer) {
        if (!isDev) return;

        // Start zeroship dev server
        startDevServer({
          root,
          port: devPort,
          serverEntry: options.serverEntry,
        });

        // Proxy /_rpc to the zeroship dev server
        server.middlewares.use((req, res, next) => {
          if (req.url?.startsWith(rpcEndpoint)) {
            // Rewrite to /rpc for the zeroship runtime
            const targetUrl = `http://localhost:${devPort}/rpc`;
            const proxyReq = require("node:http").request(
              targetUrl,
              { method: req.method, headers: req.headers },
              (proxyRes: any) => {
                res.writeHead(proxyRes.statusCode, proxyRes.headers);
                proxyRes.pipe(res);
              }
            );
            req.pipe(proxyReq);
            proxyReq.on("error", () => {
              res.writeHead(503);
              res.end(JSON.stringify({ error: "Server not ready" }));
            });
          } else {
            next();
          }
        });

        // Restart server on source file changes
        server.watcher.on("change", (file) => {
          if (file.endsWith(".ts") || file.endsWith(".tsx")) {
            // Check if it's a server-relevant file
            const relPath = relative(root, file);
            if (
              serverFunctionMap.has(relPath) ||
              relPath.startsWith("src/index") ||
              relPath.startsWith("src/server")
            ) {
              console.log(`[zeroship] Server file changed: ${relPath} — restarting`);
              restartDevServer({ root, port: devPort, serverEntry: options.serverEntry });
            }
          }
        });
      },

      // Cleanup on server close
      buildEnd() {
        if (isDev) {
          stopDevServer();
        }
      },
    },

    // Plugin 3: Build — bundle server code after client build
    {
      name: "zeroship:build",

      closeBundle() {
        if (isDev) return;

        const outDir = config.build?.outDir
          ? resolve(root, config.build.outDir)
          : resolve(root, "dist");

        console.log("\n[zeroship] Building server bundle...");

        const result = buildServerBundle({
          root,
          outDir,
          serverEntry: options.serverEntry,
          minify: config.build?.minify !== false,
        });

        if (result) {
          console.log(`[zeroship] Server bundle: ${result.bundlePath}`);
          if (result.serverFunctions.length > 0) {
            console.log(`[zeroship] Server functions: ${result.serverFunctions.join(", ")}`);
          }
        } else {
          console.log("[zeroship] No server code detected — client-only build");
        }

        // Report
        if (serverFunctionMap.size > 0) {
          console.log("\n[zeroship] Server/client split:");
          for (const [file, fns] of serverFunctionMap) {
            console.log(`  ${file}: ${fns.join(", ")} → RPC stubs`);
          }
        }
      },
    },
  ];
}

export default zeroship;
