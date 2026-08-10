# Gateway review - 2026-08-09

Scope: `crates/gateway/` only (manifest dispatch, JWT/session validation, rate
limiting, CHWBL routing, asset proxy, OIDC RP, back-channel logout, sessions,
signing). Regenerated from the code, not from a prior report.

**Labels are `G<n>` and are stable.** They are deliberately NOT `F<n>`: `F`
labels are reused across reviews in this repo, so searching by an `F` label
silently crosses review boundaries. Cite findings from this review as
`gateway-review-2026-08-09 G3`, never bare `G3`.

Out of scope by operator instruction (active edit): `sdks/db`, `sdks/migrate`,
`sdks/bootstrap`, `crates/plugin-db`, `crates/zeroship-schema`,
`crates/migrated`, `crates/zeroship-migrate-adapter`, `db/`,
`docs/reference/db.md`. Where a gateway finding touches one of those the
dependency is stated and the trail stops there.

## How each finding was established

| Label | Established by |
| --- | --- |
| G1 | EXECUTED - red test at the real entry point (`resolve_auth`) |
| G2 | EXECUTED - red test (`PerRuleRateLimitRegistry::check`) |
| G3 | EXECUTED - red test (`inject_cors_response_headers`) |
| G4 | READ - full read of the type's `impl` block + all call sites of the field |
| G5 | READ - call-path walk from `handle_request` |
| G6 | READ - call-path walk from `execute_resource_tree` + doc cross-check |
| G7 | READ - call-path walk from `resolve_app_session_user_header_inner` |
| G8 | READ - symbol reachability (`#[cfg(test)]` gating) |
| G9 | READ - call-path walk from `sync_once`, compared against `--blob-store` |
| G10 | READ - call-path walk from `handle_request` / `handle_dispatch` |
| G11 | READ - four-leg walk across gateway, control and auth (each leg read) |
| G12 | READ - call-path walk from `session()` |
| G13 | READ - three-way diff against the two sibling verifiers |
| G14 | EXECUTED - red test (`sanitize_oidc_original_path`) |
| G15 | READ - full read of `CircuitBreaker` + all `call()` exits |
| G16 | READ - full read of `signal_ingress.rs` + its two mount sites |
| G17 | READ - full read of `Stash` / `encode` / `decode` / `finish_callback` |
| G18 | READ - call-path walk from `backchannel_logout::handle` |

Killed findings are listed at the end. A killed finding is a result.

Note on quoting: this document is ASCII-only, so punctuation inside quoted
source comments (em dashes, curly quotes, section signs) has been
transliterated. Line references are exact; go to the file for byte-exact text.

---

## G1 - the auth gate only recognises `Authorization: Bearer`, the rate-limit and affinity code also recognise `bearer`

**File / symbol:** `crates/gateway/src/router/auth.rs:670`
(`resolve_bearer_user_header`) vs `crates/gateway/src/router/dispatch.rs:833-835`
(`compute_bucket_id`) and `crates/gateway/src/router/dispatch.rs:929`
(`subscription_affinity_key`).

**What is wrong.** The auth gate strips exactly one spelling:

```rust
let Some(token) = auth_header.strip_prefix("Bearer ") else {
    return BearerOutcome::NotBearer;
};
```

RFC 7235 section 2.1 makes the auth-scheme token case-insensitive, and the gateway's
own bucket-derivation code agrees - it accepts both `Bearer ` and `bearer `.
The gate does not. So one request can be simultaneously "carries no Bearer
credential" (auth) and "carries a Bearer credential whose `sub` is X" (rate
limiting, CHWBL affinity).

Two consequences, in increasing severity:

1. **Correctness.** A non-browser client that sends the lowercase scheme (some
   HTTP stacks normalise it) is not authenticated at all. On a `user` route it
   gets a 401 that does not name the real problem; on an `anon` route it is
   served anonymously and the app sees no user - silently, with a valid token
   in hand.
