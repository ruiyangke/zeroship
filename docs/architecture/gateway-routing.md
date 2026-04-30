# Gateway routing

Per-request flow through `crates/gateway`. This is the most actively edited surface — read this before touching `dispatch.rs`, `router.rs`, or `core/src/types.rs`'s `Manifest`.

## Pipeline

```
HTTP request
  │
  ├─ extract_app_name (subdomain or path-prefix)        crates/gateway/src/router.rs
  ├─ lookup_by_name → (Uuid, Arc<CompiledRoute>)        crates/gateway/src/sync.rs
  │
  ├─ COMPILED MANIFEST DISPATCH                          crates/gateway/src/compiled.rs
  │    walk pre-compiled rules in declaration order
  │    method bitset check (1 bit-AND)
  │    matcher (Exact/Prefix/Glob/Any)
  │    action → Outcome
  │
  ├─ Outcome::Static    → blob_cache.get → blob_store.get_blob → respond with cache headers
  ├─ Outcome::Worker    → forward via CHWBL hash ring → worker (V8)
  ├─ Outcome::Redirect  → 30x with Location header
  └─ Outcome::NotFound  → 404
```

Source manifest: `crates/core/src/types.rs` (`Manifest`, `Rule`, `Match`, `Action`). JSON wire format, stays serializable.

Compiled form: `crates/gateway/src/compiled.rs`. Built once at route-update time. Gateway-private.

## The `Manifest` JSON shape

```jsonc
{
  "rules": [
    {
      "match": { "kind": "prefix", "method": "POST", "path": "/_rpc/" },
      "action": { "kind": "worker", "mode": "rpc" }
    },
    {
      "match": { "kind": "glob", "path": "/blog/[slug]" },
      "action": { "kind": "worker", "mode": "ssr" }
    },
    {
      "match": { "kind": "any" },
      "action": {
        "kind": "static",
        "try": ["$path", "/index.html"],
        "cache": { "max_age": 60 }
      }
    }
  ],
  "assets": {
    "/index.html":     { "hash": "sha256-...", "content_type": "text/html",       "size": 2048 },
    "/_assets/main.js":{ "hash": "sha256-...", "content_type": "application/javascript", "size": 84203 }
  },
  "runtime_assets": {},
  "worker": { "entry": "index.js", "modules": { "index.js": "sha256-..." } },
  "asset_version": 0
}
```

- **`rules`** are walked first-match-wins. Specificity ordering is enforced at parse time by `Manifest::validate()` (shadow detection).
- **`assets`** is the immutable map of paths → content-addressed assets. Populated on deploy. Includes HTML shells, JS chunks, CSS, images, prerendered HTML — every static byte.
- **`runtime_assets`** is mutable, populated by the user's server code via `zeroship.assets.put(...)`. `asset_version` bumps on each mutation; the gateway re-syncs only when it changes.
- **`worker`** is the JS code that runs in V8 (`entry` specifier + `modules` map of specifier → blob hash). `null` for SSG-only deploys. The hash set is distinct from any asset hashes, so an asset-only deploy doesn't bust the worker's V8 isolate.

## `Match` variants

| Variant | What it matches | Captures |
| --- | --- | --- |
| `Exact { method?, path }` | path equality | none |
| `Prefix { method?, path }` | segment-aware prefix; `/admin` matches `/admin`, `/admin/`, `/admin/users`, **not** `/administrator` | none |
| `Glob { method?, path }` | Next.js-style: `[slug]` (single segment), `[...rest]` (catch-all, must be last), `*` (anonymous single segment) | named, by capture group |
| `Any` | every request | none |

Method-omitted (or `"*"`) means all methods.

## `Action` variants

| Variant | Effect |
| --- | --- |
| `Static { try, cache?, status? }` | Walk `try` chain (each entry is a template). First entry that resolves to an asset wins. Substitutes `$path` and `[name]`. |
| `Worker { mode, cache?, rate_limit? }` | Forward to worker. `mode = rpc` checks API key first; `mode = ssr` is open. |
| `Redirect { to, status }` | HTTP 30x. `to` accepts the same templates as `Static`. |
| `Rewrite { to }` | Internal rewrite. Restart the rule walk with the new path. Bounded by `MAX_HOPS = 8`. |

