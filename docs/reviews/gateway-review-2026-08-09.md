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

## TRIAGE 2026-08-10

Every finding G1-G18 was re-established against HEAD. Verdicts below; each
finding also carries a `**Status (triage 2026-08-10)**` paragraph in its own
section with the evidence. To list the per-finding state:
`grep -n 'Status (triage' docs/reviews/gateway-review-2026-08-09.md` returns
exactly 18 lines, one per finding.

**Gateway suite at triage time: 472 passed, 0 failed, 1 ignored** (sum of all
11 `test result:` lines from `cargo test -p zeroship-gateway`). The review-time
figure of 356 is stale and should not be quoted. The baseline before this
triage added two tests was 470/0/1.

**Line numbers throughout this document have drifted** (typically +9 in
`dispatch.rs`, +21 in `auth_token.rs`) because of the post-review commits below.
Every triage note re-cites the current line. Re-locate by symbol name, not by
line.

| Label | Verdict | One-line basis |
| --- | --- | --- |
| G1 | **PARTLY SHIPPED / PARTLY LIVE** | `ddb9711b8` closed consequence 2 (caller-chosen bucket) by narrowing the bucket readers. Consequence 1 (a lowercase `bearer` credential is not authenticated at all) is untouched and was re-proved red. |
| G2 | **LIVE** | Re-proved red by execution, with a passing one-variable control. |
| G3 | **FIXED IN THIS TRIAGE** | Red, fixed, mutation-checked. `append_vary_origin` in `router/cors.rs`. |
| G4 | **LIVE** | Whole `impl` re-enumerated: `new`/`resolve_rate`/`get_or_create`/`check`, no removal path. |
| G5 | **LIVE** | Preflight returns at `dispatch.rs:1247`; every gate is inside `execute_resource_tree`, called at `dispatch.rs:1260`. |
| G6 | **LIVE (all three sub-claims)** | 501 confirmed single-exit; both named symbols have no definition anywhere; docs still silent. See the note on task #179 - it does **not** close G6. |
| G7 | **LIVE, and it has replicated** | Doc comment unchanged; a fourth wrong site was *added* post-review by `4e4dafce4`. One supporting claim in the finding is **wrong** - see below. |
| G8 | **LIVE** | 17 functions / 566 lines, all `#[cfg(test)]`; duplication confirmed by actual `diff`, not assertion. Two corrections to the finding's counts. |
| G9 | **SHIPPED** | `fd54716ec`. Verified on the real binary, not only in a unit test. |
| G10 | **LIVE (both consequences)** | `grep` for any host allow-list in `crates/gateway/` returns nothing. |
| G11 | **LIVE (all four legs)** | Each leg read at HEAD and confirmed. Not observed against a live stack. |
| G12 | **LIVE** | Guard is `want_mint` on both axes; the non-mint fall-through reaches `rotate_family` / `sessions::create` / `sign_session_cookie`. |
| G13 | **LIVE (all three parts)** | `if let Ok(..) else` intact; both siblings match `NoMatchingKey` only; `refresh()` has no throttle. |
| G14 | **FIXED IN THIS TRIAGE** | Red, fixed, mutation-checked. `is_safe_oidc_original_path` in `router/dispatch.rs`. |
| G15 | **LIVE (a and b)** | Both mechanisms intact. The module-doc claim is **split**, not uniformly wrong - correction below. |
| G16 | **LIVE (all three sub-defects)** | File has exactly one commit; nothing fixed. |
| G17 | **LIVE (both parts)** | `Stash` still has six fields and no timestamp. |
| G18 | **doc half LIVE, behaviour half LIVE-BY-DECISION** | `d49abc963` fixed the *inline* comment and left the *rustdoc* saying the original wrong thing. |

**Post-review commits that bear on these findings** (`3d699c54a..HEAD`):
`ddb9711b8` (G1, partial), `fd54716ec` (G9, complete), `d49abc963` (G18,
inline comment only), `4e4dafce4` (made G7 worse, not better).

### Two claims in this review that the triage found WRONG

Recorded here because a confidently-written review is not evidence, and both
were quoted as supporting reasoning rather than as the finding itself.

