# Round 2 — Token security: findings

Total: 7 findings (0 critical, 2 high, 3 medium, 2 low).

## CRITICAL

(none)

## HIGH

### H1. OAuth-introspection bearer path on control plane never enforces `aud`
**File:** `crates/control/src/authz_guard.rs:162-198`

**Severity rationale:** Any hydra-issued access token whose scopes include the appropriate scope strings — regardless of the audience or client it was issued to — is accepted by the control plane. In a deployment with multiple hydra OAuth clients (gateway, builder, app-runtime, future first/third-party clients), a token meant for the gateway or for an app can be replayed against `console.zeroship.ai`/`api.zeroship.ai` control endpoints and yield real CRUD rights up to the scope it carries. Hydra's `/oauth2/introspect` response carries the `aud` claim (and is already deserialized in `crates/core/src/hydra.rs:213-218,221-231`); the control plane just throws it away.

**Reproducer:**
1. Register two OAuth clients with hydra: `gateway` and `console.zeroship.ai`. Both have `apps:read` in their grantable scopes.
2. Have a user grant `apps:read` to `gateway`. Receive an access token whose introspection returns `active: true`, `aud: ["gateway"]`, `scope: "apps:read"`, `sub: <user-uuid>`.
3. `curl -H 'Authorization: Bearer <gateway-token>' https://api.zeroship.ai/v1/apps` — the request is honored: `oauth_guard_from_bearer` only checks `active`, `sub`, and `scope`. The reproducer audit-trail entry in `control.authz_decisions` shows the token's principal_id but no client/audience.

**Suggested fix:** Add a constructor parameter `expected_aud: &str` to `oauth_guard_from_bearer` (or thread `state.expected_oauth_audience` through) and reject the request if the introspection result's `aud` (which is already parsed as `Option<Vec<String>>`) does not contain the control plane's audience identifier (`control.zeroship.ai` or the canonical PAT audience). Same check should be applied symmetrically on the gateway side if/when a non-DPoP OAuth bearer path is added there. Add a test alongside `crates/control/tests/authz_guard_oauth_test.rs` that asserts a `gateway`-aud token is rejected with 401 even when scopes match.

### H2. DPoP-bound creator-app requests authenticate as user with empty `id` for client-credentials grants
**File:** `crates/gateway/src/wrapper_token.rs:196-216` and `crates/gateway/src/router/auth.rs:332-356`

**Severity rationale:** When a hydra introspection returns `active: true` without `sub` (the canonical client-credentials grant shape — RFC 7662 §2.2 makes `sub` optional, and hydra omits it for non-user grants), the wrapper-token issuer writes `sub: introspection.sub.clone().unwrap_or_default()` (an empty string) into the wrapper claim set; the dispatcher's `build_worker_user_from_wrapper` and `build_worker_user_from_introspection` then construct an `OwnedWorkerUser { id: "", ... }` and sign a `ZeroShip-User` HMAC for it. The worker MAC-verifies the header successfully and the creator app sees an authenticated principal whose id is the empty string. A creator-app handler that only checks "any user authenticated" (the common case) treats the client-credentials caller as any other user. Worse, any caller that does `if (user.id) { ... }` style gating would still see an authenticated request because the header is present.

**Reproducer:**
1. Register an OAuth client with hydra using the client-credentials grant. Grant it `openid` (or any scope) and obtain an access token via `POST /oauth2/token`.
2. Issue a DPoP proof for a creator-app host. `POST /__zs/auth/dpop-exchange` with `Authorization: Bearer <client-credentials-token>`. The gateway introspects the token, sees `active: true, sub: None`, and mints a wrapper with `sub: ""`.
3. Send the wrapper + a fresh DPoP proof to any creator-app endpoint. The worker receives a valid `ZeroShip-User: <base64({"id":"","email":"","name":"","email_verified":false})>...` header.
4. From inside the creator app, `env.auth.user` (or however it surfaces) returns an authenticated user with `id: ""`. Authorization decisions in user-land that only check "is there a user?" pass.

**Suggested fix:** At `wrapper_token::Issuer::issue` (or the dpop_exchange handler before calling `issue`), if `introspection.sub.is_none() || sub_is_empty`, refuse to mint a wrapper — return `Err(GatewayError::Internal(...))` so the exchange endpoint returns 401 with `error: hydra_token_inactive` or a new `error: no_subject`. Symmetric fix in `resolve_dpop_user_header`'s introspection fallback: reject if `info.sub` is empty. Add a regression test in `crates/gateway/src/router/auth.rs::tests` along the shape `dpop_rejects_wrapper_with_empty_sub` asserting `resolve_dpop_user_header` returns `None` when the wrapper claims `sub: ""`.

## MEDIUM

### M1. Hydra-introspection cache caches `active: true` for full TTL, defeating revocation for up to 5 minutes
**File:** `crates/core/src/hydra.rs:108-166`