## Two non-obvious invariants

1. **`Action::Static` only fires for GET/HEAD.** Even if the matching rule is `Match::Any`, a POST/PUT/DELETE skips Static rules and falls to the next match. Without this, `Any → Static` would serve the SPA shell HTML on POST. This invariant is encoded in the dispatcher; manifest writers don't have to add the method gate themselves.

2. **The first entry of `try` does NOT have to be `$path`.** It's just a template. `try: ["/404.html"]` with `status: 404` is a legal final rule. The dispatcher walks the chain in order and serves the first asset that exists.

## Validation (parse-time errors)

`Manifest::validate()` in `crates/core/src/types.rs` checks:

- `Action::Static.status` ∈ `[100, 599]` and `Action::Redirect.status` ∈ `[300, 399]`. Out-of-range = parse error.
- **Shadow detection.** A rule is shadowed when an earlier rule's `(effective_methods, path-coverage)` is a strict superset. `Manifest::passthrough()` and the standard SSR app shape (`POST /_rpc/`, `Glob /blog/[slug]`, `Any → Static`) all pass. Glob-as-shadower is deferred (TODO in code).
- **CORS sanity.** A rule's `cors` block (when present) MUST have non-empty `allow_origins`, and `allow_credentials: true` MUST NOT pair with `allow_origins: ["*"]` (the browser CORS spec forbids that combination).

Apps whose manifest fails validation get `Manifest::passthrough()` synthesized with a logged warning — they're never served from a broken manifest.

## CORS handling

CORS is a per-rule concern. `Manifest::Rule { cors: Option<Cors> }` lets apps opt into CORS on the rules that need it (typically the API rule) without leaking headers everywhere.

Two gateway-side hooks:

