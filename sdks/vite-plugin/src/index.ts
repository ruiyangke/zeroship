/**
 * @zeroship/vite-plugin — full-stack Vite plugin for zeroship.
 *
 * Usage:
 *   import { zeroship } from '@zeroship/vite-plugin'
 *   export default defineConfig({ plugins: [react(), zeroship()] })
 *
 * How it works:
 *   1. transform: discovers `"use server"` modules, rewrites RPC exports
 *      into client stubs, and appends the dev registry hooks the runtime
 *      bootstrap consumes
 *   2. environment: registers zeroship DevEnvironment with Vite
 *   3. dev-server: spawns the dev runtime, exposes HTTP module fetch +
 *      HMR poll endpoints, and proxies runtime-bound requests
 *   4. build: bundles server code for production via esbuild
 */

/// <reference path="./client-manifest.d.ts" />

import type { Plugin } from "vite";
import { DEFAULT_RPC_ENDPOINT } from "./constants.js";
import { transformPlugin, type TransformState } from "./transform.js";
import { devServerPlugin } from "./dev-server.js";
import { buildPlugin } from "./build.js";
import { nodeCompatPlugin, nodeInjectPlugin } from "./node-compat.js";
import { zeroshipModulePlugin, zeroshipBootstrapResolverPlugin } from "./zeroship-module.js";

/** One configured dev-auth user (all fields optional; sensible defaults). */
export interface DevAuthUser {
  /** Opaque per-app pairwise subject. Defaults to a stable `pws_dev…`. */
  id?: string;
  /** Per-app email (or relay alias). Defaults to `dev@localhost`. */
  email?: string | null;
  name?: string | null;
  avatar?: string | null;
  /** Granted scopes. Defaults to `["openid","profile","email"]`. */
  scopes?: string[];
  /**
   * Password the dev login form prefills + validates for this user. Defaults
   * to the well-known dev password (`"dev"`). Not a secret — it only makes the
   * dev credential check (and its failure path) real.
   */
  password?: string;
}

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
  /**
   * Dev-tier auth (the `pnpm dev` impl of the platform auth contract — the
   * peer of `env.db`→SQLite / `env.kv`→redb). When enabled the dev runtime
   * serves the same-origin `/__zeroship/auth/*` endpoints the `@zeroship/auth` client
   * drives and supplies a logged-in identity to `env.auth.getUser()` /
   * `currentUser()` server-side — with NO gateway / Hydra / control plane.
   *
   * - `true` (the default in dev) — a single built-in dev user
   *   (`pws_dev…` / `dev@localhost` / scopes `openid profile email`).
   * - `{ user: {...} }` — one configured dev user.
   * - `{ users: [...], defaultUserId? }` — multiple users; `/authorize`
   *   renders a tiny dev picker so you can switch identities / scope sets.
   * - `false` — disable; `/__zeroship/auth/*` falls through to the user module and
   *   `env.auth.getUser()` returns `null` (anonymous).
   *
   * Dev-only by construction: this provider lives in the dev runtime
   * (`@zeroship/bootstrap/dev`) and is structurally absent from any production
   * `.zship` build.
   */
  devAuth?: boolean | DevAuthUser | { user: DevAuthUser } | { users: DevAuthUser[]; defaultUserId?: string };
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
    zeroshipBootstrapResolverPlugin(),
    transformPlugin(rpcEndpoint, state),
    ...devServerPlugin(options, state),
    buildPlugin(state, {
      serverEntry: options.serverEntry,
      mode: options.mode ?? "full",
    }),
  ];
}

export default zeroship;
