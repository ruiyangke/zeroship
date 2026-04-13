/**
 * @zeroship/vite-plugin — full-stack Vite 8 integration for zeroship.
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
 *   - In build: outputs dist/ (client) + dist/server/ (.appbundle)
 *
 * Vite 8 compatibility:
 *   - Uses hook filter feature for performance (only transforms TS/TSX/JS/JSX)
 *   - Compatible with Rolldown bundler
 *   - Environment-aware (client environment only — server is external)
 */

import type { Plugin, ResolvedConfig, ViteDevServer } from "vite";
import { readFileSync, existsSync, readdirSync } from "node:fs";
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

  // Cache: module path → is "use server" module
  const serverModuleCache = new Map<string, boolean>();
  // Cache: resolved package specifier → is server package
  const serverPackageCache = new Map<string, boolean>();
  // Track server functions per file for build report
  const serverFunctionMap = new Map<string, string[]>();
  // Known server module specifiers (resolved once at startup)
  const knownServerSources = new Set<string>();

  /** Check if a file has "use server" at top (cached) */
  function checkServerModule(filePath: string): boolean {
    if (serverModuleCache.has(filePath)) return serverModuleCache.get(filePath)!;
    try {
      const code = readFileSync(filePath, "utf-8");
      const result = isServerModuleQuick(code);
      serverModuleCache.set(filePath, result);
      return result;
    } catch {
      serverModuleCache.set(filePath, false);
      return false;
    }
  }

  /** Check if an npm package is a server module (cached) */
  function checkServerPackage(specifier: string): boolean {
    if (serverPackageCache.has(specifier)) return serverPackageCache.get(specifier)!;

    const pkgDir = resolve(root, "node_modules", specifier);
    if (!existsSync(pkgDir)) {
      serverPackageCache.set(specifier, false);
      return false;
    }

    const pkgJsonPath = resolve(pkgDir, "package.json");
    if (!existsSync(pkgJsonPath)) {
      serverPackageCache.set(specifier, false);
      return false;
    }

    try {
      const pkg = JSON.parse(readFileSync(pkgJsonPath, "utf-8"));
      const entry =
        pkg.exports?.["."]?.import ??
        pkg.exports?.["."]?.default ??
        (typeof pkg.exports?.["."] === "string" ? pkg.exports["."] : null) ??
        pkg.module ??
        pkg.main;

      if (!entry) {
        serverPackageCache.set(specifier, false);
        return false;
      }

      const entryPath = resolve(pkgDir, entry);
      const result = checkServerModule(entryPath);
      serverPackageCache.set(specifier, result);
      return result;
    } catch {
      serverPackageCache.set(specifier, false);
      return false;
    }
  }

  /** Scan node_modules/@zeroship/* to find server packages */
  function discoverServerPackages(): void {
    const scopeDir = resolve(root, "node_modules", "@zeroship");
    if (!existsSync(scopeDir)) return;

    try {
      for (const pkg of readdirSync(scopeDir)) {
        const specifier = `@zeroship/${pkg}`;
        if (checkServerPackage(specifier)) {
          knownServerSources.add(specifier);
        }
      }
    } catch {
      // ignore
    }
  }

  return [
    // Plugin 1: Transform — detect "use server" and generate RPC stubs
    {
      name: "zeroship:transform",
      enforce: "pre" as const,

      configResolved(resolvedConfig: ResolvedConfig) {
        config = resolvedConfig;
        isDev = config.command === "serve";
        root = config.root;

        // Discover server packages on startup
        discoverServerPackages();
      },

      // Vite 8 hook filter: only process source files, skip node_modules
      transform: {
        filter: {
          id: {
            include: /\.(ts|tsx|js|jsx)$/,
            exclude: /node_modules/,
          },
        },
        handler(code: string, id: string) {
          // Analyze module for "use server"
          const analysis = analyzeModule(code, id, knownServerSources);

          if (analysis.serverFunctions.length === 0) {
            return null; // No server code — pass through
          }

          // Track for build report
          const relPath = relative(root, id);
          serverFunctionMap.set(relPath, analysis.serverFunctions);

          // Replace server functions with RPC stubs
          const clientCode = transformToClient(
            code,
            analysis.serverFunctions,
            rpcEndpoint
          );

          return { code: clientCode, map: null };
        },
      },
    },

    // Plugin 2: Dev server — zeroship runtime + RPC proxy
    {
      name: "zeroship:dev-server",

      configureServer(server: ViteDevServer) {
        if (!isDev) return;

        // Start zeroship runtime for server functions
        startDevServer({
          root,
          port: devPort,
          serverEntry: options.serverEntry,
        });

        // Proxy /_rpc → zeroship runtime /rpc
        server.middlewares.use((req, res, next) => {
          if (!req.url?.startsWith(rpcEndpoint)) {
            return next();
          }

          const http = require("node:http");
          const proxyReq = http.request(
            `http://localhost:${devPort}/rpc`,
            { method: req.method, headers: req.headers },
            (proxyRes: any) => {
              res.writeHead(proxyRes.statusCode, proxyRes.headers);
              proxyRes.pipe(res);
            }
          );
          req.pipe(proxyReq);
          proxyReq.on("error", () => {
            res.writeHead(503, { "Content-Type": "application/json" });
            res.end(JSON.stringify({ error: "zeroship server not ready" }));
          });
        });

      },

      /** Use Vite's HMR hook for server code hot-restart */
      handleHotUpdate({ file, server: _server }: { file: string; server: ViteDevServer }) {
        const ext = extname(file);
        if (![".ts", ".tsx", ".js", ".jsx"].includes(ext)) return;

        const rel = relative(root, file);
        const isServerFile =
          serverFunctionMap.has(rel) ||
          rel.startsWith("src/index") ||
          rel.startsWith("src/server");

        if (isServerFile) {
          console.log(`[zeroship] Server code changed: ${rel} — restarting runtime`);
          restartDevServer({
            root,
            port: devPort,
            serverEntry: options.serverEntry,
          });
          // Invalidate server module cache so next transform re-analyzes
          serverModuleCache.clear();
        }
      },

      buildEnd() {
        if (isDev) stopDevServer();
      },
    },

    // Plugin 3: Build — bundle server code after Vite finishes client
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
          console.log(`[zeroship] Server: ${result.bundlePath}`);
          if (result.serverFunctions.length > 0) {
            console.log(
              `[zeroship] RPC endpoints: ${result.serverFunctions.join(", ")}`
            );
          }
        } else {
          console.log("[zeroship] No server code — client-only");
        }

        // Build report
        if (serverFunctionMap.size > 0) {
          console.log("\n[zeroship] Server/client split:");
          for (const [file, fns] of serverFunctionMap) {
            console.log(`  ${file}: ${fns.join(", ")}`);
          }
        }
      },
    },
  ];
}

// Re-export for convenience
export { analyzeModule, isServerModuleQuick } from "./analyze.js";
export { transformToClient, generateStubs } from "./stub.js";
export { buildServerBundle } from "./server-bundle.js";

export default zeroship;
