# Auth

Zeroship currently splits auth across the control plane, the gateway, and the `@zeroship/auth` browser/runtime helper.

## Control-plane flow

The control service exposes the current auth endpoints in [crates/control/src/main.rs](../../crates/control/src/main.rs):

- `POST /auth/login`
- `POST /auth/logout`
- `GET /auth/userinfo`
- `POST /auth/consent`
- `GET /auth/authorize`
- `GET /auth/google/start`
- `GET /auth/google/callback`

Session handling is implemented in [crates/control/src/auth_service.rs](../../crates/control/src/auth_service.rs) and [crates/control/src/auth_handlers.rs](../../crates/control/src/auth_handlers.rs). The session cookie name is `__zs_session`.

## Gateway behavior

The gateway validates the session cookie and injects a `ZeroShip-User` header for worker requests. The current logic is in [crates/gateway/src/user_auth.rs](../../crates/gateway/src/user_auth.rs).

For HTML requests that need login, the gateway redirects to the control plane authorize route. That flow is implemented in [crates/gateway/src/router/dispatch.rs](../../crates/gateway/src/router/dispatch.rs).

## SDK surface

The package name is `@zeroship/auth`, with a single root export defined in [sdks/auth/package.json](../../sdks/auth/package.json). There is no `@zeroship/auth/client` export.

The current helper lives in [sdks/auth/src/index.ts](../../sdks/auth/src/index.ts):

- `auth.getUser()`
- `auth.requireUser()`
- `auth.signOut()`

`auth.getUser()` first checks `env.auth` if it exists, then falls back to `window.__zs_user` in the browser, and otherwise returns `null`.

## Current limitation

The runtime-side auth helper exists in [crates/runtime/src/auth.rs](../../crates/runtime/src/auth.rs), but the source trees listed above do not currently register a native `env.auth` plugin alongside `env.db`, `env.kv`, or `env.storage`. The JS helper therefore carries the browser fallback path in [sdks/auth/src/index.ts](../../sdks/auth/src/index.ts).