**Severity rationale:** `HydraIntrospector` caches every active introspection result for `DEFAULT_TTL = 300s`, keyed by SHA256(token). When an operator/end-user revokes a token via hydra (`/oauth2/revoke`, `/oauth2/sessions/logout`, admin grant deletion), the next request to control or gateway through the cached path continues seeing `active: true` for the remaining TTL. There is no cache invalidation hook tied to the OIDC back-channel logout receiver in `crates/control/src/backchannel_logout.rs` or `crates/gateway/src/backchannel_logout.rs`, so even a successful BCL revocation doesn't drop the introspection cache entry for the affected `sub`'s tokens.

**Reproducer:**
1. Mint an OAuth access token. Cause it to flow through `HydraIntrospector::introspect` once. Cache is populated.
2. Revoke the token at hydra (`POST /oauth2/revoke`).
3. Replay the same token within 300 s. `cached_result` returns `Some(IntrospectResult { active: true, ... })` and the request is honored.

**Suggested fix:** Either (a) when a BCL `logout_token` is verified, walk the introspection cache and drop any entry whose `sub`/`sid` matches and the JTI replay-cache; or (b) shorten the introspection cache TTL to a small skew window (e.g. 5–10s); or (c) wire the `/oauth2/revoke` admin endpoint into the cache via a side-channel. The simplest fix is exposing a `HydraIntrospector::invalidate_by_sub(sub: &str)` and calling it from both `backchannel_logout::handle` implementations after revocation succeeds. Add a unit test demonstrating cache invalidation by sub.

### M2. Wrapper tokens have no revocation path — captured wrapper = 1-hour access regardless of hydra revoke
**File:** `crates/gateway/src/wrapper_token.rs:166-229` and `crates/gateway/src/router/auth.rs:184-322`

**Severity rationale:** The wrapper-token Verifier only checks signature, `iss`, `aud`, `exp` (1h), `kid`, `typ`, and `cnf.jkt`. There is no cross-reference to a denylist, no introspection-back-to-hydra by the `wraps` claim, no DB lookup. So an attacker who captures both the wrapper and the DPoP private key retains full access for the wrapper's 1-hour `exp` even after the underlying hydra token is revoked or the user signs out via BCL. The Phase-8 doc-string at lines 17-26 says the `wraps` claim "ties the wrapper to the underlying hydra token... so revocation pings can invalidate the wrapper by introspecting the original" — but no code path uses that claim today.

**Reproducer:**
1. Client mints a DPoP keypair, exchanges a hydra access token for a wrapper. Stash the wrapper, DPoP private key, and `jkt`.
2. Operator revokes the hydra access token (or user signs out via BCL).
3. Within the wrapper's `exp` window, replay the wrapper + a fresh DPoP proof. The Verifier returns success; the introspection-fallback branch is never reached. Request is honored.

**Suggested fix:** Either (a) on every wrapper-verify, look up `wraps` in a "recently revoked" set the BCL receiver populates; or (b) introspect the underlying hydra token by the SHA-256 fingerprint; or (c) shorten the wrapper `exp` to a few minutes (1–5 min). Option (a) is the lowest-overhead. Add a Postgres table `auth.wrapper_revoked` keyed by `wraps` with a 1h retention sweep, populated from `console_backchannel_logout::handle` and `gateway_backchannel_logout::handle`, and consulted in `wrapper_token::Verifier::verify`. Regression test asserts revoked wrapper is rejected.

### M3. `PgJtiCache::insert` silently drops the caller's `ttl_secs` and uses a fixed 5-minute retention window
**File:** `crates/core/src/dpop.rs:930-969`

**Severity rationale:** The signature `pub async fn insert(&self, jti: &str, _ttl_secs: i64)` accepts a TTL parameter but the underscore prefix shows it is intentionally ignored. The sweep then deletes rows older than a hardcoded 5 minutes. A future caller that passes a longer TTL (e.g. raising the DPoP iat window) would silently keep its old behavior in the PG tier, opening a replay window cross-instance.

**Suggested fix:** Either accept the parameter and use it (`INTERVAL '$2 seconds'`), or rename it to `_unused_ttl_secs` AND adjust the sweep to track the longest configured TTL the binary actually uses. Add an explicit constant `PG_JTI_RETAIN_SECS: i64 = 300` so future readers see the value, and assert at construction that no caller passes a higher TTL than this constant.

## LOW

### L1. Magic-link redeem allows post-60-second takeover when finalize is never called
**File:** `crates/auth/src/identity/magic_link.rs:149-197`

**Severity rationale:** `redeem_pending` accepts a redeem if `consumed_pending_at IS NULL OR consumed_pending_at <= NOW() - INTERVAL '60 seconds'`. If (a) attacker has the raw token, (b) legitimate user clicks first and the UI fails to call `finalize_consume` or `clear_consume_pending`, and (c) more than 60s pass, then the attacker's click on the same email link succeeds.

