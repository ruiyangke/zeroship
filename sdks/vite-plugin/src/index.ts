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
import { transformPlugin, type TransformState } from "./transform.js";
import {
  createProjectConfigHolder,
  type ProjectConfigHolder,
  type ProjectConfigInput,
  type ProjectConfigOverride,
} from "./project-config/index.js";
import { devServerPlugin } from "./dev-server.js";
import { buildPlugin } from "./build.js";
import { nodeCompatPlugin, nodeInjectPlugin } from "./node-compat.js";
import { zeroshipModulePlugin, zeroshipBootstrapResolverPlugin } from "./zeroship-module.js";

/**
 * One configured dev-auth user (all fields optional; sensible defaults).
 *
 * There is no `password` field. The dev login form prefills + validates a
 * password DERIVED from `id` (`devPasswordFor` in
 * `sdks/bootstrap/src/dev-auth.ts`): `"dev-"` + the first 8 characters of the
 * id after `pws_`, e.g. `pws_alice000000000000000` -> `dev-alice000`. It is
 * not a secret; it only makes the dev credential check (and its failure path)
 * real, and it is deliberately short enough that the deployed platform's
 * signup policy refuses it.
 */
export interface DevAuthUser {
  /** Opaque per-app pairwise subject. Defaults to a stable `pws_dev…`. */
  id?: string;
  /** Per-app email (or relay alias). Defaults to `dev@localhost`. */
  email?: string | null;
  name?: string | null;
  avatar?: string | null;
  /** Granted scopes. Defaults to `["openid","profile","email"]`. */
  scopes?: string[];
}

/**
 * The plugin's option bag.
 *
 * IT IS SMALL ON PURPOSE. `rpcEndpoint`, `serverEntry`, `mode` and
 * `migrations.*` used to live here and are now in `zeroship.jsonc`, because
 * every one of them was a fact the Rust CLI also needed and could not read.
 * What remains varies per developer machine (`devServerPort`, `devAuth`) plus
 * three levers that point AT the file rather than duplicating it (`configPath`,
 * `env`, `config`). Cloudflare's Vite plugin converged on the same split.
 */
export interface ZeroshipOptions {
  /** Port for the zeroship dev server (default: 3001) */
  devServerPort?: number;
  /**
   * Path to `zeroship.jsonc`, absolute or relative to the Vite root.
   *
   * An explicit `configPath` takes precedence over `ZEROSHIP_CONFIG` and
   * auto-discovery in the app root. A path that does not exist THROWS - only
   * auto-discovery may come up empty.
   */
  configPath?: string;
  /**
   * Select a named entry from the file's `environments` block.
   *
   * The plugin's equivalent of the CLI's `--env=`. There is no implicit
   * environment and no `ZEROSHIP_ENV`: a variable that silently switched which
   * target a build was shaped for is the same hazard as one that switched
   * which database got migrated.
   */
  env?: string;
  /**
   * Escape hatch: customise the resolved config programmatically.
   *
   * ```ts
   * zeroship({ config: (c) => ({ ...c, build: { ...c.build, mode: process.env.SSG ? "static" : "full" } }) })
   * ```
   *
   * A partial object (shallow-merged) or a function applied AFTER the file
   * loads and after environment selection. It MAY NOT change any field the
   * Rust CLI also reads (`name`, `app`, `control`, `runtime_date`, `build.output`,
   * `migrations.dir`, `migrations.out`) - the CLI cannot run a JavaScript
   * function, so an override there would reintroduce the exact drift the file
   * removes. The deny-list is generated from the schema, so it cannot fall
   * behind. Attempting one is an error naming the field.
   */
  config?: ProjectConfigOverride;
  /**
   * Dev-tier auth (the `pnpm dev` impl of the platform auth contract — the
   * peer of `env.db`→SQLite / `env.kv`→redb). When enabled the dev runtime
   * serves the same-origin `/__zeroship/auth/*` endpoints the `@zeroship/auth` client
   * drives and supplies a logged-in identity to `env.auth.getUser()` /
   * `currentUser()` server-side — with NO gateway / external auth service / control plane.
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
}

export function zeroship(options: ZeroshipOptions = {}): Plugin[] {
  // Shared state across plugins
  const state: TransformState = {
    serverFunctionMap: new Map(),
    discoveredProcedures: [],
    discoveredSchedules: [],
    discoveredWorkflows: [],
  };

  // ONE reader, shared by the build and dev-server plugins. Two independent
  // reads of the same file is how the two halves of one tool come to disagree,
  // which is the shape this whole change exists to remove -- so the holder
  // memoises per root and both plugins take the same instance.
  const input: ProjectConfigInput = {
    configPath: options.configPath,
    environment: options.env,
    override: options.config,
  };
  const project: ProjectConfigHolder = createProjectConfigHolder(input);

  return [
    nodeCompatPlugin(),
    nodeInjectPlugin(),
    zeroshipModulePlugin(),
    zeroshipBootstrapResolverPlugin(),
    transformPlugin(state),
    ...devServerPlugin(options, state, project),
    buildPlugin(state, project),
  ];
}

export default zeroship;