1. **G7's "The gateway never calls `is_family_revoked_since` at all - the only
   callers in the tree are `crates/auth/src/oidc/{userinfo,introspect}.rs` and
   two test files."** False, and it was false when written. `crates/gateway/src/
   auth_token.rs:1319` calls it, in production, inside
   `session_cookie_family_revoked` on the `GET /__zeroship/auth/session` fast
   path - and *that* call really is an uncached `SELECT EXISTS`. The gateway has
   **two** revocation readers with different caching. G7's actual defect is
   unaffected and arguably sharper: the doc comment on the *cookie arm* attaches
   the uncached reader's name to the cached one, so the name is not merely stale,
   it belongs to a real function ten screens away that behaves as advertised.

2. **G15's reading of the module doc.** The finding says the module doc "names
   exactly this case as the motivating risk", which is true of `op_client.rs:6`
   ("slow or 5xx-ing"). But `op_client.rs:28-33` of the same doc states the
   limitation correctly and deliberately: *"A OP 4xx (e.g. `invalid_grant`) is a
   VALID upstream response, NOT a breaker failure - only transport errors and
   timeouts count."* The doc is internally inconsistent, not uniformly wrong.
   The behaviour defects (a) and (b) are unaffected.

### What this triage could NOT establish

- **No finding was verified by running the same operation against both tiers
  and diffing the results.** That gap, which the review itself flags for G6, is
  unchanged. G6's deployed-side 501 is established from a single-exit function;
  the dev-side half is still asserted from code, not observed. Standing up
  control+worker+gateway+`pnpm dev` was out of scope for a triage pass.
- **G11 was not observed against a live stack.** All four legs are read-confirmed
  at HEAD, spanning `crates/gateway`, `crates/control` and `crates/auth`. The
  verdict is on the mechanism.
- **G16(b)'s and G4's operational impact remain deployment-dependent** (whether
  an L7 proxy fronts the gateway; whether apps declare per-rule limits). The
  mechanisms are confirmed; the exploitability is not measured.

---

## G1 - the auth gate only recognises `Authorization: Bearer`, the rate-limit and affinity code also recognise `bearer`

**Status (triage 2026-08-10): PARTLY SHIPPED, PARTLY LIVE.**

Consequence 2 (caller-chosen rate-limit bucket) is **SHIPPED** by `ddb9711b8`,
which took the option this finding explicitly warned against - it narrowed the
*bucket readers* rather than widening the gate. Both sites now read
`auth.strip_prefix("Bearer ")` only (`dispatch.rs:839` in `compute_bucket_id`,
`dispatch.rs:938` in `subscription_affinity_key`), each carrying the invariant
in a comment ("Keep this set <= the gate's"). That closes the hole; the commit
message says so and records the reasoning:

> LEFT FOR THE OPERATOR, deliberately not decided here: RFC 7235 makes the auth
> SCHEME case-insensitive, so the strictly spec-correct fix is arguably to widen
> `router/auth.rs` instead. I did not, because widening the gate makes it ACCEPT
> credentials it currently ignores - a behaviour change on the authentication
> path, not a hardening.

That commit also found something this review understated: `subscription_affinity_key`
takes no `identity_verified` parameter at all, so it read the header
unconditionally - an entirely unauthenticated caller could steer their own CHWBL
worker.

Consequence 1 is **LIVE**. `crates/gateway/src/router/auth.rs:670` is unchanged:

```rust
let Some(token) = auth_header.strip_prefix("Bearer ") else {
    return BearerOutcome::NotBearer;
};
```

Re-proved by execution (temporary test at `resolve_bearer_user_header`, since
removed; one-variable control is the existing
`bearer_non_jwt_token_is_not_user_session`, changing only the scheme case):

```
thread 'router::auth::tests::temp_triage_g1_lowercase_bearer_reaches_the_same_arm'
panicked at crates/gateway/src/router/auth.rs:2062:9:
RFC 7235 makes the auth scheme case-insensitive: lowercase `bearer` must reach
the same reserved-scheme arm as `Bearer`, got NotBearer
```

Not fixed here: it is the contract call the fixing commit deliberately left
open, and widening an authentication gate is not a small change.

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

**Status (triage 2026-08-10): LIVE.** `get_or_create` is unchanged at
`crates/gateway/src/enforce.rs:279-292`; the `or_insert_with` still discards the
`rate`/`burst` of every later `check()`.

Re-proved by execution (temporary test, since removed):

```
thread 'enforce::tests::temp_triage_g2_tightened_rate_takes_effect_on_existing_bucket'
panicked at crates/gateway/src/enforce.rs:633:9:
the SECOND request in the same second must 429 under rps=1; if it passes, the
pre-existing bucket kept deploy 1's rate and the tightening had no effect
```

The one-variable control in the same test - a client that did **not** exist
under deploy 1, same registry, same `rule_idx`, same tightened config, only the
bucket id differing - asserted *before* the case and passed, so the instrument
reached the check.

Not fixed here: both fix shapes the finding proposes are entangled with G4.
Keying on `(rate, burst)` makes the stale bucket immortal, which worsens the
unbounded map; storing and resetting the parameters on the bucket touches the
lock-free `TokenBucket` CAS loop. That is a design decision, not a small fix.

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

**Status (triage 2026-08-10): was LIVE, FIXED IN THIS TRIAGE (main defect).**

The clobber was confirmed still live and then fixed. `inject_cors_response_headers`
now calls a new `append_vary_origin(headers)` (`crates/gateway/src/router/cors.rs`),
which reads the existing `Vary`, splits it, adds `Origin` only if absent, and
writes back one joined header. `Vary: *` is left alone (narrowing it to a list
would weaken the response's cacheability contract).

Red before the fix, with the fixture precondition asserting first and passing:

```
thread 'router::cors::tests::cors_injection_appends_to_vary_instead_of_clobbering_it'
panicked at crates/gateway/src/router/cors.rs:349:9:
Vary must still list Accept-Encoding after CORS injection, got "Origin"
```

Mutation check (the test is load-bearing, not merely passing): removing only the
dedup early-return from `append_vary_origin` - a NARROW mutation, not a delete -
turns the test red on a different assertion:

```
assertion `left == right` failed
  left: Some("Origin, Origin")
 right: Some("Origin")