1. **Preflight short-circuit.** `OPTIONS` with an `Origin` header. Before normal dispatch, `router::handle_request` calls `CompiledManifest::cors_for(request_method, path)`, where `request_method` comes from `Access-Control-Request-Method` (the future request's method, not `OPTIONS` itself). If the matched rule has `cors`, the gateway answers `204 No Content` with `Access-Control-Allow-*` headers built from the policy and returns. If no CORS-bearing rule matches, OPTIONS falls through to normal dispatch (which usually 404s).

2. **Response-header injection.** `CompiledManifest::dispatch` returns `(Outcome, Option<Cors>)`. After `execute_outcome` builds the worker/static/redirect response, `inject_cors_response_headers` adds `Access-Control-Allow-Origin` (and `Vary: Origin` for non-wildcard origins), `Access-Control-Expose-Headers`, and `Access-Control-Allow-Credentials` when the request's `Origin` is in the rule's allow list. If it isn't, no CORS headers are added — the browser enforces the deny.

The `Outcome` enum stays lean (no per-variant cors field); the `(Outcome, Option<Cors>)` tuple keeps the policy reachable at the response stage regardless of which variant fired.

## Compiled dispatch (the hot path)

`CompiledManifest::compile(&Manifest)` builds:

- A `Vec<CompiledRule>` in declaration order.
- Per rule: pre-computed method bitset (1 byte), pre-segmented `Glob` patterns, pre-parsed templates (`$path`/`[name]` resolved into a `Vec<TemplateChunk>` so `String::replace` doesn't run per request).
- The `Match::Prefix` strings are normalized at compile time (trailing slash stripped).

Per-request dispatch:

```
for rule in compiled.rules:
    if (req_method_bit & rule.methods) == 0: continue
    if not rule.matcher.matches(path, &mut captures): continue
    return rule.action.evaluate(path, &captures)
```

No HashMap allocation on the hot path for non-glob matches. Captures are an inline-sized `SmallVec` (or empty slice) collected only for `Match::Glob`.

## Synthesized passthrough

Apps that haven't shipped a manifest get this default at registry-load time:

```jsonc
{
  "rules": [
    { "match": { "kind": "prefix", "method": "POST", "path": "/_rpc/" },
      "action": { "kind": "worker", "mode": "rpc" } },
    { "match": { "kind": "any" },
      "action": { "kind": "worker", "mode": "ssr" } }
  ]
}
```

Reproduces the pre-manifest behavior: POST `/_rpc/<method>` is RPC (gateway gates the API key); everything else goes to the worker as SSR.

## HTTP completeness on the static path

`serve_static_hit` (and its streaming sibling) honour the standard HTTP semantics that browsers, proxies, and CLIs assume:

- **`If-None-Match` → 304 Not Modified.** Short-circuited at the top of `serve_static_hit`, **before any blob fetch** — the whole point of the conditional GET is to skip the byte transfer. `etag_matches` accepts an exact strong ETag, the wildcard `*`, and a comma-separated list. Weak ETags (`W/"…"`) are NOT accepted (our hashes are content-addressed and always strong).
- **`Range: bytes=N-M`** on both buffered and streaming paths. Single ranges return `206 Partial Content` with `Content-Range: bytes <start>-<end>/<size>`. Open-end (`bytes=N-`) and suffix (`bytes=-N`) forms are supported. Multi-range syntax (`bytes=A-B,C-D`) degrades to a `200` with the full body — RFC 7233 allows ignoring `Range` entirely. Unsatisfiable ranges return `416 Range Not Satisfiable` with `Content-Range: bytes */<size>`.
- **`Accept-Ranges: bytes`** is advertised on every 200 / 206 / 304 static response, so clients know they can re-request a range.
- **`Cache-Control: stale-if-error=<n>`** (RFC 5861) is emitted alongside `stale-while-revalidate` when both `swr_window` is set and `stale_on_error: true`. The `background_refresh` flag in `CacheCtl` is gateway-internal — it does NOT translate into a Cache-Control directive, and the current blob_cache LRU does not yet honour it (TODO in `serve_static_hit`).

The streaming path's range support is wired through `chunk_stream_from_path_range`, which passes a starting offset to `compio::fs::File::read_at` — only the requested slice is read off disk, no overshoot.

## Content-encoding variants

The gateway negotiates pre-compressed asset variants emitted by the build pipeline (see `docs/reference/zsapp.md`'s "Asset variants" section). The `pick_variant` helper resolves the request's `Accept-Encoding` against the asset's `variants` map; the chosen variant's hash, size, and encoding token feed every downstream concern in `serve_static_hit` and `serve_static_streaming`:

- **Body fetch** uses the variant's hash. The mem / disk LRUs key off `chosen.hash`, so requests for the same encoding share the same cached bytes.
- **ETag** is `"<variant.hash>"`. A client that fetched the brotli body and re-requests it later with `If-None-Match: "<br_hash>"` short-circuits to 304 even though the identity hash differs.
- **`Content-Length` / `Content-Range` totals** reflect the variant's size. A `Range: bytes=0-9` against a 1 KB brotli body returns `Content-Range: bytes 0-9/1024`, not `0-9/<identity-size>`.
- **`Content-Encoding: <token>`** and **`Vary: Accept-Encoding`** are added when a non-identity variant fires. Without `Vary`, intermediaries would conflate compressed and identity responses for clients with different `Accept-Encoding`.
- **Streaming-threshold check** runs against the variant's size — a 5 MB JS bundle that brotlies down to 800 KB takes the buffered path, which is correct: the whole point of compression is making bodies cheap to ship.

`pick_variant`'s preference order is the request header's listed-encodings order — `Accept-Encoding: br, gzip` picks `br` over `gzip`. q-values are honoured for `q=0` rejections (RFC 7231 §5.3.4); other q-values are treated as accepted regardless of weight (the listed order already conveys preference for the common browser case). Unknown encoding tokens (`lz4`, `zstd`, …) and missing variants both fall through to identity — the gateway never serves bytes the client didn't ask for.

v1 limits: only `br` and `gzip` are supported. `Manifest::validate()` rejects unknown variant keys at parse time so a typo can't silently disable negotiation on the wire.

## Rate limiting

Two layers, both enforced at the gateway before any request reaches a worker.

| Layer | Source | Bucket key | Lives in |
| --- | --- | --- | --- |
| Global per-app | Gateway boot (`RateLimitRegistry::new(rate, burst)`) | `app_id` | `state.rate_limiters` |
| Per-rule | Manifest's `Action::Worker.rate_limit` | `(app_id, rule_idx, RateLimitPer)` | `state.per_rule_rate_limits` |

The two are conceptually distinct: the global limit is a platform-level DoS guard with a fixed lifetime; the per-rule limit is creator-defined business logic that changes with every deploy. They live in separate registries so the lifetimes don't tangle.

Order of checks in `execute_outcome`'s `Outcome::Worker` arm:

1. **Per-rule first.** Cheaper for the common "rule is already saturated" case — a 429 short-circuits before the global bucket lookup. Bucket id is derived from `RateLimitPer` (`Ip` → `connection_info().remote()`, `Session` → `__zs_session` cookie value with IP fallback, `App` → constant `"app"`).
2. **API-key check** (RPC mode only).
3. **Global per-app limit + concurrency guard** inside `handle_dispatch`.

429 responses from the per-rule layer carry `Retry-After: 1` so clients back off for at least one refill window. The bucket capacity comes from `rate_limit.rps` (preferred) or `rate_limit.rpm.div_ceil(60)` (fallback). With both `rps` and `rpm` absent, the check is a no-op pass-through.

`rule_idx` is the rule's position in `Manifest::rules` and is captured at compile time. Two worker rules with identical `rate_limit` shape but different positions get independent buckets — the bucket survives manifest re-orderings only as far as the rule's position is stable.

## Common things to look up

- "How does the gateway find an asset by path?" → `serve_static_hit` in `crates/gateway/src/router.rs` checks `state.blob_cache` (in-memory LRU keyed by hash); on miss it calls `state.blob_store.get_blob(hash)` and fills the cache. ETag = `hash`; Cache-Control composed from `CacheCtl` precedence (entry → rule → default).
- "How are rewrites handled?" → `walk_rules` in `crates/gateway/src/compiled.rs` re-enters with the new path up to `MAX_HOPS = 8` times. Hop budget exhausted (cycle) → `Outcome::NotFound`.
- "How does the gateway enforce per-rule rate limits?" → `compute_bucket_id` in `crates/gateway/src/router.rs` derives the bucket discriminator from `RateLimitPer`; `state.per_rule_rate_limits.check(app_id, rule_idx, per, &bucket_id, &rl)` (in `crates/gateway/src/enforce.rs`) takes a token from the matching bucket or returns 429 with `Retry-After: 1`. The check fires inside `execute_outcome`'s `Outcome::Worker` arm before any worker dispatch.
- "How does an app get a custom domain?" → Not currently supported. Adding it needs a verified `host → app_id` map at the gateway, sourced from a DNS TXT or ACME challenge.

## Where to start when changing routing behavior

| You're doing… | First read | Then edit |
| --- | --- | --- |
| Adding a new `Match` kind | `crates/core/src/types.rs` (`Match` enum, `Match::test`) | Add variant; extend `CompiledMatch` in `crates/gateway/src/compiled.rs`; extend shadow detection's `covers_path` |
| Adding a new `Action` kind | `crates/core/src/types.rs` (`Action`); `crates/gateway/src/dispatch.rs` (`Outcome`) | Add variants in both places; extend `CompiledAction`; extend `execute_outcome` in `router.rs` |
| Changing how assets are fetched | `crates/gateway/src/router.rs` (`serve_static_hit`, `fetch_static_bytes`); `crates/gateway/src/blob_cache.rs` | Phase A is in-memory LRU; Phase B layers mmap (see `docs/architecture/blob-store.md`) |
| Adding a manifest validator | `crates/core/src/types.rs` (`Manifest::validate`) | Append a check; add tests in `crates/core/tests/types_test.rs` |
