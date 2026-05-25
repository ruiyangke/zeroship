# `@zeroship/vite-plugin` and the Vite Environment API

`@zeroship/vite-plugin` registers a custom `zeroship` Vite environment and runs server code through the real zeroship runtime during local development.

Current implementation:

- [sdks/vite-plugin/src/index.ts](sdks/vite-plugin/src/index.ts)
- [sdks/vite-plugin/src/dev-server.ts](sdks/vite-plugin/src/dev-server.ts)
- [sdks/vite-plugin/src/environment.ts](sdks/vite-plugin/src/environment.ts)
- [sdks/vite-plugin/src/dev-bootstrap/index.ts](sdks/vite-plugin/src/dev-bootstrap/index.ts)
- [sdks/vite-plugin/src/dev-bootstrap/transport.ts](sdks/vite-plugin/src/dev-bootstrap/transport.ts)

## Public plugin options

`zeroship(options)` accepts:

- `rpcEndpoint?: string` — defaults to `"/_rpc"`
- `serverEntry?: string` — explicit server entry; otherwise auto-detected
- `devServerPort?: number` — defaults to `3001`
- `mode?: "full" | "static"` — production build mode
- `rpc?.strict?: "auto" | "always" | "never"` — RPC strictness policy

The public options type lives in [sdks/vite-plugin/src/index.ts](sdks/vite-plugin/src/index.ts).

## What the plugin registers

`zeroship()` currently composes these pieces:

- node compat plugins
- the `zeroship` virtual-module resolver
- the server transform plugin
- the dev-server/environment pair
- the production build plugin

The custom environment is registered under `environments.zeroship` via `createZeroshipEnvironmentOptions` in [sdks/vite-plugin/src/environment.ts](sdks/vite-plugin/src/environment.ts).

That environment currently sets:

- `consumer: "server"`
- resolve conditions: `["zeroship", "worker", "module", "import", "default"]`
- `resolve.noExternal = true`
- `build.target = "es2024"`
- `keepProcessEnv = true`

## Dev request flow

In dev, [sdks/vite-plugin/src/dev-server.ts](sdks/vite-plugin/src/dev-server.ts) does three important things:

1. Registers the `zeroship` environment.
2. Spawns `zeroship serve dist/dev-bootstrap.js --port=<devServerPort> --workers=1`.
3. Proxies `/_zs/v1/*`, `/_rpc`, `/rpc`, and `/api/*` to that child runtime.

The child process receives these plugin-internal env vars from [sdks/vite-plugin/src/constants.ts](sdks/vite-plugin/src/constants.ts):

- `ZEROSHIP_DEV`
- `ZEROSHIP_VITE_WS`
- `ZEROSHIP_ENTRY`

If neither `.env` nor the parent environment provides `DATABASE_URL`, the dev server prepares a project-local SQLite database through [sdks/vite-plugin/src/dev-db.ts](sdks/vite-plugin/src/dev-db.ts) and passes that URL to the child process.

## Module loading in dev

The dev bootstrap in [sdks/vite-plugin/src/dev-bootstrap/index.ts](sdks/vite-plugin/src/dev-bootstrap/index.ts):

- creates a `ModuleRunner`
- loads the user server entry on demand
- routes schema installation through `@zeroship/bootstrap/dev`
- keeps a procedure registry for transform-emitted `__register(...)` calls

The current transport is HTTP, not a bidirectional ModuleRunner HMR channel. [sdks/vite-plugin/src/dev-bootstrap/transport.ts](sdks/vite-plugin/src/dev-bootstrap/transport.ts) sends module requests to:

- `POST /__zeroship_fetch`

It explicitly constructs `ModuleRunner({ transport, hmr: false }, ...)`.

## Hot reload behavior

Hot reload is currently poll-based:

- Vite records changed `.ts`, `.tsx`, `.js`, and `.jsx` files in `hotUpdate(...)`
- the runtime polls `GET /__zeroship_hmr_check`
- the bootstrap invalidates the changed entries from `runner.evaluatedModules`
- the next import re-fetches transformed code from Vite

This is why normal source edits do not require restarting the dev runtime, even though ModuleRunner's built-in bidirectional HMR transport is disabled.

## Runtime shims

Two runtime-specific shims matter in dev:

- `ZeroshipDevEnvironment.fetchModule(...)` in [sdks/vite-plugin/src/environment.ts](sdks/vite-plugin/src/environment.ts) intercepts `node:*` and other builtins so the runner can use zeroship-native or polyfilled modules instead of Node resolution.
- [sdks/vite-plugin/src/zeroship-module.ts](sdks/vite-plugin/src/zeroship-module.ts) provides the dev-side `zeroship` virtual module so imports match the runtime-installed module shape.