2. **Caller-chosen rate-limit bucket.** `compute_bucket_id` guards the
   unverified-`sub` read behind `identity_verified`, and the doc comment at
   `dispatch.rs:801-811` states exactly why: *"trusting those bytes would let
   the caller pick their own bucket and mint a fresh allowance per request"*.
   But `identity_verified` is `user_header_from_gate.is_some()`
   (`dispatch.rs:1475`), and that can be satisfied by the **cookie** arm while
   the `Authorization` header is entirely attacker-chosen - because the gate
   never looked at it. The invariant the flag encodes ("if we are reading these
   bytes, something upstream verified them") does not hold for the lowercase
   spelling.

**Concrete failure scenario.** App declares a resource with
`rate_limit: { rps: 5, per: "user" }`. Attacker holds one legitimate logged-in
session.

```
GET /api/expensive HTTP/1.1
Host: myapp.zeroship.ai
Cookie: __Host-zeroship_app_session=<their own valid signed cookie>
Authorization: bearer eyJhbGciOiJub25lIn0.eyJzdWIiOiJyYW5kb20tMDAwMDEifQ.x
```

The cookie arm authenticates (GET is exempt from the CSRF gate,
`auth.rs:428`), so `identity_verified == true`. `compute_bucket_id` then
returns `sub:random-00001` from the *unsigned, never-verified* payload.
Incrementing the fake `sub` on each request yields a fresh 5-token bucket every
time: the `per: "user"` limit is unbounded, and every distinct value also
allocates a permanent `PerRuleKey` entry (see G4).

**What would have to be true for this to be wrong.** (a) The gate would have to
canonicalise the header before `resolve_bearer_user_header` sees it - it does
not; `req.headers().get(AUTHORIZATION)` returns the raw value and ntex does not
rewrite values. (b) `identity_verified` would have to be false whenever a
Bearer header is present - it is not; it is derived only from the resolved
`ZeroShip-User`, and the cookie arm sets it. (c) `NotBearer` would have to be
rejected rather than fall through - `resolve_auth_inner:348` explicitly falls
through to the cookie arm on `NotBearer`.

**Verification (EXECUTED, red).** Two temporary tests were added at the real
entry points and then removed. One-variable controls: identical to the existing
`resolve_auth_non_user_session_bearer_401s_even_on_anon` and
`bearer_non_jwt_token_is_not_user_session`, changing only the scheme case.

```
thread 'router::auth::tests::temp_g_verify_lowercase_bearer_reserved_scheme'
panicked at crates/gateway/src/router/auth.rs:2072:9:
reserved-scheme Bearer must 401 even on Anon regardless of scheme case,
got Allowed { user_header: None }

thread 'router::auth::tests::temp_g_verify_lowercase_bearer_on_user_route'
panicked at crates/gateway/src/router/auth.rs:2099:9:
lowercase bearer must reach the same reserved-scheme arm, got NotBearer
```

The capital-`Bearer` controls pass in the same run, so the instrument
discriminates: the only variable is the scheme case.

**Confidence:** high.

**Fix shape.** Match the scheme case-insensitively in
`resolve_bearer_user_header` (split on the first space, compare
`eq_ignore_ascii_case("bearer")`), so all three readers agree. Do not "fix" it
by making the bucket readers case-sensitive - that leaves consequence 1 intact.

---

## G2 - a tightened per-resource rate limit never takes effect on an already-created bucket

**File / symbol:** `crates/gateway/src/enforce.rs:280`
(`PerRuleRateLimitRegistry::get_or_create`), reached from
`crates/gateway/src/router/dispatch.rs:1477`.

**What is wrong.** The bucket's `(rate, burst)` are baked in at creation:

```rust
w.entry(key)
    .or_insert_with(|| Arc::new(TokenBucket::new(rate, burst)))
    .clone()
```

`or_insert_with` does not run when the key exists, so the `rate`/`burst`
arguments of every later `check()` are discarded. The key is
`(app_id, rule_idx, bucket)`; `rule_idx` is `resource_key_hash(resource key)`
(`dispatch.rs:1469`) - a hash of the *manifest resource key*, which is stable
across deploys. So redeploying with a stricter `rate_limit` leaves every
already-live bucket running at the old rate, for the process lifetime. There is
no invalidation on route update: `RouteCache::update` (`sync.rs:61`) touches
`set_degraded`/`clear_degraded` only, never the per-rule bucket map.

**Concrete failure scenario.** App ships `/api/*` with
`rate_limit: { rps: 100, per: "ip" }`. A scraper at `1.2.3.4` starts hammering
it. The creator ships a new deploy with `rps: 1` to stop the bleeding.
`1.2.3.4`'s bucket already exists with `refill_rate = 100_000` and
`capacity = 100_000`, so the scraper keeps its 100 rps. A *different* IP that
first appears after the deploy is correctly limited to 1 rps - which is exactly
what makes this hard to spot from the outside: the fix looks like it worked
when you test it from your own machine.

**What would have to be true for this to be wrong.** The per-rule map would have
to be rebuilt or invalidated on a route update. `buckets` is a private field of
`PerRuleRateLimitRegistry`; the only construction sites are `main.rs:715` and
three test-state builders, and the only mutation path is `check` ->
`get_or_create`. There is no `remove`, `clear`, `retain`, or `set_degraded`
equivalent on the type. Confirmed by reading the whole `impl` block
(`enforce.rs:249-323`), not by grep.

**Verification (EXECUTED, red).** Temporary test, since removed:

```
thread 'enforce::tests::temp_g_verify_tightened_rate_takes_effect_on_existing_bucket'
panicked at crates/gateway/src/enforce.rs:607:9:
the SECOND request in the same second must 429 under rps=1; if it passes,
the pre-existing bucket kept deploy 1's rate and the tightening had no effect
```

One-variable control in the same test: a bucket that did *not* exist under
deploy 1 (different client IP, same rule, same tightened config) does enforce
`rps=1`. That control passed, so the instrument discriminates between "the
registry is broken" and "my test never reached the check".

**Confidence:** high.

**Fix shape.** Either key the bucket on the rate parameters as well (so a
changed limit is a different key and the old bucket ages out), or store the
`(rate, capacity)` on the bucket and reset them when `check` is called with
different values. The first is simpler and composes with the eviction G4 needs
anyway.

---

## G3 - CORS injection overwrites `Vary`, dropping `Accept-Encoding` on pre-compressed static assets

**File / symbol:** `crates/gateway/src/router/cors.rs:83-86`
(`inject_cors_response_headers`), interacting with
`crates/gateway/src/router/static_serve.rs:385`.

**What is wrong.** `static_serve` sets `Vary: Accept-Encoding` when it serves a
negotiated pre-compressed variant. `inject_cors_response_headers` then does:

```rust
headers.insert(HeaderName::from_static("vary"), HeaderValue::from_static("Origin"));
```

`HeaderMap::insert` *replaces* all existing values for the name. Step 10 of
`execute_resource_tree` (`dispatch.rs:1697-1701`) runs this on **every** arm,
including `ResolvedAction::Static`. So a brotli-encoded asset served to an
allowed CORS origin goes out as `Content-Encoding: br` with `Vary: Origin` and
no `Accept-Encoding`.

**Concrete failure scenario.** App has a `*` or `/_assets/*` resource carrying
`cors: { allow_origins: ["https://app.example.com"] }` (the normal shape for a
site whose fonts/JS are fetched cross-origin). A CDN or corporate proxy sits in
front.

1. Client A: `GET /_assets/main.js` with `Origin: https://app.example.com` and
   `Accept-Encoding: br`. Gateway answers `200`, `Content-Encoding: br`,
   `Vary: Origin`.
2. The cache stores that entry keyed on `Origin` only.
3. Client B (same `Origin`, e.g. a `fetch` from the same SPA on an old client
   or a proxy that strips `Accept-Encoding`): sends no `Accept-Encoding`. The
   cache serves the brotli bytes. B cannot decode them - the response is
   garbage, not a graceful degradation.

The same clobber also loses any `Vary` the *worker* set on an SSR/RPC response,
so an app that varies on `Accept-Language` or a custom header has that stripped
whenever CORS matches.

**What would have to be true for this to be wrong.** `HeaderMap::insert` would
have to append rather than replace (it replaces - ntex mirrors the `http`
crate's semantics), or the static arm would have to be excluded from step 10
(it is not; the CORS injection at `dispatch.rs:1697` is outside the action
match and unconditional on `policy.cors.is_some()`).

**Verification (EXECUTED, red).** Temporary test, since removed, built on the
exact header set `static_serve.rs:385` emits:

```
thread 'router::cors::tests::temp_g_verify_vary_accept_encoding_survives_cors_injection'
panicked at crates/gateway/src/router/cors.rs:324:9:
Vary must still list Accept-Encoding after CORS injection, got "Origin"
```

The test's own precondition assertion (that the fixture response really carries
`Vary: Accept-Encoding` before injection) passed, so the failure is the
clobber, not a bad fixture.

**Confidence:** high.

**Related, lower severity, same function.** The *negative* CORS answer emits no
`Vary: Origin` at all (`cors.rs:87-90` returns early, and
`build_preflight_response` only sets `Vary` on the allowed branch). A shared
cache can therefore store a no-`Access-Control-Allow-Origin` response and serve
it to an allowed origin. Correct behaviour is `Vary: Origin` on every
origin-dependent response, allowed or not.

**Fix shape.** Append to `Vary` rather than insert: read the existing value,
add `Origin` if absent, write the joined list. Do the same in
`build_preflight_response`, and emit `Vary: Origin` on the disallowed branch
too.

---

## G4 - the per-rule rate-limit bucket map is unbounded and keyed on caller-controlled strings

**File / symbol:** `crates/gateway/src/enforce.rs:236`
(`PerRuleRateLimitRegistry.buckets`).

**What is wrong.** `buckets: RwLock<HashMap<PerRuleKey, Arc<TokenBucket>>>` has
no capacity cap, no TTL, and no eviction. `PerRuleKey.bucket` is the
discriminator string from `compute_bucket_id`, which for `per: "ip"` is the
client IP and for `per: "session"`/`per: "user"` can be a cookie value or a JWT
`sub`. Every distinct value allocates a `TokenBucket` plus a `String` that is
never freed while the process lives.

The gateway *does* bound its other unbounded-input cache - the revocation cache
has `REVOCATION_CACHE_MAX_ENTRIES`
(`crates/authz/src/wrapper_revocation.rs:154` neighbourhood) - so this is an
inconsistency inside the gateway's own design, not a missing convention.

**Concrete failure scenario.** Any app with a `per: "ip"` rule and an attacker
with an IPv6 /64 (a normal consumer allocation, 2^64 addresses): one request per
source address, ~120 bytes retained per entry. A sustained 10k rps for an hour
is 36M entries, several GB, and the gateway OOMs - taking down *every* app it
fronts, not just the targeted one. No IPv6 is needed if G1 is unfixed: vary the
lowercase-`bearer` `sub` instead and one client does it from one address.

**What would have to be true for this to be wrong.** Some other component would
have to prune the map. `buckets` is private; the type's whole `impl`
(`enforce.rs:249-323`) is `new` / `resolve_rate` / `get_or_create` / `check`  - 
no removal path exists, and the field is not exposed. The seven references to
`per_rule_rate_limits` across the crate are four constructions (one real,
three test states), one `.check(...)` call site, and two comments.

**Confidence:** high on the mechanism (established by full read, not grep);
medium on exploitability in a given deployment, since it depends on the app
declaring a per-rule limit at all.

**Fix shape.** Bound the map (LRU with a max-entries cap, mirroring
`REVOCATION_CACHE_MAX_ENTRIES`) and evict on overflow. Eviction hands the
evicted caller a fresh allowance, so pair the cap with the G2 fix rather than
treating them separately.

---

## G5 - CORS preflight is answered before every enforcement gate

**File / symbol:** `crates/gateway/src/router/dispatch.rs:1230-1241`
(`handle_request`).

**What is wrong.** The preflight short-circuit returns
`build_preflight_response(...)` directly from `handle_request`, *before*
`execute_resource_tree` runs. Every gate lives in `execute_resource_tree`:
account (`check_account`, step 1a), spend (`check_spend`, 1b), the global
per-app rate limit and concurrency ceiling (1c), and the per-resource rate limit
(step 6). None of them see an `OPTIONS` request that carries an `Origin` and
matches a CORS-bearing resource.

Preflights legitimately must not require *auth*. They are not exempt from rate
limiting, and they are certainly not exempt from the spend/account gates that
`execute_resource_tree` deliberately hoisted to bind "every action class" - the
hoist comment at `dispatch.rs:1314-1322` argues at length that `Degrade` must
cover static and redirect because that egress is billed; the preflight path
escapes the same argument entirely.

**Concrete failure scenario.** App declares `cors` on `*`. Attacker sends
`OPTIONS /anything` with `Origin: https://x` and
`Access-Control-Request-Method: POST` at line rate. Each request does a route
lookup, a canonicalisation, a resource match and a response build, and consumes
a connection slot, while consuming zero rate-limit tokens and zero concurrency
budget - including for an app whose creator is `Suspended` or whose spend state
is `Block`, both of which are supposed to 402 before anything is served.

**What would have to be true for this to be wrong.** A gate would have to exist
upstream of `handle_request`. `handle`/`handle_subdomain` do app-name
extraction and auth-host routing only; `main.rs` mounts the handlers directly
with no rate-limiting middleware in the chain (`main.rs:829-910`).

**Confidence:** high on the ordering (read the path); medium on impact  - 
`build_preflight_response` is cheap, so this is a resource-accounting and
billing-gate hole rather than a remote-crash primitive.

**Fix shape.** Move the preflight branch inside `execute_resource_tree`, after
the account/spend/rate/concurrency gates and before the auth gate.

---

## G6 - gateway-fronted WebSocket subscriptions return 501; only the comments say otherwise

**File / symbol:** `crates/gateway/src/router/dispatch.rs:2272`
(`handle_subscription_dispatch`).

**What is wrong (behaviour).** Every `kind: "subscription"` resource dispatched
through the multi-node gateway answers
`501 {"code":"UNIMPLEMENTED", ...}`. The handler does the CHWBL affinity
selection, acquires and immediately releases the worker slot, and returns the
stub. There is no WebSocket proxy.

**What is wrong (claims).** Two comments on the live path assert the opposite:

- `dispatch.rs:1568` - *"The transparent WS proxy itself is wired in
  `proxy::forward_subscription`."* No such symbol exists anywhere in
  `crates/`; the only two occurrences of `forward_subscription` and
  `proxy_subscription_upgrade` in the tree are these two comments.
- `dispatch.rs:1351-1352` - *"The actual handshake runs in
  `proxy_subscription_upgrade` once we get past the rest of the pre-dispatch
  checks."* Same non-existent symbol.

`docs/architecture/gateway-routing.md` also lists `Subscription` in the
`ProcedureKind` table with no note that the arm is unimplemented, and the
`426 UPGRADE_REQUIRED` answer for a non-upgrade GET (`dispatch.rs:1383`) tells a
client to "switch protocols" into a path that then 501s.

**Concrete failure scenario - the tier divergence.** This is a creator-path
blocker, and per the review's own rule a blocker on the creator path is itself a
finding.

- `pnpm dev` / `zeroship serve`: subscriptions work. The single-tenant runtime
  handles WS directly; the dev tier does not consult manifest policy at all
  (`docs/architecture/gateway-routing.md`, "Manifest auth is enforced only by
  the gateway").
- deployed: `GET /__zeroship/v1/chat.messages` with
  `Upgrade: websocket`, `Connection: Upgrade`, `Sec-WebSocket-Protocol: zs.v1`
  -> `501 UNIMPLEMENTED`.

So the *same* operation returns a live WebSocket locally and a 501 in
production. A creator who builds a chat/live-cursor/notifications app locally
has no workaround: they cannot route around the gateway, and the error text
tells them to use `zeroship serve`, which is not a deployment target.

**Verification status.** This is the one finding where I attempted the
run-in-both-tiers diff the brief calls for and did **not** complete it. I read
the deployed-side answer off the code path (`execute_resource_tree` ->
`ResolvedAction::WorkerRpc` with `kind == Subscription` ->
`handle_subscription_dispatch` -> 501), which is unambiguous - the function has
a single return. I did not stand up the full control+worker+gateway stack to
observe it over the wire, and I did not run the dev-tier half, so the
divergence is asserted from two code paths rather than from two observed
results. Marked accordingly; it is the highest-value item to walk end to end.

**Confidence:** high that the deployed answer is 501 and that the two comments
name symbols that do not exist. Medium on the dev-side half of the diff, which
was not executed.

**Fix shape.** Two separate things. (1) The comments must stop naming
functions that were never written - that is what makes this invisible to a
reading pass. (2) The gap itself needs either the WS proxy or an explicit,
documented "subscriptions are single-tenant only" line in
`docs/architecture/gateway-routing.md` and `docs/reference/websocket-design.md`
so a creator learns it before building on it, not after deploying.

---

## G7 - the cookie arm's doc comment claims an uncached revocation read; the code reads a 5-second cache

**File / symbol:** `crates/gateway/src/router/auth.rs:911-918`, doc comment on
`resolve_app_session_user_header_inner`.

**What is wrong.** Step 4 of the function's doc comment says:

> 4. Revocation gate - the SAME per-app family marker the Bearer arm use:
>    `is_family_revoked_since(client_id, pws_, iat)`. This is a direct
>    `SELECT EXISTS` (**NOT cached**) ... at the cost of one revocation DB
>    round-trip per request.

The body (`auth.rs:991-1007`) calls `family_revocation_decision`, which is a
read-through cache with a 5-second TTL
(`REVOCATION_CACHE_TTL_SECS = 5`, `crates/authz/src/wrapper_revocation.rs:154`)
and returns the cached answer with no DB round-trip on a hit. The gateway never
calls `is_family_revoked_since` at all - the only callers in the tree are
`crates/auth/src/oidc/{userinfo,introspect}.rs` and two test files.

This is the third comment on the same path to be wrong in the same direction.
Commit `fe8788eb8` ("stop claiming the revocation marker is read uncached")
fixed the *inline* comment at `auth.rs:983` and left the *doc* comment eleven
lines above it untouched - the structured, scannable part is the one that rots.

**Why it matters more than a typo.** This is a claim that reads as protection.
An auditor asking "how long can a revoked session keep working?" reads step 4,
sees "NOT cached ... per request", and stops. The real answer is up to 5 seconds
on a cache-warm node, and the correct inline comment (`auth.rs:986-988`) says so
 -  but only if you keep reading past the doc comment that already answered.

**Concrete failure scenario.** Support revokes a compromised session at
T. Requests on a node whose cache entry for `(client_id, pws_)` was populated at
T-4s continue to authenticate until T+1s. That window is a deliberate,
documented design choice in the inline comment; it is denied in the doc comment
that anyone reads first.

**What would have to be true for this to be wrong.** `family_revocation_decision`
would have to bypass the cache, or `is_family_revoked_since` would have to be
reachable from the gateway. Walked down from the entry point: `resolve_auth` ->
`resolve_auth_inner:351` -> `resolve_app_session_user_header_inner:929` ->
`family_revocation_decision:992` -> `state.revocation_cache.get(...)` at
`auth.rs:99`, which returns without touching the pool on a hit.

**Confidence:** high.

**A second site with the same wrong name.** I expected
`crates/gateway/src/lib.rs:136-147` (the doc comment on
`GateState::revocation_cache`) to be the correct counter-example. It is not: it
states the TTL correctly but also says *"A miss performs one
`is_family_revoked_since` DB read"*. The miss path calls
`wrapper_revocation::revoked_after_for` (`auth.rs:123`). So both gateway
comments name a function the gateway does not call, and neither is a reliable
control for the other.

**Fix shape.** Rewrite `auth.rs` step 4 to name `family_revocation_decision`
and state the 5 s TTL, and correct `lib.rs:138` to name `revoked_after_for`.

---

## G8 - ~600 lines of workflow step-result normalisation in the gateway are `#[cfg(test)]`-only forks of `plugin-workflow`

**File / symbol:** `crates/gateway/src/router/dispatch.rs:209-775`
(`workflow_worker_result_to_step_result`, `normalize_workflow_outcomes`,
`legacy_step_result_to_outcomes`, `normalize_workflow_step_result`,
`single_worker_result_to_outcome`, `parse_iso8601_duration_ms`, and ~11 more).

**What is wrong.** Each of these functions is behind `#[cfg(test)]`. They are
byte-for-byte siblings of the same-named functions in
`crates/plugin-workflow/src/advance.rs:167-713`, which is where the *production*
normalisation happens. The gateway's own production path,
`workflow_advance_internal` (`dispatch.rs:105`), calls only
`workflow_worker_advance_response` (`dispatch.rs:182`), which checks
`ack XOR nack`, a `runId`, and a non-empty `registrations` array - none of the
normalisation.

Ten `#[test]` functions drive the test-only copy - eight through
`workflow_worker_result_to_step_result` (`dispatch.rs:3226, 3255, 3285, 3307,
3334, 3379, 3431, 3483`) and two through `normalize_workflow_step_result`
(`dispatch.rs:3517, 3548`). They read as coverage of the gateway's
workflow-advance edge. They cover a fork that no request can reach, and they
cannot fail when the real implementation in `plugin-workflow` changes. The two
tests in the same module that *do* drive production  - 
`internal_workflow_advance_spend_blocked_app_returns_402` and
`public_vhost_workflow_advance_path_is_404` - are the whole real surface.

**Concrete failure scenario.** Someone changes the wake-at normalisation or the
legacy-`StepResult` shape in `crates/plugin-workflow/src/advance.rs`. The eight
gateway tests stay green, because they exercise the frozen copy. A reviewer
looking for "who covers the gateway's workflow edge" finds them and concludes it
is covered. The gateway's actual contribution - the `ack`/`nack` validation and
the missing signature check flagged in the `TODO(DW-signed-transport)` at
`dispatch.rs:149-151` - has one small test surface by comparison.

**What would have to be true for this to be wrong.** A non-test caller would
have to exist. `cargo` would refuse to compile a non-test reference to a
`#[cfg(test)]` item, so this is decidable from the attribute alone: the release
build of `zeroship-gateway` does not contain these functions.

**Confidence:** high.

**Fix shape.** Delete the fork and its eight tests from `dispatch.rs`; the
behaviour they describe belongs to `plugin-workflow` and is tested there. If the
gateway needs to assert something about the worker's advance ack, test
`workflow_worker_advance_response` directly.

---

## G9 - `--control` silently ignores the URL scheme: `https://` connects in cleartext on port 80

**File / symbol:** `crates/gateway/src/sync.rs:200-207` (`http_get_inner`),
called from `sync_once:177`.

**What is wrong.**

```rust
let parsed = url::Url::parse(url)...;
let host = parsed.host_str()...;
let port = parsed.port().unwrap_or(80);
...
let mut stream = TcpStream::connect(&addr).await...;
```

`Url::port()` returns `None` for a scheme's default port, so
`https://control.zeroship.ai/internal/routes` yields port **80**, and the
request is written as plaintext HTTP/1.1 on a raw `TcpStream` - including
`Authorization: Bearer {control_key}`. The scheme is never inspected. There is
no TLS anywhere in this path. `crates/gateway/src/proxy.rs:169,182` has the
same `unwrap_or(80)` for the worker hop.

Intra-cluster plaintext may well be the intended topology. The finding is that
an operator who writes `https://` gets neither TLS nor an error - the config is
accepted and silently downgraded. The gateway already demonstrates the right
pattern one screen away: `--blob-store` is parsed by `StoreUrl::parse` and the
process `exit(2)`s on an invalid value (`main.rs`, "gateway: invalid
--blob-store"). `control_url` gets no validation at all; `--check-config` just
echoes it back (`main.rs:524`).

**Concrete failure scenario.** Operator runs
`zeroship-gate --control https://control.internal.example --control-key $K`.
The gateway connects to `control.internal.example:80`. Either (a) nothing
listens and route sync fails every 5 s with a connect error while the gateway
keeps serving its last-known route table indefinitely - deleted apps still
answering, new deploys never landing, spend state frozen; or (b) something does
listen on :80, in which case `$K` goes out in cleartext on every poll and
whatever answers can inject an arbitrary `RouteMap` - which is the
authorization policy for every app on that gateway
(`RouteCache::update`'s own comment at `sync.rs:82-86` says the manifest *is*
the policy).

**What would have to be true for this to be wrong.** A TLS wrapper would have
to exist upstream of `http_get_inner`, or `control_url` would have to be
scheme-validated at boot. Neither: `sync_once` is the only caller, it passes
`state.config.control_url` verbatim, and the boot path stores it as a plain
`String` (`lib.rs:45`) with no parse.

**Confidence:** high on the mechanism; the severity depends on the deployment
topology, which is outside `crates/gateway/`.

**Fix shape.** Validate the scheme at boot the way `--blob-store` is validated:
reject `https://` with a message saying the control hop is plaintext-only, or
implement TLS. Silently downgrading is the one option that should not survive.

---

## G10 - the raw `Host` header is forwarded into the URL the worker's `fetch` sees, with no allow-list

**File / symbol:** `crates/gateway/src/router/dispatch.rs:2425-2430`
(`handle_dispatch` -> `forward_url`).

**What is wrong.** The URL handed to the worker is built from the client's
`Host` header verbatim:

```rust
let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("localhost");
let url = forward_url(scheme, host, tail, req.uri().query());
```

There is no configured base domain to check it against - no `base_domain`,
`app_domain`, or host allow-list exists anywhere in the crate. On the
path-based route (`/apps/{app_name}/{tail}*`, `main.rs:838`) the `Host` is
entirely free, because the app is chosen from the path. On the subdomain route
only the **first label** is constrained (it must name a route,
`extract_app_name:82-83`); everything after the first dot is unconstrained, so
`Host: myapp.attacker.example` resolves app `myapp` and forwards
`http://myapp.attacker.example/...`.

The same unvalidated `Host` is also the basis of the CSRF gate's expected origin
(`auth.rs:432-438`).

**Concrete failure scenario.** App builds an absolute link from
`new URL(request.url).origin` - the normal way to produce a verification or
password-reset URL, an OG `og:url`, or a canonical tag.

```
POST /apps/myapp/__zeroship/v1/account.requestReset HTTP/1.1
Host: myapp.attacker.example
```

The worker sees `request.url == "http://myapp.attacker.example/__zeroship/v1/..."`
and emails the victim a reset link pointing at the attacker's host. Nothing in
the gateway rejected the request; the app did nothing wrong.

**What would have to be true for this to be wrong.** A host check would have to
run upstream. `handle_subdomain` checks only `is_auth_host` (exact match against
two constants) before falling through to `extract_app_name`; `handle` does not
look at `Host` at all. No middleware is mounted in front of either
(`main.rs:829-910`).

Caveat I checked and could not rule in: whether a fronting proxy in the shipped
deployment normalises `Host`. `deploy/` is outside this crate and I did not
read it, so this may be mitigated in practice by the edge rather than by the
gateway. It is still a defence the gateway itself does not have.

**Confidence:** high that `Host` is unvalidated in `crates/gateway/`; medium on
end-to-end exploitability, for the reason above.

**A second consequence in the same class.** `browser_auth.rs:170-174`
(`is_registered_redirect_uri`) builds the expected origin as
`format!("{scheme}://{host}{path}")` from `route.host`, which is the raw `Host`
header (`auth_token.rs:89-94`). The comment above it (`browser_auth.rs:114-117`)
says the gateway mirrors the OP's exact-match allowlist *"so a gateway-layer
deviation can never widen the OP's allowlist if registration ever drifts"* - but
because the expected origin is derived from the same attacker-supplied header,
the check constrains only the **path**, never the origin. A request with
`Host: myapp.attacker.test` and a matching `redirect_uri` passes the gateway
mirror and the gateway emits a 302 to the OP carrying that foreign
`redirect_uri`. The OP's own registered-URI check is the only thing left - i.e.
exactly the drift the comment claims the mirror protects against. Not exploitable
today (the OP check is real), but the stated property does not hold.

**Fix shape.** Give the gateway a configured base domain and reject a `Host`
whose suffix does not match it (with an explicit dev bypass under
`insecure_dev`), or reconstruct the forwarded URL and the expected redirect
origin from the resolved route's canonical hostname rather than from the
request.

---

## G11 - a revoked session is permanently resurrected by anchor reload-recovery

**File / symbol:** `crates/gateway/src/auth_token.rs:1210-1226` (the F4
post-refresh re-check in the rotation path), reached from
`auth_token.rs:737-778` (`session`).

**What is wrong.** The rotation's revocation re-check is:

```rust
match revoked_after_for(&conn, client_id, pws).await {
    Ok(Some(revoked_after)) if revoked_after >= rotation_started_at => {
        return Err(RotationError::LoginRequired);
    }
    Ok(_) => {}
    ...
}
```

`rotation_started_at` is stamped when *this* rotation begins
(`auth_token.rs:1067`). So the gate rejects only a marker written **during** the
rotation - the TOCTOU window it was designed for. A marker written *before* the
request lands in `Ok(_) => {}` and the rotation proceeds. The rotation then
signs a fresh session cookie with `iat = now`, and the dispatch-side gate is
`family_revoked_at(revoked_after, iat) == revoked_after > iat`
(`crates/authz/src/wrapper_revocation.rs:108-110`), which is now permanently
false. `sweep_expired_families` (same file, line 115) then deletes the marker
after the retention window, so even the raw-OP-Bearer arm stops rejecting.

The fall-through that feeds this is deliberate: the `/session` fast path
detects the revoked family and *intentionally* drops into reload-recovery
(`auth_token.rs:746-747`, comment at `771-773`), on the stated belief that
recovery "will end in `login_required` for a revoked family". It does not.

**Concrete failure scenario.** All four legs read directly, not inferred:

1. User revokes an app's grant: `DELETE /me/oauth-grants/oac_abc`.
   `revoke_grant_cascade` (`crates/control/src/oauth_grants_handlers.rs:170-230`)
   deletes the `oauth_grants` row, revokes the relay alias, and writes
   `token_revocations(oac_abc, pws_X, revoked_after = T0)`. It does **not**
   touch `zeroship.app_session_anchors` - I read the whole function; the only
   writers of that table's `revoked_at` are
   `crates/auth/src/identity/password_reset.rs:344`, and the gateway's own
   `anchors::delete` / `delete_all_for_user` (`browser_auth.rs:444,463`,
   `backchannel_logout.rs:515`).
2. The victim's ~15-minute signed cookie is correctly rejected on dispatch.
3. The browser reloads at `T1 = T0 + 60s`. The SPA calls
   `GET /__zeroship/auth/session`. The fast path sees the revoked family and
   falls through to reload-recovery.
4. The 30-day anchor is still live (`revoked_at IS NULL`), so `read_live`
   returns it. The OP refresh succeeds: `crates/auth/src/oidc/refresh.rs:509-510`
   gates on `oauth_refresh_tokens.revoked_at` and `family_has_revoked_row`, and
   consults neither `oauth_grants` nor `token_revocations`.
5. The F4 re-check sees `T0 < T1` -> `Ok(_) => {}` -> pass. A fresh cookie with
   `iat = T1 > T0` is minted. The session is restored, with scopes, and stays
   restored.

Net: "disconnect this app" holds for at most one page load.

**What would have to be true for this to be wrong.** (a) The OP refresh would
have to consult the grant or the family marker - it consults neither
(`refresh.rs:509-510`, read). (b) Some writer would have to revoke the anchor on
grant revoke - none does (every `app_session_anchors` reference in `crates/`
enumerated above). (c) The dispatch gate would have to be `>=` rather than `>`
 -  it is `>` (`wrapper_revocation.rs:109`, read).

**Confidence:** high on the mechanism. The four legs span `crates/control` and
`crates/auth`, which are outside this review's scope; I read the specific
functions to close the chain but did not audit those crates, and I did not
observe the sequence against a live stack.

**Fix shape.** Compare the marker against the *anchor's* or the presented
credential's instant, not against `rotation_started_at` - a marker at any time
after the family was established must fail closed. Alternatively make
`revoke_grant_cascade` revoke the anchor, which is what `password_reset`
already does and is presumably why that path is not affected.

---

## G12 - the `/session` CSRF guard is keyed on `mint=1`, but the rotation it protects runs without `mint=1`

**File / symbol:** `crates/gateway/src/auth_token.rs:277-284`
(`session_csrf_guard`) and `:737-778` / `:794-965` (`session`).

**What is wrong.**

```rust
pub(crate) fn session_csrf_guard(req, host, insecure_dev, want_mint) -> ... {
    same_origin_guard(req, host, insecure_dev, want_mint, want_mint)
}
```

Both `require_custom_header` and `require_origin` are `want_mint`. Its doc says
*"A non-`mint` GET is a pure read, so neither the header nor `Origin` is
required there."* That is not what the handler does. When `want_mint == false`
and the signed cookie is absent, expired, tampered, non-`pws_`, or revoked,
control falls out of the `if !want_mint` block at line 778 and reaches the
**same** rotation path - OP refresh, `sessions::create`,
`update_rotated_family`, fresh cookie - at lines 794-965. The rotation is not
gated on `mint=1` at all; `mint=1` only *forces* it.

That fall-through is the normal steady state, not an edge: the cookie lives ~15
minutes and the anchor lives 30 days, so every reload after fifteen idle
minutes takes it.

**Concrete failure scenario.** `<img src="https://victim.zeroship.ai/__zeroship/auth/session">`
embedded on `evil.zeroship.ai`. A GET subresource sends no `Origin`, so check
(2) passes; no `X-ZS-Auth` is required because `want_mint` is false; the anchor
cookie is `SameSite=Strict` but `evil.zeroship.ai` -> `victim.zeroship.ai` is
same-**site**, so it is sent. The remaining defence is `Sec-Fetch-Site`, which
is enforced only *when present* (`auth_token.rs:248-260`) - so a UA that omits
it (Safari < 16.4) completes a forced family rotation, one OP round-trip and
one `gateway_sessions` row per hit. The attacker cannot read the response.

**Why it matters beyond the modest impact.** `session_csrf_tests`
(`auth_token.rs:1586-1663`) pins "mint requires Origin" as *the* fix. Dropping
four characters from the query string routes around the property the test
claims to protect, and the test cannot see it, because it tests the guard rather
than the handler.

**What would have to be true for this to be wrong.** The non-mint fall-through
would have to be unreachable. It is the documented reload-recovery path, and I
traced the `if let (Some(token), Some(verifier))` at 738-741 failing on a
missing cookie and on `verifier.verify` returning `Err`.

**Confidence:** high that the guard's stated property does not hold; medium on
browser exploitability (needs a sibling subdomain and a UA without
`Sec-Fetch-Site`).

**Fix shape.** Gate on "this request will rotate", not on `want_mint` - i.e.
run the guard after the fast path decides, or require Origin whenever the
handler is about to enter reload-recovery.

---

## G13 - `verify_access_jwt` force-refreshes the JWKS on ANY verification error, unauthenticated and unthrottled

**File / symbol:** `crates/gateway/src/oidc_rp.rs:740-749` (`verify_access_jwt`);
doc comment at `:676-678`.

**What is wrong.**

```rust
let raw = if let Ok(c) = try_verify(cache.keys().await...?) {
    c
} else {
    // Likely cause: JWKS rotated. Force-refresh once and retry.
    cache.refresh().await.map_err(OidcRpError::VerifyAccessToken)?;
    ...
};
```

`if let Ok(...) else` discards the error *kind*. A bad signature, an expired
`exp`, an `iss` mismatch, a missing claim - every one lands in the `else` arm
and calls `JwksCache::refresh()`, an unconditional outbound GET to
`{issuer}/.well-known/jwks.json` with no single-flight, no minimum interval, and
no circuit breaker in front of it.

The one-variable controls are in the same codebase and do it correctly:

- `crates/core/src/oidc_verify.rs:476-484` matches `Err(OidcError::NoMatchingKey(_))`
  and returns every other error unrefreshed.
- `crates/core/src/logout_token.rs:289-302` does the same **and documents why**:
  *"Signature/claim failures aren't fixable by refetching the JWKS, and forcing
  a refresh on them just doubles the latency on every bad token."*

And the doc comment on the broken one (`oidc_rp.rs:676-678`) claims it is
*"mirroring `zeroship_core::oidc_verify::verify_id_token`"*. It is the one
verifier of the three that does not.

**Concrete failure scenario.** The Bearer arm runs on **every** route including
`auth: anon`, and the only pre-check is the *unsigned* `iss` peek
(`router/auth.rs:675-680`). So an unauthenticated attacker sends, to any public
page of any app:

```
Authorization: Bearer <b64url {"alg":"EdDSA","typ":"at+jwt","kid":"zzz"}>
  . <b64url {"iss":"https://auth.zeroship.ai/oauth2","sub":"x","aud":"x",
             "exp":9999999999,"iat":1,"jti":"x","client_id":"x","scope":"openid"}>
  . AAAA
```

Every such request drives one JWKS fetch at the OP. The response to the
attacker is a normal 200 (anon route, `BearerOutcome::Invalid`), so nothing
signals abuse; the per-app rate limit is 1000 rps / 2000 burst by default
(`main.rs:714`) and the attacker can multiply by app subdomain. Each gateway
request also parks up to the JWKS fetch timeout inside a worker future while the
OP is saturated.

The benign form needs no attacker: any server-to-server client retrying with a
merely *expired* access token drives the same refresh, so a JWKS blip turns a
free 401 into a multi-second stall per request.

**What would have to be true for this to be wrong.** `refresh()` would need
coalescing or rate limiting (read end to end in
`crates/core/src/oidc_verify.rs` - it has none), or the Bearer arm would have to
reject unsigned garbage first (`router/auth.rs:655-694` - the only pre-check is
the unsigned `iss` peek), or `resolve_auth` would have to skip anon routes
(`router/auth.rs:329-334` serves anon *after* the verify ran).

**Confidence:** high.

**Fix shape.** Match on `Err(OidcError::NoMatchingKey(_))` exactly as the two
siblings do, and delete the "mirroring `verify_id_token`" claim or make it
true.

---

## G14 - a colon anywhere in the query string discards the post-login return path

**File / symbol:** `crates/gateway/src/router/dispatch.rs:2893-2915`
(`is_safe_oidc_original_path`), via `sanitize_oidc_original_path:2885`.

**What is wrong.** The "first segment" used for the scheme-lookalike check is
delimited only by `/`:

```rust
let first_segment_end = path[1..].find('/').map(|idx| idx + 1).unwrap_or(path.len());
!path[1..first_segment_end].contains(':')
```

Neither `?` nor `#` terminates it, so the colon scan runs across the query
string. `start_oidc_redirect` stashes `req.uri().path_and_query()`
(`dispatch.rs:2849-2854`), so the query is always present.

**Concrete failure scenario.** A user deep-links to
`https://app.zeroship.ai/search?q=https://example.com` and is not logged in.
`path[1..]` is `search?q=https://example.com`; the first `/` is the one inside
`https://`, so the "first segment" is `search?q=https:`, which contains `:`, so
the path is judged unsafe and rewritten to `/`. After logging in the user lands
on the app root with their destination gone. Same for `/agenda?t=12:30`,
`/page?ref=a:b` - any query carrying a URL, a time, or a namespaced value.

**Verification (EXECUTED, red).** Temporary test, since removed:

```
thread 'router::dispatch::tests::temp_g_verify_query_string_colon_survives_sanitise'
panicked at crates/gateway/src/router/dispatch.rs:3629:9:
assertion `left == right` failed: a colon inside the QUERY must not make the path unsafe
  left: "/"
 right: "/search?q=https://example.com"
```

The colon-free control (`/search?q=example`) asserted first and passed, so the
instrument ran and the only variable is the colon's presence in the query. The
existing tests at `dispatch.rs:4963-4979` all use colon-free queries, which is
why this is uncovered.

**Confidence:** high. Functional, not a security hole - the security intent
(rejecting `/javascript:alert(1)` as a scheme-lookalike) is fully served by
stopping the segment at the first `/`, `?`, or `#`.

---

## G15 - the OP circuit breaker cannot trip on a 5xx-ing OP, and any completed response re-closes it

**File / symbol:** `crates/gateway/src/op_client.rs:395-405` (`call`) and
`:223-230` (`CircuitBreaker::on_success`); module doc `:5-7`.

**Two defects, one type.**

**(a) Only transport errors and timeouts count as failures.** `Ok(Ok(resp)) =>
{ ... breaker.on_success(is_probe); ... }` - a completed HTTP response resets
`consecutive_failures` to 0 regardless of status. An OP whose own DB is down and
answers `500` in 4.9 s therefore *never* trips the breaker: every call waits
4.9 s, returns `Ok(resp)`, and resets the counter. The module doc names exactly
this case as the motivating risk (*"`/oauth2/token` slow **or 5xx-ing** while
every request opens a new connection pool"*). The reused client and the 5 s wall
are real fixes; the breaker contributes nothing to the 5xx half of that
sentence. Attacker-drivable too: the counter is *consecutive*, and
`/__zeroship/auth/callback` reaches `post_token` with attacker-chosen `code`
values that the OP rejects fast as `400 invalid_grant` -> `Ok(resp)` ->
`on_success` -> counter pinned at 0 through a genuine brownout.

**(b) `on_success` stores `Closed` unconditionally.** It checks neither the
current state nor `was_probe`. With N calls in flight against a browning-out OP,
calls 1-5 fail and open the breaker; call 6, admitted at the same instant and
still in flight, resolves with any completed response and slams the state back
to `Closed` with `failures = 0`. The cooldown never runs and the half-open probe
discipline is skipped. Every test in `op_client.rs:437-625` drives
`on_failure`/`on_success` sequentially on one thread, so this interleaving is
never exercised.

**What would have to be true for this to be wrong.** A status check between
`call()` and `on_success` - there is none; each caller (`oidc_rp.rs:275-284`,
`500-505`, `526-535`) inspects the status *after* the breaker was told
"success". Or an outer serialisation preventing concurrent OP calls - the
breaker is explicitly `Arc`-shared across ntex worker threads
(`oidc_rp.rs:72-80`).

**Confidence:** high on both mechanisms; medium on operational impact (a *fast*
5xx does not exhaust connections; a slow one does).

---

## G16 - the public signal-ingress endpoint has an unbounded limiter map, the wrong client key, and a fresh HTTP client per request

**File / symbol:** `crates/gateway/src/signal_ingress.rs:48-65` and `:103`.
Mounted at `POST /__zeroship/v1/signal` and `POST /__zeroship/signals/v1`
(`main.rs:851-864`), outside the per-app dispatch path, so
`enforce::check_rate_limit` never applies.

Three defects in one small unauthenticated handler:

**(a) Unbounded map.** `PLACEHOLDER_LIMITER` is a
`HashMap<String, PlaceholderBucket>` with no cap and no eviction  - 
`check_placeholder_rate_limit` only does `entry(...).or_insert_with(...)`. The
bucket never rejects a *first* request from a new key, so the limiter's shape
is exactly wrong: it grows a permanent entry per source while admitting the
request. Same class as G4, and the gateway again has the right pattern nearby  - 
`backchannel_logout.rs:31,411-415` caps and evicts.

**(b) Wrong client key.** `source_key` uses `req.peer_addr()`. The crate already
has the correct helper - `client_ip(req, trust_proxy)`
(`dispatch.rs:862`) - used by every other limiter. Behind any L7 load balancer
every request presents the LB's address, so the whole endpoint collapses to one
5 rps bucket and a single client can deny workflow signal ingress for every app
on the platform.

**(c) `cyper::Client::new()` per request** (`signal_ingress.rs:103`). This is the
exact anti-pattern `op_client.rs:9-18` exists to eliminate ("NO
`cyper::Client::new()` per call anywhere on the auth path") - here on an
unauthenticated public path, building a fresh connection pool toward control on
every accepted POST.

**What would have to be true for this to be wrong.** A cap or sweep elsewhere.
The module is small and was read in full; the only bound anywhere near it is the
`PayloadConfig` body cap at the mount sites.

**Confidence:** high for (a) and (c); high on mechanism and
deployment-dependent on impact for (b).

---

## G17 - the OIDC stash's advertised 10-minute window is never enforced

**File / symbol:** `crates/gateway/src/oidc_rp.rs:1032-1034`
(`STASH_MAX_AGE_SECS`), `:899-943` (`Stash`, `encode`, `decode`).

**What is wrong.** The constant is documented as *"10-minute window for the OIDC
dance to complete. After this the user has to re-initiate."* `Stash` has exactly
six fields - `state`, `client_id`, `verifier`, `nonce`, `original_path`,
`redirect_uri` - and **no timestamp**. `Stash::decode` verifies the MAC and
deserialises; it performs no freshness check, and neither does
`finish_callback`. The constant's only use is the cookie's `Max-Age` attribute
(`oidc_rp.rs:1048`) - a browser-side hint that a non-browser client ignores and
that does not travel with a captured copy of the blob.

Consequence: a captured stash is valid for the life of the stash signing key,
not ten minutes. Compounding it, `/__zeroship/auth/callback` is intercepted in
`handle_subdomain` at `dispatch.rs:987-988`, *before* `handle_request` and
therefore before the rate limit, the concurrency guard, and the spend gates - so
a replayed stash plus an arbitrary `code` is an unauthenticated,
unrate-limited, gateway->OP `POST /token` amplifier, which also (per G15a) pins
the breaker's failure counter at zero.

**What would have to be true for this to be wrong.** A timestamp inside the
signed blob or an age gate at the handler. `Stash`, `encode`, `decode`,
`finish_callback` and `handle_auth_callback` were read in full; neither exists.

**Confidence:** high on the mechanism (the comment states a property the code
does not implement); low-medium on standalone impact, since the OP's
authorization code is the genuinely short-lived secret.

---

## G18 - back-channel logout's `sub` fallback fires on "sid matched nothing", not "no sid", and logs the user out everywhere

**File / symbol:** `crates/gateway/src/backchannel_logout.rs:209-247`; doc
comment at `:47-50`.

**What is wrong.** The doc says: *"prefer `sid` and revoke only local sessions
that originated from that OP session. If the logout token lacks `sid`, fall back
to revoking all sessions for the token's `sub` at that app."*

The code takes the `sid` branch at line 188, and when `users.is_empty()` at line
209 - meaning `sid` **was** present and matched zero rows - it calls
`revoke_by_sub`, which runs `sessions::revoke_app_sessions_for_user` (all
sessions for that user at that app), writes the per-app family revocation
marker, and calls `anchors::delete_all_for_user` (line 515).

Since the platform OP always sends both `sub` and `sid`
(`crates/auth/src/oidc/backchannel_logout.rs:158-163`), the "lacks sid" branch
the doc describes is dead, and this fallback is the only one that ever fires.

**Concrete failure scenario.** A user is signed in on a laptop (OP session S1)
and a phone (S2) at the same app and signs out on the phone. The OP posts a
logout token with `sid = S2`. If no `gateway_sessions` row carries `sid = S2`  - 
which is a real state, because reload-recovery has to *reconstruct* the `sid`
from history via `sessions::latest_sid_for_user` (`sessions.rs:255-287`)
precisely because refresh grants do not always return an ID token - then
`users` is empty and the gateway revokes every session for that user at that
app, deletes every anchor, and writes the family marker that kills every live
signed cookie and raw-OP access token for that `(client_id, pws_)`. The laptop
is silently logged out by a single-device signout.

The `sid` branch's own SQL is correct (`sessions.rs:293-352` binds `sid` to
`sub`); the defect is purely the fallback's trigger condition.

**Confidence:** high that the doc misdescribes the code; medium on how often the
empty case occurs, since I did not trace every session-mint vector to
exhaustion.

**Related, same file:** `retryable_processing_error()` (`:392-396`) returns 503
without burning the `jti`, on the assumption that the sender retries. It does
not: `crates/auth/src/oidc/backchannel_logout.rs:174-184` posts once per RP and,
on a non-2xx, logs a warning and moves on - no queue, no backoff, no outbox. So
a momentary pool exhaustion on the gateway silently drops a real logout, and the
user's sessions live out their full lifetime. Every 503 arm in the handler has
this property.

---

## Findings I killed

Recording these because a killed finding is a result, and the next reviewer
should not re-derive them.

**K1 - `$path` static try-chain looked like a path-traversal primitive.**
`serve_resource_tree_static` (`static_serve.rs:54`) substitutes the raw request
path into the try-chain. Killed: the substituted value is
`dispatch_path`, which `handle_request` already canonicalised (and any
dot-segment or empty-interior-segment form was rejected with a 400 at
`dispatch.rs:1210-1217`); and the lookup is a `HashMap::get` against the
manifest's asset table (`compiled.rs:387`), not a filesystem open. There is no
path to escape from.

**K2 - the CSRF gate's expected origin is derived from a client-controlled
`Host`.** Looked like a same-origin bypass. Killed as a *CSRF* finding: a
browser sets `Origin` from the initiating page and `Host` from the target URL,
so an attacker cannot make them agree without already controlling a page served
from that host - and the app's `__Host-`-prefixed session cookie is not sent to
a different host anyway. The underlying unvalidated `Host` is real and is
reported as G10; the CSRF consequence specifically is not.

**K3 - anonymous callers can spray the CHWBL ring via `subscription_affinity_key`.**
The unverified JWT `sub` chooses the worker (`dispatch.rs:928-932`). Partially
killed: the security consequence the doc comment addresses (reading other
users' subscription state) really is handled elsewhere, and with G6 in place the
subscription path returns 501 before any traffic is pumped, so there is no
worker to hot-spot today. Worth revisiting *if* G6 is fixed - at that point a
caller pinning N connections to one worker becomes a live availability question.

**K3b - `alg` confusion in `verify_id_token`.**
`crates/core/src/oidc_verify.rs:458-466` takes `header.alg` from the untrusted
token and builds `Validation::new(alg)` - the textbook shape. Killed: the key
lookup at `461-464` requires `k.kid == kid && k.alg == alg`, and `CachedKey.alg`
is populated only from the JWKS through the whitelist at `refresh()` (296-307),
which admits RS256/384/512, ES256/384 and EdDSA and no symmetric family. An
HS256 token finds no key; `jsonwebtoken::Algorithm` has no `none` variant.
(`verify_access_jwt` and `logout_token::verify` additionally hard-pin EdDSA;
`verify_id_token` is the only one relying solely on the key-alg binding.)

**K3c - missing `azp` validation.** No code reads `azp`, and
`jsonwebtoken`'s `aud` check is *membership*, so a multi-audience ID token would
pass `finish_callback`'s single-`aud` expectation. Killed as exploitable: this
OP's `IdTokenClaims.aud` is a plain `String`
(`crates/auth/src/oidc/issuer.rs:51`), so multi-audience tokens cannot be
issued. Retained as a defence-in-depth note - `backchannel_logout.rs:79-88`
picks the *first* provisioned client out of an `aud` array and verifies against
it, so both would break together if the OP ever gained multi-aud tokens.

**K3d - BCL `jti` check-then-act race.** `logout_jti_cache.contains`
(`backchannel_logout.rs:116`) and `LogoutJtiClaim::claim` (161) are separated by
two awaits, so two concurrent replays can both pass the `contains`. Killed: the
in-flight claim CAS at `404-421` closes it - the loser gets
200-already-processing.

**K3e - half-open probe slot leaking permanently.** Every exit of `call()`
(`op_client.rs:394-423`) and `ProbeGuard::drop` (297-305) clears
`probe_in_flight`; `on_success(false)` not clearing it is harmless because the
probe's own resolver always runs. No wedge. (The *unconditional* `Closed` store
in the same function is a separate real defect - G15b.)

**K3f - open redirect via the post-login `Location`.** `//evil.com`,
`/\evil.com`, `https://evil.com` and `/foo:bar` are all rejected by the
`bytes[1]` alnum/underscore gate (`dispatch.rs:2903-2908`). `/a/..//evil.com`
passes the gate but resolves per RFC 3986 against the base URL's authority, so
it stays on the app's own host. CRLF cannot reach the field either - inbound it
is `req.uri().path_and_query()` (parser-validated), outbound it comes from the
gateway's own MAC'd stash.

**K4 - `RouteEntry.name` is a hostname, so subdomain dispatch is broken.**
`sync.rs`'s test fixtures spell `name` as `"provisioned.zeroship.ai"` while
`extract_app_name` yields only the first label. Killed as a *bug*: control
populates `name` from `zeroship.apps.name`
(`crates/control/src/registry.rs:642`), which is the bare app slug, so
production agrees. It survives as a note: those fixtures do not match the
production shape, so they cannot catch a first-label-vs-full-host regression.

---

## Not covered by this pass

Stated so the gap is visible rather than implied:

- `blob_cache.rs`, `proxy.rs`, and the byte-range / conditional-request paths in
  `static_serve.rs` were not audited; commit `0f082ed1c` touched Range clamping
  recently and that area deserves its own pass.
- `rls.rs`, `db.rs`, and `identities.rs` were read only far enough to follow the
  paths above.
- The end-to-end `pnpm dev` vs deployed diff was attempted for G6 only and not
  completed (see that finding). **No finding in this document was verified by
  running the same operation against both tiers and diffing the results** - the
  five EXECUTED findings are in-process red tests at real entry points, not
  cross-tier walks. G6 is the one that most needs that walk.

## Lower-priority items noted while walking the above

Recorded so they are not re-found, but not written up as full findings:

- `crates/gateway/src/sessions.rs:157` - `validate` has zero production callers
  (the only two references in the crate are doc comments saying it is no longer
  used), yet the module doc at `:5-12` still lists it under "Lifecycle" and
  advertises the "30 min sliding idle / 12 h absolute" limits it enforces. Those
  limits therefore constrain nothing.
- `crates/gateway/src/sessions.rs:255-287` - `latest_sid_for_user` orders by
  `issued_at DESC LIMIT 1` with no `revoked_at IS NULL` and no expiry filter, so
  a `?mint=1` rotation can copy forward a `sid` that a back-channel logout
  already revoked.
- `crates/gateway/src/anchors.rs:300-307` - `update_rotated_family` is the only
  anchor operation without an explicit `AND app_id = $2`; `read_live` (`:259`)
  and `delete` (`:328`) both carry one in addition to RLS.
- `crates/gateway/src/session_token.rs:174-177` - `Issuer::issue` rejects an
  empty `sub` but not an empty `app`, and `Verifier::verify` (`:319`) compares
  `app` with a plain `!=`. Two routes that both resolved
  `oauth_client_id = Some("")` would cross-accept each other's cookies. I did
  not find a path that stores an empty `oauth_client_id`.
- `crates/gateway/src/anchors.rs:483-485` - the single-flight rationale cites "the
  short cached wrapper", which the same file's header (`:19-24`) declares
  removed. The conclusion still holds (the OP's `SELECT ... FOR UPDATE` plus its
  30 s idempotent-replay window absorbs the concurrency), but the stated reason
  does not, and it is what a future reader would trust when widening the
  single-flight's scope.
- `crates/gateway/src/anchors.rs:138-142` - `breadcrumb_cookie_name` interpolates
  the unvalidated `Host` into a `Set-Cookie` *name*
  (`format!("zs.{host}.is.authenticated")`), so a crafted `Host` can inject
  cookie attributes such as `Domain=`. CR/LF is blocked by the HTTP parser, so
  no header splitting; self-inflicted unless a shared cache sits in front. Same
  root cause as G10.
</content>
</invoke>
