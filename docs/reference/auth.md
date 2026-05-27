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

The helper lives in [sdks/auth/src/index.ts](../../sdks/auth/src/index.ts):

- `auth.getUser()` — returns the authenticated user from `env.auth.getUser()`, or `null` if the request is anonymous.
- `auth.requireUser()` — returns the user or throws `Authentication required`.
- `auth.isLoggedIn()` — convenience boolean.
- `auth.signOut(returnTo?)` — returns a 302 Response to `/__zs/auth/signout`.

The runtime injects the authenticated identity via `env.auth.user` (parsed from the gateway's HMAC-signed `ZeroShip-User` header); `auth.getUser()` reads it directly and `auth.requireUser()` throws a 401-shaped Error if absent. The previous `window.__zs_user` browser fallback has been removed — authenticated identity is server-side only, and client code that needs the user calls back through a server fetch handler / RPC.