```

**What the new test does NOT cover, stated so the next reader does not
overestimate it:** it calls `inject_cors_response_headers` directly, so it cannot
catch the static arm being removed from step 10 of `execute_resource_tree`; and
it asserts nothing about `build_preflight_response`.

**The "Related, lower severity" item is still LIVE and was deliberately not
fixed**: the negative CORS answer still emits no `Vary: Origin`
(`cors.rs`, the `else { return; }` arm) and `build_preflight_response` still sets
`Vary` only on the allowed branch. Emitting `Vary: Origin` on a *disallowed*
origin is a cache-behaviour change on a response that currently carries no CORS
headers at all - a contract decision about what shared caches should key on,
not a mechanical fix. Left for the operator.

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

**Status (triage 2026-08-10): LIVE.** The whole `impl PerRuleRateLimitRegistry`
(`crates/gateway/src/enforce.rs:249-323`) was re-enumerated: exactly `new`,
`resolve_rate`, `get_or_create`, `check`. No `remove`, `retain`, `clear`, TTL or
cap. The only `remove` calls in the file (`enforce.rs:168`, `:371`) are
`w.remove(app_id)` on the *degraded* `HashSet<Uuid>` of two different registries,
bounded by app count. The field is private with no accessor; its references
outside `enforce.rs` are one real construction (`main.rs:723`), the field decl
(`lib.rs:88`), three test-state constructions, one call site
(`dispatch.rs:1486`), and two comments.

The contrast the finding draws is real, with a corrected line:
`crates/authz/src/wrapper_revocation.rs:159` (not 154) is
`pub const REVOCATION_CACHE_MAX_ENTRIES: usize = 100_000;`, wired through
`with_ttl_and_capacity` at `:191`.

Not fixed here: adding an LRU with a cap changes the enforcement semantics
(eviction hands the evicted caller a fresh allowance) and, as the finding itself
says, has to be designed together with G2. Not small.

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

**Status (triage 2026-08-10): LIVE, ordering unchanged.** The preflight
short-circuit is now at `crates/gateway/src/router/dispatch.rs:1239-1247`
(`return build_preflight_response(cors, origin, wall_start);`).
`execute_resource_tree` is called ten lines later at `dispatch.rs:1260`, and
every gate is inside it: `check_account` `:1307`, `check_spend` `:1318`,
`check_rate_limit` `:1341`, `acquire_concurrency` `:1344`. Only route lookup
(`:1201`) and `canonicalize_dispatch_path` (`:1220`) run before the preflight
return, and neither gates.

Nothing upstream gates either: `grep -n "\.wrap(\|middleware" crates/gateway/src/main.rs`
returns nothing - there is no middleware chain at all - and
`enforce::check_rate_limit` has exactly one non-test call site platform-wide.

No test drives the ordering: the four preflight tests in `router/cors.rs` call
`build_preflight_response` / `lookup_resource` directly, never `handle_request`.

Not fixed here: moving the preflight branch inside `execute_resource_tree`
changes what an OPTIONS request costs and when it can 402/429 - a product
decision about whether preflights are billable and blockable, which is exactly
the "what the platform SHOULD do" class. Report, do not patch.

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

**Status (triage 2026-08-10): LIVE, all three sub-claims.**

(a) `handle_subscription_dispatch` is now at `dispatch.rs:2281`. It contains no
`return` statement at all - the sole exit is the tail expression at
`dispatch.rs:2315-2322` building the 501 `UNIMPLEMENTED` body. The CHWBL work
before it is real and discarded (`select_with_affinity` `:2307`, `acquire`
`:2308`, `release` `:2311`). Reached from the only subscription arm,
`dispatch.rs:1580-1589`. The 426 for a non-upgrade GET is at `:1392-1397`.

(b) Both named symbols still have **no definition anywhere**. A whole-tree
search over `crates/` and `sdks/` returns exactly two hits, both comments:

```
crates/gateway/src/router/dispatch.rs:1361:    //    in `proxy_subscription_upgrade` once we get past the rest of
crates/gateway/src/router/dispatch.rs:1577:            // WS proxy itself is wired in `proxy::forward_subscription`.
```

`crates/gateway/src/proxy.rs` contains `forward_dispatch`,
`forward_workflow_advance`, `forward_to_worker_dispatch`, `forward_to_worker_path`,
`forward_http` and private helpers - no `forward_subscription`. Note that
`3e98347fa` edited comments nearby on 2026-08-09 and left both phantom-symbol
comments intact. The function's own doc comment (`dispatch.rs:2270-2275`) *is*
honest ("gateway-fronted subscriptions return 501"), so the file contradicts
itself within one screen.

(c) Correction to the finding: `docs/architecture/gateway-routing.md:82-88` is a
**bullet list, not a table**. `- \`Subscription\`: GET plus WebSocket upgrade
headers`, with no unimplemented note; grepping both that file and
`docs/reference/websocket-design.md` for `501|unimplemented|not yet wired|UNIMPLEMENTED`
returns zero hits. `websocket-design.md:89-99` is worse than silent - it
affirmatively describes gateway WS-upgrade auth as working, and that paragraph
was *added* on 2026-08-09 by `f2529c60b`, still gaining no 501 note.

