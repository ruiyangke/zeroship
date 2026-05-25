// sdks/vite-plugin/src/environment.ts
//
// Vite Environment API integration for zeroship's V8 runtime.
// The runtime fetches modules and polls for HMR updates over HTTP; no
// bidirectional transport is opened from V8 back to Vite.

import * as vite from "vite";
import type { FetchFunctionOptions, FetchResult } from "vite/module-runner";
import { getNodeCompatId, getCustomPolyfillCode, isRuntimeNative } from "./node-compat.js";

const NODEISH_IMPORT_RE = /^(crypto|buffer|path|util|events|stream|os|url|http|https|fs|assert|process|async_hooks|timers|string_decoder|querystring|punycode|net|tls|dns|zlib|worker_threads|diagnostics_channel|perf_hooks|module)(\/.+)?$/;

// ── DevEnvironment ─────────────────────────────────────────────────────────

/**
 * A `vite.DevEnvironment` for the zeroship runtime.
 *
 * Real dev traffic runs over HTTP:
 *   - POST `/__zeroship_fetch` for `fetchModule` / `getBuiltins`
 *   - GET `/__zeroship_hmr_check` for poll-based invalidation
 *
 * We still opt into `hot: true` so Vite gives the environment a normalized
 * local channel, but no network transport is attached because V8 never opens
 * a HotChannel socket.
 */
export class ZeroshipDevEnvironment extends vite.DevEnvironment {
  private readonly builtinFetchCache = new Map<string, FetchResult>();
  private readonly builtinTransformCache = new Map<string, Promise<FetchResult | null>>();

  constructor(name: string, config: vite.ResolvedConfig) {
    super(name, config, { hot: true });
  }

  /**
   * Override fetchModule to intercept node:* builtins.
   *
   * Vite treats node:* as builtins and returns `{ externalize }` BEFORE
   * any plugin resolveId hook runs. Since our V8 runtime can't import
   * node: modules natively, we intercept them here and route to either:
   *   - Custom polyfill (crypto) → return code directly
   *   - unenv polyfill (buffer, path, etc.) → rewrite to unenv path, fetch normally
   */
  override async fetchModule(
    id: string,
    importer?: string,
    options?: FetchFunctionOptions,
  ): Promise<vite.FetchResult> {
    // Check if this is a node:* or bare builtin we can polyfill
    const isNodeish = id.startsWith("node:") || NODEISH_IMPORT_RE.test(id);

    if (isNodeish) {
      if (options?.cached) {
        return { cache: true } as vite.FetchResult;
      }

      const cached = this.builtinFetchCache.get(id);
      if (cached) {
        return cached;
      }

      // Runtime-native specifiers (`node:async_hooks`, `node:crypto`)
      // are owned by the V8 runtime's SyntheticModule loader. In dev,
      // ModuleRunner can't issue native imports — bridge through the
      // runtime-installed `__zeroshipNodeBuiltin` helper that returns
      // the same namespace object the synthetic module exposes.
      if (isRuntimeNative(id)) {
        const code = `
const m = globalThis.__zeroshipNodeBuiltin && globalThis.__zeroshipNodeBuiltin(${JSON.stringify(id)});
if (!m) throw new Error(${JSON.stringify(`${id}: runtime native module helper missing`)});
Object.assign(__vite_ssr_exports__, m, { default: m.default ?? m });
`;
        const result = { id, url: id, code, file: id } as vite.FetchResult;
        this.builtinFetchCache.set(id, result);
        return result;
      }

      const compatId = getNodeCompatId(id);
      if (compatId) {
        // Custom polyfill (e.g. crypto) → return code directly
        const code = getCustomPolyfillCode(compatId);
        if (code) {
          const result = { id, url: id, code, file: id } as vite.FetchResult;
          this.builtinFetchCache.set(id, result);
          return result;
        }
        // unenv polyfill → resolve and transform through Vite's pipeline.
        // We use transformRequest() which runs resolveId → load → transform,
        // converting the unenv module to SSR-compatible code.
        let pending = this.builtinTransformCache.get(id);
        if (!pending) {
          pending = this.transformRequest(compatId)
            .then((transformed) => {
              if (!transformed) return null;
              const result = {
                id,
                url: compatId,
                code: transformed.code,
                file: compatId,
              } as vite.FetchResult;
              this.builtinFetchCache.set(id, result);
              return result;
            })
            .finally(() => {
              if (!this.builtinFetchCache.has(id)) {
                this.builtinTransformCache.delete(id);
              }
            });
          this.builtinTransformCache.set(id, pending);
        }
        const transformed = await pending;
        if (transformed) {
          return transformed;
        }
      }
    }

    return super.fetchModule(id, importer, options);
  }
}

// ── Environment options factory ────────────────────────────────────────────

/**
 * Returns the `vite.EnvironmentOptions` block to register the zeroship
 * server environment.  Place this under
 * `environments: { zeroship: createZeroshipEnvironmentOptions() }` in your
 * `vite.config.ts`.
 *
 * @param serverEntry  Absolute path to the user's server entry file. Passed
 *                     to Vite as `optimizeDeps.entries` so dep discovery runs
 *                     at Vite startup (before the ModuleRunner issues any
 *                     fetchModule calls). Without this, Vite discovers deps
 *                     lazily as modules are imported; re-optimization then
 *                     invalidates previously-hashed files and the
 *                     ModuleRunner hits "file does not exist" on stale URLs.
 */
export function createZeroshipEnvironmentOptions(
  serverEntry?: string,
): vite.EnvironmentOptions {
  return {
    consumer: "server",
    resolve: {
      conditions: ["zeroship", "worker", "module", "import", "default"],
      noExternal: true,
    },
    dev: {
      createEnvironment(name: string, config: vite.ResolvedConfig, _context?: any): ZeroshipDevEnvironment {
        return new ZeroshipDevEnvironment(name, config);
      },
    },
    build: { target: "es2024" },
    keepProcessEnv: true,
    // Enable dep optimization (CJS → ESM conversion).
    optimizeDeps: {
      noDiscovery: false,
      // Prevent mid-request reloads when new deps are discovered.
      // The ModuleRunner can't handle pre-bundle version changes.
      ignoreOutdatedRequests: true,
      // Pre-crawl the server entry so all deps are discovered at Vite
      // startup, not lazily on first request. If the entry isn't known
      // (no detectable server file), fall back to scanning common paths.
      entries: serverEntry
        ? [serverEntry]
        : ["src/index.{ts,tsx,js,jsx}", "src/server.{ts,js}", "server.{ts,js}"],
      // The framework-internal bootstrap package is wired up via
      // resolve.alias above (pointing at the workspace install).
      // Users don't import it themselves — the dev-bootstrap loads
      // `@zeroship/bootstrap/install-schema` through the ModuleRunner
      // for `instanceof TypeBuilder` identity matching with the
      // user's `t.*` builders.
    },
  };
}
