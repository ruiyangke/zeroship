/**
 * @zeroship/vite-plugin — full-stack Vite plugin for zeroship.
 *
 * Usage:
 *   import { zeroship } from '@zeroship/vite-plugin'
 *   export default defineConfig({ plugins: [react(), zeroship()] })
 *
 * How it works:
 *   1. transform: discovers server modules by path (src/server.{ts,tsx,js,jsx}
 *      or anywhere under src/server/**) and rewrites them into RPC stubs
 *      in the client environment + bare exports in the ssr environment
 *   2. environment: registers zeroship DevEnvironment with Vite
 *   3. dev-server: spawns V8 runtime, WS bridge, proxy middleware
 *   4. build: bundles server code for production via esbuild
 */

/// <reference path="./client-manifest.d.ts" />

import type { Plugin } from "vite";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";
import { transformPlugin, type TransformState } from "./transform.js";
import { devServerPlugin } from "./dev-server.js";
import { buildPlugin } from "./build.js";
import { nodeCompatPlugin, nodeInjectPlugin } from "./node-compat.js";
import { zeroshipModulePlugin } from "./zeroship-module.js";

export interface ZeroshipOptions {
  /** RPC endpoint path (default: "/_rpc") */
  rpcEndpoint?: string;
  /** Server entry point (auto-detected if not specified) */
  serverEntry?: string;
  /** Port for the zeroship dev server (default: 3001) */
  devServerPort?: number;
  /**
   * Build mode.
   *
   * - `"full"` (default): client + SSR builds; emits `worker` in the manifest.
   * - `"static"`: SSG-only deploy. Skips the SSR Rollup sub-build, and
   *   tells Vite that an empty `rollupOptions.input` is OK so users don't
   *   have to ship a placeholder `vite.empty.js`. The emitter walks `dist/`
   *   for HTML / CSS / images / etc. and packs them as assets; manifest's
   *   `worker` is omitted. Useful for static-site generators that copy
   *   prerendered HTML into `dist/` themselves.
   */
  mode?: "full" | "static";
  /** RPC v2 (`docs/proposals/rpc.md` §1) — server-function discovery + emission. */
  rpc?: {
    /**
     * Strict-mode posture for the reference-graph walk.
     *
     * - `"auto"` (default): strict in `mode === "production"`, lenient
     *   in dev. Matches the proposal's "Strict mode (production builds)"
     *   gate.
     * - `"always"`: strict regardless of mode.
     * - `"never"`: lenient regardless of mode (graph-only bindings
     *   warn, do not error).
     */
    strict?: "auto" | "always" | "never";
  };
}

/** Resolve the strict-mode posture against the current Vite mode. */
export function resolveRpcStrict(
  setting: "auto" | "always" | "never" | undefined,
  mode: string | undefined,
): "always" | "never" {
  if (setting === "always") return "always";
  if (setting === "never") return "never";
  // "auto" (default) — production gates strict; dev does not.
  return mode === "production" ? "always" : "never";
}

export function zeroship(options: ZeroshipOptions = {}): Plugin[] {
  const rpcEndpoint = options.rpcEndpoint ?? DEFAULT_RPC_ENDPOINT;

  // Shared state across plugins
  const state: TransformState = {
    serverFunctionMap: new Map(),
    discoveredProcedures: [],
  };

  return [
    nodeCompatPlugin(),
    nodeInjectPlugin(),
    zeroshipModulePlugin(),
    transformPlugin(rpcEndpoint, state),
    ...devServerPlugin(options, state),
    buildPlugin(state, {
      serverEntry: options.serverEntry,
      mode: options.mode ?? "full",
    }),
  ];
}

export default zeroship;
