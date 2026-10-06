/**
 * @zeroship/vite-plugin — full-stack Vite plugin for zeroship.
 *
 * Usage:
 *   import { zeroship } from '@zeroship/vite-plugin'
 *   export default defineConfig({ plugins: [react(), zeroship()] })
 *
 * How it works:
 *   1. transform: discovers `"use server"` modules and rewrites RPC exports
 *      into client stubs while retaining server binding metadata
 *   2. environment: registers zeroship DevEnvironment with Vite
 *   3. dev-server: spawns the dev runtime, exposes HTTP module fetch +
 *      HMR poll endpoints, and proxies runtime-bound requests
 *   4. build: builds the client and then the worker (the `zeroship`
 *      environment) through Vite's app builder, from the app's own config,
 *      and packs the .zship
 */

/// <reference path="./client-manifest.d.ts" />

import type { Plugin } from "vite";
import type { ProjectConfigOverride } from "./project-config/index.js";
import { zeroshipPlugins } from "./plugins.js";

/**
 * One configured dev-auth user (all fields optional; sensible defaults).
 *
 * There is no `password` field. The dev login form prefills + validates a
 * password derived from `id` by the Vite dev-auth provider. It is visible in
 * the form and only exists to exercise the credential failure path locally.
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
 * `migrations.*` live in `zeroship.jsonc`, because
 * every one of them was a fact the Rust CLI also needed and could not read.
 * What remains varies per developer machine (`devServerPort`, `devAuth`) plus
 * three levers that point AT the file rather than duplicating it (`configPath`,
 * `env`, `config`). Cloudflare's Vite plugin converged on the same split.
 */
export interface ZeroshipOptions {
  /** Port for the zeroship dev server (default: 3001) */
  devServerPort?: number;
  /**
   * Which of the workspace's `apps` this build is.
   *
   * A workspace declaring one app implies it; one declaring several must say,
   * because the app decides which databases are folded, packed and served.
   * The value is a LOCAL LABEL from the file, never an app id: the id comes
   * from the file, so a label never travels as an identifier.
   */
  app?: string;
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
   * Rust CLI also reads (`name`, `control`, `runtime_date`, `build.output`, the
   * `databases` and `apps` maps, `secrets`) - the CLI cannot run a JavaScript
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
   * Dev-only by construction: Vite serves these routes as middleware and only
   * shares the cookie secret with the local runtime verifier. Production
   * `.zship` builds do not contain the provider.
   */
  devAuth?: boolean | DevAuthUser | { user: DevAuthUser } | { users: DevAuthUser[]; defaultUserId?: string };
}

export function zeroship(options: ZeroshipOptions = {}): Plugin[] {
  // Where the plugin takes the process environment. The project-config
  // locator and the dev server, with the runtime it spawns, take it as an
  // input instead of reading `process.env` themselves. It is the live object
  // rather than a copy, because the environment can change after this call:
  // once the config file has run, Vite writes `VITE_USER_NODE_ENV` from a
  // `.env` file's `NODE_ENV`, and `NODE_ENV` itself when that value is
  // `development`. A config file that writes `process.env` itself, as
  // meal-kit's `useWorkspaceEnvironment()` does, is honoured wherever in the
  // file the write sits.
  return zeroshipPlugins(options, process.env);
}

export default zeroship;
