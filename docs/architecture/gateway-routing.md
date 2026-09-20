# Gateway routing

Current request dispatch lives in `crates/zeroship-gateway`. The hot path is the compiled resource tree from `zeroship_bundle::Manifest`, not the older rule walker.
That compile step replaced the rule walker so inheritance flattening and RPC/path indexing happen once during route-cache updates instead of being recomputed on every request.

## Relevant files

- [crates/zeroship-bundle/src/manifest.rs](../../crates/zeroship-bundle/src/manifest.rs): `Manifest`
- [crates/zeroship-bundle/src/rule.rs](../../crates/zeroship-bundle/src/rule.rs): `ResourceEntry`, `RequiredPrincipal`, `ProcedureKind`, `Cors`, `RateLimit`
- [crates/zeroship-bundle/src/compiled.rs](../../crates/zeroship-bundle/src/compiled.rs): `CompiledManifest`, `EffectivePolicy`, `ResolvedAction`, `admit`. It lives in the manifest crate, NOT in the gateway, because the gateway and the worker both resolve the declared policy and must reach the same answer
- [crates/zeroship-gateway/src/router/dispatch.rs](../../crates/zeroship-gateway/src/router/dispatch.rs): request entry and policy enforcement
- [crates/zeroship-worker/src/policy.rs](../../crates/zeroship-worker/src/policy.rs): the SECOND enforcement of the same declared policy, in the worker, before creator code runs
- [crates/zeroship-gateway/src/router/static_serve.rs](../../crates/zeroship-gateway/src/router/static_serve.rs): static asset path
- [crates/zeroship-gateway/src/sync.rs](../../crates/zeroship-gateway/src/sync.rs): `/internal/routes` poller and route cache

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

- **`rpc:` procedures fail closed — they default to `auth: user`.** A server function nobody gave an explicit policy still requires an authenticated session, so a forgotten or mistyped resource key can never *silently* expose it. This is the root-cause fix for the SEC-5 class (a drifted `rpc:apps` vs `projects.*` family key had left the whole surface anonymous); see `crates/zeroship-bundle/src/compiled.rs::resolve_effective_policy`.
- **URL / SSR / static resources stay public by default (`auth: anonymous`)** — the web norm: a creator's blog, landing page, or static asset is readable without login.

To expose an RPC procedure publicly, opt in **explicitly** with `auth: "anonymous"` + `publicly_accessible: true` on the procedure or a `rpc:<prefix>` family policy (the `publicly_accessible` flag is the deliberate confirmation the manifest validator requires alongside `auth: anonymous`). `RequiredPrincipal` has exactly two variants, so merging a chain is a boolean OR and there is no strictness ladder.

### Two enforcers, one resolution

**This section said "manifest auth is enforced only by the gateway" until 2026-09-05.** It is now enforced twice.

The worker refuses a dispatch its own copy of the manifest does not admit, in Rust, before the creator's handler is entered ([crates/zeroship-worker/src/policy.rs](../../crates/zeroship-worker/src/policy.rs)). That is not redundancy: the signed `ZeroShip-User` header exists precisely because a caller with direct network access to a worker never passes through the gateway, and until this landed the format was designed for an enforcer that was never built. The worker's only other gate was `env.auth.requireUser()`, which is creator-called and optional — a creator who forgot had no fence at all.

Both fences resolve through the SAME `CompiledManifest`, which is why `compiled.rs` lives in `zeroship-bundle` rather than in the gateway. Two tiers computing "which resource is this, and what does it require" differently would make the second one worse than useless.

The two fences are NOT interchangeable. The gateway still owns routing (`404 no resource matched`), rate limits, spend and account gates, CORS, CSRF, idempotency and the method-vs-kind gate; the worker rules only on the declared principal and `required_scopes`, and admits a path that matches no declared resource rather than inventing a second routing verdict.

**The dev tier is unchanged and is a real divergence.** `pnpm dev` and the single-tenant CLI `serve` run `crates/zeroship-runtime/src/core/serve.rs`, not the worker, and no manifest exists at dev time — so neither gates by manifest policy, exactly as before. What the worker fence reads is the resolved principal (`Option<&str>` of the `ZeroShip-User` payload), which is the same value `dev_auth::resolve_dev_user_json` produces from the dev session cookie, so a dev identity would satisfy it unchanged the day dev gains a manifest. There is no dev bypass to remove because there is no dev arm.

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

Idempotency lives in [idempotency.rs](../../crates/zeroship-gateway/src/idempotency.rs).

Current behavior:

- only applies when the compiled policy sets `idempotent: true`
- only runs for `WorkerRpc`
- only applies to mutation/action-style RPCs
- returns cached responses directly on a hit

The default store in `main.rs` is `InMemoryIdempotencyStore`.

## Route cache updates

Gateway polls `/internal/routes` every 5 seconds. The required
`GatewaySnapshot` carries both routes and principal lifecycle denials. The
gateway consumes global IDs and persisted per-app pairwise subjects and rejects
app credentials when the snapshot is missing or stale. `RouteCache::update`
validates each manifest before compiling the route.

## Where to start

| Change | Start here |
| --- | --- |
| Resource lookup / policy merge | [compiled.rs](../../crates/zeroship-bundle/src/compiled.rs) |
| Request gating | [dispatch.rs](../../crates/zeroship-gateway/src/router/dispatch.rs) |
| The worker's own declared-policy fence | [policy.rs](../../crates/zeroship-worker/src/policy.rs) |
| Static asset behavior | [static_serve.rs](../../crates/zeroship-gateway/src/router/static_serve.rs) |
| Route sync | [sync.rs](../../crates/zeroship-gateway/src/sync.rs) |

## Related docs

- [docs/architecture/overview.md](../architecture/overview.md): entry point and system map.
- [docs/architecture/distributed.md](../architecture/distributed.md): end-to-end request flow across gateway, worker, and control.
- [docs/architecture/control-plane.md](../architecture/control-plane.md): where route state and manifests come from.
- [docs/architecture/blob-store.md](../architecture/blob-store.md): blob-backed static asset and bundle storage.
- [docs/reference/zeroship-standard.md](../reference/zeroship-standard.md): the `/__zeroship/v1` dispatch contract.