**On the mapping to task #179 - it does NOT close G6, and the mapping should not
be relied on to do so.** Task #179 exists, is `completed`, and its content is
accurately remembered: it concluded the 501 makes the CSWSH concern unreachable
by construction, citing `dispatch.rs:2315`, which matches HEAD exactly. But #179
is the *CSWSH* finding, and the 501 is its **bound**, not its subject. G6's three
claims are that the 501 exists (which #179 depends on), that two live comments
name functions that were never written, and that no doc says so. #179 addresses
none of them; closing it removed nothing G6 asserts. #179 itself records what
survives: *"`csrf_origins` will not cover subscriptions when WS proxying is
wired."*

Not fixed here: sub-claim (b) is a two-line comment correction and (c) is a doc
line, both cheap - but a comment fix admits no red-before-green test, and the
underlying gap (build the WS proxy, or declare subscriptions single-tenant-only)
is the product decision this finding names. Left whole rather than half-fixed.

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

**Status (triage 2026-08-10): LIVE, and it has REPLICATED since the review.**
One supporting claim in this finding is **wrong** - see below.

Site 1 unchanged, `crates/gateway/src/router/auth.rs:912-916`, still says
`is_family_revoked_since` and still says `(NOT cached)`. The body still calls
`family_revocation_decision` (`auth.rs:992`), which short-circuits on the cache
at `auth.rs:99` and only reaches the DB via `revoked_after_for` at `auth.rs:123`.

Site 2 unchanged, `crates/gateway/src/lib.rs:138` still says *"A miss performs
one `is_family_revoked_since` DB read"*. The miss performs `revoked_after_for`.
The TTL sentence in the same doc is correct, so this site is a wrong-name defect
only.

**Site 3 is new and post-review.** Commit `4e4dafce4` (2026-08-10, *after* the
review) added `crates/gateway/src/sessions.rs:157-159`:

```
/// marker (`is_family_revoked_since(client_id, pws_, iat)`, an uncached
/// `SELECT EXISTS` on every request - see
/// `crate::router::auth::resolve_app_session_user_header_inner`)
```

That comment cites, by name, the very doc comment this finding says is wrong,
and its commit message says *"Measured before writing this: the per-request gate
exists and is uncached"*. The wrong claim was measured against itself and
copied into a third file. G7 is not merely unfixed; it is spreading, which is
the strongest possible argument for the finding.

**CORRECTION - this finding's supporting claim is FALSE.** The review states
*"The gateway never calls `is_family_revoked_since` at all - the only callers in
the tree are `crates/auth/src/oidc/{userinfo,introspect}.rs` and two test
files."* It does call it, in production, and did when the review was written:
`crates/gateway/src/auth_token.rs:1319`, inside `session_cookie_family_revoked`,
on the `GET /__zeroship/auth/session` fast path - and *that* call really is a
direct uncached read. So the gateway has **two** revocation readers with
different caching, and the defect is sharper than stated: the doc comments do not
name a function that does not exist, they name the *other, genuinely uncached*
reader, so a reader who checks the name finds a real function that behaves
exactly as advertised - and stops.

Not fixed here: three doc-comment sites, no red-before-green test is possible
for a comment. Recommended as the cheapest high-value follow-up, precisely
because `4e4dafce4` proves an unfixed wrong comment gets re-derived as truth.

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

**Status (triage 2026-08-10): LIVE.** 17 functions, all individually
`#[cfg(test)]`, contiguous from `dispatch.rs:209` to `:774` - **566 lines**,
matching the finding's "~600".

The duplication claim was checked by actually extracting and `diff`ing the
functions rather than taking "byte-for-byte siblings" on trust:

| function | dispatch.rs | plugin-workflow/src/advance.rs | result |
| --- | --- | --- | --- |
| `single_worker_result_to_outcome` | 259-350 | 209-300 | byte-identical, 92 lines |
| `normalize_workflow_step_result` | 626-689 | 553-616 | byte-identical, 64 lines |
| `legacy_step_result_to_outcomes` | 521-608 | 451-538 | byte-identical, 88 lines |
| `parse_iso8601_duration_ms` | 724-766 | 713-755 | byte-identical, 43 lines |
| `normalize_workflow_outcomes` | 413-519 | 356-450 | 12 comment lines + one local rename (`wn` vs `workflow_name`) |

Four of five sampled are byte-identical; the fifth has already begun to drift,
which is itself the finding's point.

Unreachability holds: `workflow_advance_internal` (`dispatch.rs:105`) calls only
`workflow_worker_advance_response` (`dispatch.rs:182`), which is not
`cfg(test)`. A non-test reference to a `#[cfg(test)]` item is a compile error,
so the release binary does not contain the fork.

**Two corrections to the finding's counts.** The 10 fork-driving tests are
confirmed. But the review says only *two* tests drive production; there are
**three** - `internal_workflow_advance_spend_blocked_app_returns_402`,
`public_vhost_workflow_advance_path_is_404`, and
`internal_workflow_advance_routes_to_worker_and_returns_ack`, the last of which
does exercise the real ack path. `cargo test -p zeroship-gateway --lib workflow_`
runs 13 tests, 13 passed. The 10:3 ratio still makes the finding's argument.

Not fixed here: deleting 566 lines and 10 tests is not a small change, and
"delete coverage" is the kind of edit that wants its own reviewed commit.

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

**Status (triage 2026-08-10): SHIPPED** by `fd54716ec fix(gateway): refuse an
https --control instead of silently sending the key in cleartext`, which brings
`--control` to the `--blob-store` bar the finding asked for: validate at boot and
`exit(2)`.

The commit's own verification is the reason this is credible rather than
plausible - it proved the validator fires on the **real binary**, not only in a
unit test, which is the dead-gate failure mode this repo keeps hitting:

```
--control https://control.internal:9090   exit=2, "unsupported --control
                                          scheme \"https\": ..."
```

with a positive control retained (`control_url_accepts_http` passing, so the
validator refuses only the scheme it cannot serve and compose/local dev keeps
working).

Note the finding's second site is **not** covered by that commit:
`crates/gateway/src/proxy.rs` still has the same `unwrap_or(80)` for the worker
hop. That is intra-cluster and the finding treats it as an aside, but it is
untouched.

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

**Status (triage 2026-08-10): LIVE, both consequences.**

The host extraction is now at `dispatch.rs:2434-2439` and unchanged; `forward_url`
(`dispatch.rs:2384-2389`) interpolates it verbatim into
`format!("{scheme}://{host}/{tail}?{q}")`.

The decisive check the finding rests on was re-run and is still empty:

```
$ grep -rn "base_domain\|app_domain\|allowed_host\|host_suffix\|allowed_hosts\|root_domain\|apex_domain" crates/gateway/
(no output)
```

The only anchored host check in the crate is `AUTH_HOSTS` (`dispatch.rs:1043`),
consumed solely by `is_auth_host` to divert the two platform auth hosts.
`extract_app_name` (`dispatch.rs:49-90`) takes `&host[..dot_pos]` and never looks
at the suffix; on the path route nothing reads `Host` at all. No middleware is
mounted (`main.rs:834-919` has zero `.wrap(` calls).

Second consequence also LIVE: `browser_auth.rs:170-174` still builds the expected
origin as `format!("{scheme}://{host}{path}")`, called at `:122` with
`&route.host`, which is the raw header (`auth_token.rs:89-94`). `scheme` is fixed,
so the check constrains only the path. The comment at `browser_auth.rs:114-117`
still claims the mirror stops a gateway-layer deviation widening the OP's
allowlist; because both sides derive from the same attacker-controlled header,
it does not.

Existing coverage anchors `is_auth_host` only
(`is_auth_host_rejects_unanchored_lookalikes` and three siblings, 4 passed);
there is no equivalent test for the forwarded-URL host.

Not fixed here: both fix shapes (a configured base domain with a dev bypass, or
reconstructing the URL from the resolved route's canonical hostname) add
configuration surface and change what `request.url` says to every deployed app.
Contract decision.

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

**Status (triage 2026-08-10): LIVE. All four legs re-read at HEAD and confirmed;
NOT observed against a live stack.**

1. `rotation_started_at` is stamped at `auth_token.rs:1088` (`let
   rotation_started_at = now_secs();`) at the top of `do_refresh`, before the OP
   refresh. The re-check at `auth_token.rs:1232-1236` still gates on
   `revoked_after >= rotation_started_at`, so a marker written *before* the
   request lands in `Ok(_) => {}`. The comment at `:1223-1226` confirms the
   intent is the TOCTOU window only - the design is the gap.
2. `revoke_grant_cascade` (`crates/control/src/oauth_grants_handlers.rs:170-232`)
   read in full: it deletes the `oauth_grants` row (`:186`), revokes the relay
   alias (`:194-198`) and inserts `token_revocations` (`:223-225`). It never
   names `app_session_anchors`. The complete set of non-test writers of that
   table is `crates/auth/src/identity/password_reset.rs:344` and
   `crates/gateway/src/anchors.rs:302,328,361` - no control-plane grant path.
3. `crates/auth/src/oidc/refresh.rs:509-513` gates on `row.revoked_at`,
   `family_has_revoked_row` (which queries `oauth_refresh_tokens` only,
   `refresh.rs:1117-1124`) and two expiries. `oauth_grants` has zero hits in the
   file; `token_revocations` appears only as three INSERTs, never as a read gate.
4. `family_revoked_at` is at `crates/authz/src/wrapper_revocation.rs:108-110`
   and is strictly `>`: `revoked_after.is_some_and(|ra| ra > iat)`. Its own doc
   at `:88-89` states the resurrection property outright: *"A token whose `iat`
   predates the marker is rejected; one minted after the marker is fine."*

The chain closes: the fall-through at `auth_token.rs:768-769` reaches
`rotate_family` (`:850`), `sessions::create` (`:903`) and `sign_session_cookie`
(`:932`), and the fresh cookie's `iat` is `now` (`session_token.rs:190`). The
comment justifying the fall-through (`auth_token.rs:756-757`, *"ends in
`login_required` for a revoked family"*) is still there and still false, because
leg 3 shows the OP check looks at neither the grant nor the marker.

Not fixed here: the finding offers two fixes in different crates (change the
comparison instant in the gateway, or make `revoke_grant_cascade` revoke the
anchor in the control plane). Which one is right is a decision about where
revocation authority lives. Highest-severity LIVE item in this document.

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

**Status (triage 2026-08-10): LIVE.** `session_csrf_guard`
(`auth_token.rs:277-284`) still passes `want_mint` as *both* `require_custom_header`
and `require_origin`, and its doc at `:274-276` still says a non-mint GET needs
neither.

The handler still contradicts it. `want_mint` is computed at
`auth_token.rs:730-733`, the guard is called once at `:735`, and the fast path
(`:759-798`) falls out of its block on a missing / expired / tampered /
non-`pws_` / revoked cookie into reload-recovery, reaching `rotate_family`
(`:850`), `sessions::create` (`:903`) and `sign_session_cookie` (`:932`) - all
with `want_mint == false`. The handler's own comment at `:756-757` confirms the
fall-through is the designed steady state, not an edge.

`Sec-Fetch-Site` is still enforced only when present (`auth_token.rs:247-260`:
`if let Some(sfs) = ... { if sfs != "same-origin" {`), so absence is a pass.

The finding's "why it matters beyond the modest impact" point was confirmed by
running the suite it names:

```
$ cargo test -p zeroship-gateway --lib session_csrf_tests
test auth_token::session_csrf_tests::non_mint_read_is_lenient ... ok
test auth_token::session_csrf_tests::mint_without_origin_is_rejected ... ok
test auth_token::session_csrf_tests::mint_without_custom_header_is_rejected ... ok
test auth_token::session_csrf_tests::mint_with_foreign_origin_is_rejected ... ok
test auth_token::session_csrf_tests::legitimate_same_origin_mint_succeeds ... ok
test result: ok. 5 passed; 0 failed
```

All five pass, and `non_mint_read_is_lenient` passes *because* it sends neither
header - it green-lights exactly the property this finding says is the wrong one
to pin. The module tests the guard, never the handler.

Not fixed here: "run the guard after the fast path decides" restructures the
handler's control flow on the authentication path. Not small.

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

**Status (triage 2026-08-10): LIVE, all three parts.**

The `if let Ok(..) else` at `oidc_rp.rs:740-749` is unchanged, and `try_verify`
(`:706-738`) can yield `NoMatchingKey` (`:711`) *or* `OidcError::Verify` from
`decode` (`:736`) covering bad signature, `exp`, `nbf`, `iss` and missing spec
claims - all landing in the same `else`.

Both siblings still match `NoMatchingKey` only:
`crates/core/src/oidc_verify.rs:476-484` and
`crates/core/src/logout_token.rs:289-302`, the latter still documenting why.
The false "mirroring [`zeroship_core::oidc_verify::verify_id_token`]" claim is
still at `oidc_rp.rs:676-678`.

`JwksCache::refresh` was read end to end (`crates/core/src/oidc_verify.rs:253-362`):
no single-flight, no minimum interval, no rate limit, no breaker. Its only two
protections are a per-fetch timeout (`:275`, 5s default) and stale-on-error. The
struct has no in-flight flag and no last-attempt timestamp - `JwksState` is
`{ keys, fetched_at }` (`:147-151`) and `fetched_at` is written on **success
only** (`:355-359`), so failed refreshes leave no throttle trace at all.

**Evidence the finding did not cite, which strengthens it:** `crates/core` already
contains the exact control test for the correct behaviour -
`verify_id_token_refreshes_jwks_only_for_missing_key` (`oidc_verify.rs:1129`),
asserting `mock.hits() == 0` with *"signature-independent failures must not force
a JWKS refresh"* and `mock.hits() == 1` for `NoMatchingKey`. There is no
counterpart for `verify_access_jwt`. The property is already written down as
desirable in this codebase; only the gateway's copy diverges.

Reachability holds: `resolve_bearer_user_header` (`router/auth.rs:655`)
pre-checks only the unsigned issuer peek (`:675`, `:680`) before
`verify_access_token` (`:688`).

Not fixed here despite being nearly mechanical (match `NoMatchingKey`, delete or
honour the doc claim): a red-before-green test needs a JWKS mock wired into the
gateway's `OidcRp`, which `crates/core` has and `crates/gateway` does not.
Building that harness is the real work, and shipping the one-line match without
it would be an untested change on the token-verification path. **Strongest
candidate for the next fix** - the target behaviour, and its test shape, are
already written in `crates/core`.

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

**Status (triage 2026-08-10): was LIVE, FIXED IN THIS TRIAGE.**

`is_safe_oidc_original_path` now ends the first segment at the first `/`, `?` or
`#` (`crates/gateway/src/router/dispatch.rs`, `path[1..].find(['/', '?', '#'])`),
with a comment stating why the delimiter set is what it is. The security intent
is untouched: a colon in the first *path* segment still fails.

Red before the fix, colon-free control asserting first and passing:

```
thread 'router::dispatch::tests::oidc_original_path_keeps_a_colon_that_lives_in_the_query'
panicked at crates/gateway/src/router/dispatch.rs:5062:9:
assertion `left == right` failed: a colon inside the QUERY must not make the path unsafe
  left: "/"
 right: "/search?q=https://example.com"
```

Mutation check: narrowing the fix to `find(['/', '?'])` - dropping only the `#` -
turns the test red on the fragment case, so every delimiter in the set is
load-bearing rather than incidentally passing:

```
assertion `left == right` failed
  left: "/"
 right: "/doc#a:b"
```

The new test carries a negative control (`/foo:bar?q=1` must still sanitise to
`/`) so a future widening of the check cannot pass by loosening it. The
pre-existing `oidc_original_path_rejects_protocol_relative_redirects`, including
its `/foo:bar/baz` case, stays green.

**What this does NOT cover:** it asserts nothing about fragments arriving over
the wire (browsers never send them) and nothing about the `bytes[1]` alnum gate,
which the neighbouring test owns.

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

**Status (triage 2026-08-10): LIVE (a) and (b). The module-doc claim is SPLIT,
not uniformly wrong.**

(a) `op_client.rs:394-406` unchanged: `Ok(Ok(resp))` calls `breaker.on_success(is_probe)`
for any completed response, 2xx through 5xx. The only `on_failure` arms are
`Ok(Err(e))` (`:412`, transport) and `Err(_elapsed)` (`:420`, timeout). Callers
inspect status only afterwards (`oidc_rp.rs:275`, `:500`, `:526`).

(b) `op_client.rs:222-230` unchanged: `on_success` stores `Closed`
unconditionally, never reading `self.state()`; `was_probe` gates only the
probe-slot release, not the state transition. No test covers the interleaving -
every test in `op_client.rs:426-625` drives the breaker sequentially on one
thread, and `shared_breaker_arc_trips_for_all_holders` only `Arc::clone`s and
asserts shared visibility; it spawns nothing.

**Correction to the finding's reasoning.** It says the module doc "names exactly
this case as the motivating risk", implying the doc is wrong. Only half of it is.
`op_client.rs:6` does say *"slow or 5xx-ing"*, but `op_client.rs:28-33` of the
same doc states the limitation correctly and on purpose: *"A OP 4xx (e.g.
`invalid_grant`) is a VALID upstream response, NOT a breaker failure - only
transport errors and timeouts count."* So the module is internally inconsistent
rather than uniformly mistaken, and part of the behaviour in (a) is a documented
deliberate choice. The 5xx half is still undefended and (b) is unambiguously a
defect.

**Narrowing of the attacker-drivable framing.** The finding says
`/__zeroship/auth/callback` reaches `post_token` with attacker-chosen `code`
values. True, but each attempt needs a matching stash and verifier
(`oidc_rp.rs:256` binds `code_verifier` from `stash.verifier`), so the attacker
must initiate a real login per attempt rather than spray blind codes. Note this
interacts with G17: a replayable stash removes that per-attempt cost.

Not fixed here: deciding that a 5xx counts as a breaker failure while a 4xx does
not is a policy change to a deliberately documented rule, and (b) needs a
compare-exchange state machine plus a concurrency test the module has never had.

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

**Status (triage 2026-08-10): LIVE, all three sub-defects.**
`git log -- crates/gateway/src/signal_ingress.rs` shows a single commit
(`549e2656e`); nothing has been fixed since the file was written.

(a) `PLACEHOLDER_LIMITER` (`signal_ingress.rs:48`) is still an uncapped
`HashMap<String, PlaceholderBucket>`; `check_placeholder_rate_limit` (`:56-64`)
only does `entry(...).or_insert_with(...)`. Grepping the symbol returns exactly
two lines (48, 58) - no sweeper, no cap, no `remove`/`retain`. And a new key is
always admitted, because the bucket is born full (`:20` `PLACEHOLDER_BURST: f64
= 10.0`, `:31` `tokens: PLACEHOLDER_BURST`), so the map grows a permanent entry
per source *while admitting the request*. The nearby correct pattern is real:
`backchannel_logout.rs:31` caps at `MAX_INFLIGHT_LOGOUT_JTIS: usize = 50_000`
with `retain` at `:420` and eviction at `:424-426`.

(b) `source_key` (`:50-53`) still uses `req.peer_addr()`. The proxy-aware helper
exists (`dispatch.rs:866`, `pub(crate) fn client_ip(req, trust_proxy)`) and
`trust_proxy` is on the config (`lib.rs:67`); `signal_ingress.rs` never mentions
it. Impact stays deployment-dependent, as the finding says.

(c) `signal_ingress.rs:103` still constructs `cyper::Client::new()` inside
`forward_to_control`, once per accepted POST. The rule it violates is verbatim
in `op_client.rs:17-18`.

Mount sites confirmed at `main.rs:859-871`: both routes attach only a
`PayloadConfig` body cap. `enforce::check_rate_limit` has exactly one non-test
call site in the crate (`dispatch.rs:1341`, inside `execute_resource_tree`), and
`main.rs` has no `.wrap(` at all, so these routes genuinely bypass it.

The module's only test (`placeholder_bucket_enforces_burst`, `:133`) exercises
`PlaceholderBucket::allow` in isolation and touches neither the map growth nor
the client key.

Not fixed here: (b) and (c) are each small in isolation, but the module is a
self-described placeholder ("G5 placeholder - operator-pending durable
rate-limit store", `:57`) for an unauthenticated public endpoint. Replacing its
limiter is the operator-pending design decision the comment names, and patching
the key without the store would make the endpoint look correct while remaining
unbounded.

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

**Status (triage 2026-08-10): LIVE, both parts.**

`Stash` (`oidc_rp.rs:899-907`) still has exactly six fields and no timestamp.
`encode` (`:911-918`) signs that JSON and nothing else; `decode` (`:923-942`) is
split -> HMAC recompute -> constant-time compare -> `from_slice`, with no age
check. `finish_callback` goes decode (`:227-228`) -> state compare (`:233-235`)
-> client_id compare (`:244-246`) -> `POST /token` (`:248-273`), and
`handle_auth_callback` (`dispatch.rs:2621-2715`) adds no freshness check either.
`STASH_MAX_AGE_SECS` (`:1034`) is used at exactly one non-test site: the cookie's
`Max-Age` attribute (`:1048`).

**Evidence the finding did not cite:** the platform already implements this
correctly elsewhere. `crates/auth/src/ui/oauth_stash.rs` has its own
`STASH_MAX_AGE_SECS` (`:146`) *and* puts it inside the signed blob -
`oauth_stash.rs:65`: `exp: iat.saturating_add(STASH_MAX_AGE_SECS)`. So this is
not a missing convention; the gateway's copy dropped the field the sibling
carries.

The compounding claim is confirmed: `dispatch.rs:996-998` intercepts
`/__zeroship/auth/callback` and returns `handle_auth_callback` directly;
`handle_request` is only called at `:1008`, and every gate lives downstream of it
inside `execute_resource_tree` (account `:1306`, spend `:1319`, rate limit
`:1341`, concurrency `:1344`). The callback path is reached with none applied.

Not fixed here: adding a field to a signed wire blob is a format change. Cheap
pre-launch (`AGENTS.md`: break the shape, update every producer and consumer in
one patch) but it is still a wire-format decision, and it should land with the
age gate in `decode`, not as a field nothing reads.

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

**Status (triage 2026-08-10): doc half LIVE (NOT shipped), behaviour half
LIVE-BY-DECISION, "Related" sub-claim LIVE.**

Commit `d49abc963` addressed this finding and deliberately did not change
behaviour, recording the reasoning - over-revoking is the safe direction for a
logout signal, and whether an unmatched `sid` should instead be a no-op is a
contract decision. That is a legitimate outcome and the behaviour half should be
read as an accepted risk, not an open bug.

**But the doc half is not fixed.** `d49abc963` rewrote the *inline* comment
inside `handle` (now `backchannel_logout.rs:128-141`, and it is a good rewrite:
*"the fallback is WIDER than 'sid was absent'"*). It left the **rustdoc on
`handle`** - the text this finding actually quoted - untouched.
`crates/gateway/src/backchannel_logout.rs:48-50` at HEAD:

```
/// Revocation policy: for per-app clients, prefer `sid` and revoke only local
/// sessions that originated from that OP session. If the logout token lacks
/// `sid`, fall back to revoking all sessions for the token's `sub` at that app.
```

That is the original wrong sentence, verbatim, and it is the part `cargo doc`
and an IDE hover surface first. **This is the same failure mode as G7**: the
paragraph gets fixed, the structured top-of-item text does not. Two of the
eighteen findings are now instances of it, and G7 shows the stale version
getting re-derived as truth a day later.

Code arm unchanged (`backchannel_logout.rs:222-226`, `if users.is_empty()` ->
`revoke_by_sub` at `:461`).

The "OP always sends both" premise is confirmed:
`crates/auth/src/oidc/backchannel_logout.rs:158-163` passes `sub: Some(&rp.sub)`
and `sid: Some(&rp.sid)` from non-nullable `RelyingPartySession` fields, and it
is the only production caller of `issue_logout_token`. So the "lacks sid" branch
the rustdoc describes is dead against this OP - which is precisely why leaving
that rustdoc in place is worse than a normal stale comment: it documents a branch
that can never run as though it were the policy.

"Related" sub-claim confirmed LIVE: `retryable_processing_error`
(`:405-409`) returns 503 with no jti burn (the durable
`logout_jti_cache.insert` at `:326` is reached only after successful revocation;
every earlier return drops `LogoutJtiClaim`, whose `Drop` at `:446-458` removes
the in-flight entry), and the sender does not retry - `emit_to_rps`
(`crates/auth/src/oidc/backchannel_logout.rs:174-184`) makes one pass, warns on
failure and continues. No queue, no backoff, no outbox.

Not fixed here: the rustdoc correction is a comment (no red-before-green test
possible), and it should land with G7's three sites as one "stale rustdoc over
correct inline comment" sweep.

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
  **Still true after the 2026-08-10 triage**, which added in-process red tests
  for G1's remaining half, G2, G3 and G14 but stood up no stack. Any cross-tier
  coverage belongs in `tests/golden_path.sh`, not a crate-local suite.

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
