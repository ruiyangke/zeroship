# auth-notes

The `env.auth` coverage example (G3 / ISS-54). A minimal **server-only**
zeroship app (no client bundle — ISS-59) that proves the gateway→worker
identity chain reaches the `env.auth` kernel primitive at the worker tier.

The platform HMAC-signs the authenticated identity into the `ZeroShip-User`
header; the worker verifies + parses it and exposes it via the kernel
`env.auth.getUser()` / `requireUser()` primitives, which the `@zeroship/auth`
server helper (`auth.getUser()` / `auth.requireUser()`) wraps.

## RPC procedures

| id                  | kind     | behaviour |
| ------------------- | -------- | --------- |
| `auth.whoami`       | query    | `auth.getUser()` — `{ user: User \| null }`. Raw identity probe: `null` when anonymous, the caller otherwise. Never throws. |
| `auth.whoamiStrict` | query    | RAW `auth.requireUser()` with **no** app-level catch — the kernel throw escapes verbatim, documenting the kernel's actual failure shape for an anonymous caller. |
| `auth.notes.list`   | query    | App-level `requireSignedIn()` (clean **401** when anonymous), then reads THIS user's notes from `env.kv`, scoped by `user.id`. |
| `auth.notes.add`    | mutation | `requireSignedIn()`, then appends a note to the caller's own `env.kv`-scoped list. |

Notes are stored in `env.kv` (keyed `auth-notes:notes:<user.id>`) so the example
needs no Postgres schema — the point under test is that the **identity** flows
to `env.auth`, not the storage backend. Per-user keying is what makes the
cross-user isolation assertion meaningful: a different injected identity sees a
different (empty) list, never the first caller's notes.

## Kernel-surface note (a finding)

`auth.requireUser()` throws a plain `Error("Authentication required")` with **no
`.status`**. The RPC fetch-handler maps a status-less throw to HTTP **500** (its
default) — *not* 401. So `auth.whoamiStrict` returns 500 for an anonymous
caller. App code that wants a real 401 must gate on `getUser()` and throw a
status-bearing error itself, as `requireSignedIn()` does here (`auth.notes.list`
→ 401 when anon). See `docs/reference/auth.md`, which describes the dispatch
path as translating the throw "into a 401" — the kernel callback does not set
that status today.

## Build

```bash
pnpm install
pnpm build      # → dist/app.zship  (4 server functions, 1 worker module)
```

## Coverage harness

`tests/e2e_app_primitives_auth.sh` deploys this app to a clean ephemeral stack
and exercises both the anonymous and the authenticated paths over the worker
`/dispatch` edge, injecting a request-bound, HMAC-signed `ZeroShip-User` header
(empty dev `worker_key`) to flow a test identity into `env.auth`.
