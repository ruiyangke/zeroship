# Gateway routing

Current request dispatch lives in `crates/gateway`. The hot path is the compiled resource tree from `zeroship_bundle::Manifest`, not the older rule walker.
That compile step replaced the rule walker so inheritance flattening and RPC/path indexing happen once during route-cache updates instead of being recomputed on every request.

## Relevant files

- [crates/bundle/src/manifest.rs](../../crates/bundle/src/manifest.rs): `Manifest`
- [crates/bundle/src/rule.rs](../../crates/bundle/src/rule.rs): `ResourceEntry`, `AuthLevel`, `ProcedureKind`, `Cors`, `RateLimit`
- [crates/gateway/src/compiled.rs](../../crates/gateway/src/compiled.rs): `CompiledManifest`, `EffectivePolicy`, `ResolvedAction`
- [crates/gateway/src/router/dispatch.rs](../../crates/gateway/src/router/dispatch.rs): request entry and policy enforcement
- [crates/gateway/src/router/static_serve.rs](../../crates/gateway/src/router/static_serve.rs): static asset path
- [crates/gateway/src/sync.rs](../../crates/gateway/src/sync.rs): `/internal/routes` poller and route cache

## Request pipeline

```text
HTTP request
  -> extract app name from path or Host
  -> RouteCache lookup
  -> compiled manifest lookup
     - RPC: strip `/__zeroship/v1/` and hit `rpc_index`
     - URL: literal match first, then glob match
  -> enforce EffectivePolicy
  -> execute ResolvedAction
```

`ResolvedAction` is one of:

- `WorkerRpc`
- `WorkerSsr`
- `Redirect { to, status }`
- `Rewrite { to }`
- `Static { try_chain }`

## Current manifest shape

The current gateway consumes `Manifest.resources`, `Manifest.assets`, `Manifest.runtime_assets`, `Manifest.worker`, `Manifest.schemas`, and `Manifest.asset_version`.

`Manifest.version` is currently validated as `1`. The gateway does not dispatch from a `rules` array.

Resource keys have three forms:

- `rpc:<wireId>`
- `/<path>` or `/<path-with-globs>`
- `*`

`*` is the inheritance root during policy compilation. URL dispatch still depends on an explicit path or glob entry.
When present, it sits at the head of each resource's inheritance chain and contributes defaults, but it never matches a URL by itself.

## Compile step

`CompiledManifest::compile` builds:

- `effective_policies: HashMap<String, EffectivePolicy>`
- `rpc_index: HashMap<String, String>`
- `url_index`: literal-path table plus glob list ordered by specificity

Policy flattening is done once per route update. The compiled policy carries auth, rate-limit, CORS, CSRF origins, cache metadata, idempotency flags, middleware names, procedure kind, schemas, and the resolved action.

### Auth resolution default (secure by default)

When a resource's inheritance chain declares no `auth` at all, the default depends on the surface:

- **`rpc:` procedures fail closed — they default to `auth: user`.** A server function nobody gave an explicit policy still requires an authenticated session, so a forgotten or mistyped resource key can never *silently* expose it. This is the root-cause fix for the SEC-5 class (a drifted `rpc:apps` vs `projects.*` family key had left the whole surface anonymous); see `crates/gateway/src/compiled.rs::resolve_effective_policy`.
- **URL / SSR / static resources stay public by default (`auth: anon`)** — the web norm: a creator's blog, landing page, or static asset is readable without login.

To expose an RPC procedure publicly, opt in **explicitly** with `auth: "anon"` + `publicly_accessible: true` on the procedure or a `rpc:<prefix>` family policy (the `publicly_accessible` flag is the deliberate confirmation the manifest validator requires alongside `auth: anon`). Manifest auth is enforced only by the gateway — the single-tenant CLI `serve` and the dev runtime do not gate by manifest policy.

## Pre-dispatch gates

`execute_resource_tree` enforces these checks before the worker is touched:

1. resource lookup
2. method vs `ProcedureKind`
3. auth
4. CSRF origin allow-list
5. `max_input_bytes`
6. per-resource rate limit
7. idempotency pre-dispatch for eligible RPC mutations

Current `ProcedureKind` handling:

- `Query`: GET, POST, or HEAD
- `Mutation`: POST
- `Action`: unrestricted
- `Stream`: unrestricted
- `Subscription`: GET plus WebSocket upgrade headers

RPC wire URLs are `/__zeroship/v1/<wireId>`. The older `/_rpc/<method>` path is not part of the current gateway resource lookup.

## Static assets

Static serving is handled by `router/static_serve.rs`:

- resolves build-time `assets` and mutable `runtime_assets`
- negotiates pre-compressed variants
- supports `If-None-Match`
- supports byte ranges
- uses memory and disk LRUs before falling back to `BlobStore`

## Idempotency

Idempotency lives in [idempotency.rs](../../crates/gateway/src/idempotency.rs).

Current behavior:

- only applies when the compiled policy sets `idempotent: true`
- only runs for `WorkerRpc`
- only applies to mutation/action-style RPCs
- returns cached responses directly on a hit

The default store in `main.rs` is `InMemoryIdempotencyStore`.

## Route cache updates

Gateway polls `/internal/routes` every 5 seconds. `RouteCache::update` validates each manifest again and falls back to `Manifest::passthrough()` on validation failure before compiling the route.

## Where to start

| Change | Start here |
| --- | --- |
| Resource lookup / policy merge | [compiled.rs](../../crates/gateway/src/compiled.rs) |
| Request gating | [dispatch.rs](../../crates/gateway/src/router/dispatch.rs) |
| Static asset behavior | [static_serve.rs](../../crates/gateway/src/router/static_serve.rs) |
| Route sync | [sync.rs](../../crates/gateway/src/sync.rs) |

## Related docs

- [docs/architecture/overview.md](../architecture/overview.md): entry point and system map.
- [docs/architecture/distributed.md](../architecture/distributed.md): end-to-end request flow across gateway, worker, and control.
- [docs/architecture/control-plane.md](../architecture/control-plane.md): where route state and manifests come from.
- [docs/architecture/blob-store.md](../architecture/blob-store.md): blob-backed static asset and bundle storage.
- [docs/reference/zeroship-standard.md](../reference/zeroship-standard.md): the `/__zeroship/v1` dispatch contract.