**Suggested fix:** Make the stale-window narrower (e.g. 5s) AND bind the pending reservation to a fingerprint (the requesting IP, the issuance-time CSRF nonce). Better: on `redeem_pending` finding a stale `consumed_pending_at`, mark the row `consumed_at` outright (treat the prior failure as "burned").

### L2. PAT signing-key dev fallback uses well-known constant key bytes
**File:** `crates/control/src/token_handlers.rs:104-107`

**Severity rationale:** `PatIssuer::dev_insecure()` constructs an Ed25519 keypair from `[9u8; 32]` — a publicly recoverable seed. A misconfigured staging deploy that quietly carries `--dev-insecure` from a previous test session would expose every user account to silent PAT forgery.

**Suggested fix:** Either (a) generate a per-process random key in dev mode (`SigningKey::generate(&mut OsRng)`) so each restart invalidates prior tokens; or (b) refuse to start unless `--dev-insecure` is set even if `signing_key_file` is empty. The first option preserves the "no operator ceremony" property of dev mode. Add a regression test asserting `PatIssuer::dev_insecure()` produces a different `kid` on each call.

## Areas reviewed and clean

- **PAT JWT signature & header validation** — `alg` pinned to EdDSA, `typ` exact, `kid` matched against issuer's own thumbprint, `iss`+`aud` in `Validation`. No "alg from header" anti-pattern.
- **PAT revocation lookup at use time** — DB row's `revoked_at IS NULL AND (expires_at IS NULL OR expires_at > NOW())` is checked atomically with `policy_hash` match. Revocation immediate.
- **PAT grant subset enforcement at issue** — every (action, resource) enforced via Cedar against the owner's static policies. `MAX_GRANT_PAIRS = 10_000` bounds work. Forbids PAT-with-PAT issuance.
- **Wrapper-token DPoP `cnf.jkt` binding** — mismatch is hard reject; no fallthrough to introspection.
- **Wrapper-token Verifier** — explicit `kid` pre-check, `typ` pinned to `at+jwt`, `iss`+`aud` enforced. Default 60s leeway on `exp`.
- **DPoP proof verifier** — `alg` allowlist excludes HS*/none, `typ` exact, `htu` query/fragment stripping, ±60s `iat` window, `ath` SHA-256 binding when access token supplied, RFC 7638 canonical-form JWK thumbprint by hand.
- **DPoP `jti` replay protection** — atomic local insert under single `Mutex` lock, PG-tier insert via `ON CONFLICT DO NOTHING RETURNING`, lazy sweep, opportunistic PG sweep every 512 inserts.
- **OIDC back-channel logout** — `events` exact-match, `nonce` forbidden, `sub`/`sid` at least one required, ±5min `iat` window, JWKS rotated-key force-refresh only on `NoMatchingKey`. Replay-cache consulted after verify.
- **Email-verification / password-reset tokens** — 256-bit CSPRNG, SHA-256 stored hash, atomic single-use redeem, prior-token superseding.
- **ZeroShip-User HMAC header** — `request_id`+`iat` covered by MAC, replay rejected via expected_request_id mismatch and ±60s/±5s window, constant-time hex compare.
- **`extract_bearer`, `validate_control_key`, `hash_api_key`** — constant-time XOR-fold comparison.

## Blockers

- **M1 gateway invalidation hook:** `GateState` currently has no `hydra_introspector` field, and the gateway DPoP fallback/exchange paths call `OidcRp::introspect_token` directly without a local active-token cache. There is no gateway-side `state.hydra_introspector.invalidate_by_sub(&sub)` hook to wire without introducing a new gateway admin-introspection client. The cached `HydraIntrospector` path exists in control and is fixed there.

## Subsystems NOT fully audited (time-limited)

- The OIDC RP callback / consent flow — only intersection with token issuance was traced.
- Hydra-client (`crates/auth/src/hydra_client/*.rs`) — not reviewed for admin-API authentication / TLS pinning.
- The advisory-lock JWK rotation cron — not deep-audited for race conditions in the prepend+retire sequence.
- GitHub/Google OAuth identity providers — only integration with magic_link/password_reset was checked, not the OAuth handshake itself.

## Status

### CLOSED

- **H1** — `9f861ce5` (`audit R2.H1: enforce control OAuth audience`)
- **H2** — `5286ed57` (`audit R2.H2: reject OAuth tokens without subject`)
- **M1** — `a92f3b36` (`audit R2.M1: invalidate Hydra cache on logout`) for the cached control-plane `HydraIntrospector` path.
- **M2** — `f65e4d0e` (`audit R2.M2: deny revoked wrapper subjects`)
- **M3** — `d2f95f92` (`audit R2.M3: honor DPoP PG JTI TTL`)
- **L1** — `18eff1c4` (`audit R2.L1: burn stale magic links`)
- **L2** — `3a509627` (`audit R2.L2: randomize dev PAT key`)

### DEFERRED

- **M1 gateway invalidation hook** — no gateway-side `HydraIntrospector` cache exists to invalidate; gateway DPoP exchange/fallback paths call `OidcRp::introspect_token` directly. See `## Blockers`.
