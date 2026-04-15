/**
 * @zeroship/vite-plugin — full-stack Vite plugin for zeroship.
 *
 * Usage:
 *   import { zeroship } from '@zeroship/vite-plugin'
 *   export default defineConfig({ plugins: [react(), zeroship()] })
 *
 * How it works:
 *   1. transform: detects "use server" + taint analysis → RPC stubs in client
 *   2. environment: registers zeroship DevEnvironment with Vite
 *   3. dev-server: spawns V8 runtime, WS bridge, proxy middleware
 *   4. build: bundles server code for production via esbuild
 */

import type { Plugin } from "vite";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";
import { transformPlugin, type TransformState } from "./transform.js";
import { devServerPlugin } from "./dev-server.js";
import { buildPlugin } from "./build.js";

export interface ZeroshipOptions {
  /** RPC endpoint path (default: "/_rpc") */
  rpcEndpoint?: string;
  /** Server entry point (auto-detected if not specified) */
  serverEntry?: string;
  /** Port for the zeroship dev server (default: 3001) */
  devServerPort?: number;
}

export function zeroship(options: ZeroshipOptions = {}): Plugin[] {
  const rpcEndpoint = options.rpcEndpoint ?? DEFAULT_RPC_ENDPOINT;

  // Shared state across plugins
  const state: TransformState = {
    serverModuleCache: new Map(),
    serverFunctionMap: new Map(),
    knownServerSources: new Set(),
  };

  return [
    transformPlugin(rpcEndpoint, state),
    ...devServerPlugin(options, state),
    buildPlugin(state),
  ];
}

export default zeroship;
