# Round 10 - Cross-binary integration: findings

Total: 12 findings (0 critical, 4 high, 5 medium, 3 low).

Scope: How the five server binaries (control, gateway, worker, auth-server, sandbox) interact across HTTP boundaries — shared secrets, request-ID propagation, header trust at boundaries, service discovery, control-key validation, JWT issuer/audience consistency, CORS, cookie scoping, inter-service retries/circuit breakers, sandbox boundary, builder → control OAuth, and wire-format compatibility.

Not fully audited (deferred / out of scope for one round): Nomad/CH backend dispatch code paths, audit-log shipping (`crates/control/src/audit.rs`), back-channel logout fan-out completeness, stripe webhook delivery, the auth-server's `mailer` + `identity::oauth` provider code, and the CLI's `zeroship deploy` HTTP client.

## CRITICAL

(none — the integration trust boundaries are reasonably hardened. The headline credential-forgery primitives all enforce HMAC + request-binding; what remains is a set of availability, hardening, and dev-default issues.)

## HIGH

### H1. Gateway honours `X-Forwarded-For` for per-IP rate-limit buckets with no `trust_proxy` opt-in
**File:** `crates/gateway/src/router/dispatch.rs:115-119`, `crates/gateway/src/router/dispatch.rs:198-202`, `crates/gateway/src/main.rs:240-270`

**Severity rationale:** The gateway derives the rate-limit / concurrency bucket via `req.connection_info().remote()`. ntex's `connection_info()` consults the `Forwarded` / `X-Forwarded-For` / `X-Real-IP` headers BEFORE falling back to the peer socket — and the gateway exposes no `trust_proxy` knob (unlike control, which has one — `crates/control/src/lib.rs:113-118`, `crates/control/src/main.rs:113`). An unauthenticated client hitting `{app}.zeroship.ai` can rotate the IP bucket on every request by setting `X-Forwarded-For: <random>`, defeating the per-IP rate-limit (`RateLimitRegistry::new(1000, 2000)` in `main.rs:256`) and the per-resource session-fallback IP bucket (`compute_bucket_id` → `RateLimitPer::Session` → IP fallback). The gateway is the FIRST hop in production (it is `api.zeroship.ai`); there is no real upstream LB whose XFF can be trusted.

**Reproducer:**
1. Configure a creator app with a `rate_limit: { per: "ip", rps: 5 }` resource.
2. Send 100 POSTs in quick succession to that resource, each carrying `X-Forwarded-For: 10.0.0.<i>` for incrementing `i`.
3. None are throttled — each request lands in a different bucket key derived from `connection_info().remote()` returning the spoofed value.

**Suggested fix:** Add `--trust-proxy` (default `false`) to the gateway, identical to control's flag. When false, derive the rate-limit / affinity key from `req.peer_addr()` directly, ignoring `Forwarded` / `X-Forwarded-For` / `X-Real-IP`. Only when an operator explicitly opts in (because they DO sit behind a trusted L7) should `connection_info()` be consulted. Bonus: also affects the subscription affinity fallback in `subscription_affinity_key` (same file, lines 197-202).

### H2. Gateway and worker control-plane HTTP fetches have no per-request timeout
**File:** `crates/gateway/src/sync.rs:111-152`, `crates/worker/src/sync.rs:400-430`

**Severity rationale:** `gateway/sync.rs::http_get` is a hand-rolled TCP+writeall+read loop with no `compio::time::timeout`. `worker/sync.rs::http_get_bytes` uses `cyper::Client` but does not set a request timeout (and the per-thread `CONTROL_CLIENT` is constructed via `cyper::Client::new()` with default — i.e. none). If the control plane is slow (overloaded PG, deadlock, GC pause in a managed PG) the gateway/worker reconcile loops block on a single in-flight read forever. The gateway's `sync_once` is awaited inside `start_sync`'s loop, so a single hang prevents the next `interval` tick from running — including route updates. The worker's `version_poll_loop` blocks on the same shape; on-demand load (`fetch_app_version`, `fetch_app_env`) called from `dispatch` also has no timeout, so an unloaded-app request can hang on the worker for as long as control is unresponsive (and the gateway-side `WORKER_TIMEOUT` of 30 s then surfaces as a 502 to the user).

**Reproducer:**
1. Run a control instance behind a TCP proxy that accepts the connection but never returns a body.
2. Start a gateway pointed at it.
3. Run `curl <gateway>/health` — succeeds.
4. Hit `tcpdump` on the gateway → control link: see one in-flight GET `/internal/routes` permanently pending.
5. After deploying a new app, the gateway never sees the new route (reconcile is hung on the previous fetch).

**Suggested fix:** Wrap both `gateway/sync::http_get` and `worker/sync::http_get_bytes` calls in `compio::time::timeout(CONTROL_TIMEOUT, ...)` with `CONTROL_TIMEOUT ≈ 10s` (or whatever fits the existing 5 s `poll_interval`). Same for `fetch_app_version` / `fetch_app_env` on the worker hot path. A simple circuit breaker (open after N consecutive timeouts, half-open after M seconds) would prevent a thundering-herd of hung connections when control comes back up; not blocking but worth a follow-up.

### H3. Empty `--control-key` silently allowed on workers/gateways at startup (no fail-closed when control's key is set)
**File:** `crates/worker/src/main.rs:62`, `crates/gateway/src/main.rs:52`, `crates/control/src/main.rs:178-180`

**Severity rationale:** Control's main.rs refuses to boot without `CONTROL_KEY` outside `--dev-insecure` (good). But neither worker nor gateway validates that `--control-key` is set or non-empty before booting. If an operator only sets the key on control and forgets it on worker/gateway, every reconcile call is sent without an `Authorization: Bearer …` header. `control::internal::check_auth` will then 401 every call, but the worker/gateway will log a generic "HTTP error: 401 Unauthorized" once per interval and otherwise carry on serving with stale routes / version maps (and on cold start the worker will return 503 "failed to load app: HTTP error: 401" for every previously-unseen app, while the gateway will keep serving the LAST-known routes forever because `sync_once` returns Err but doesn't clear the cache). The end-state is "the platform looks healthy but is silently degraded and has no auth on what it would have called if the keys matched."

**Reproducer:**
1. `CONTROL_KEY=secret zeroship-control ...`
2. `WORKER_KEY=wk zeroship-worker --control http://control:9090   # control-key omitted`
3. Logs: `WORKER_KEY not set` warning is absent (worker key IS set), control-key absence is silent.
4. Reconcile loop logs `worker-sync: poll error: HTTP error: 401 Unauthorized` once per 5 s — but the worker keeps serving cached apps and 503s cold loads. Operator may not notice for hours.

**Suggested fix:** In worker and gateway `main.rs`, mirror control's posture: refuse to boot when `control_key` is empty unless an explicit `--dev-insecure` flag is passed. Same for `worker_key` on the gateway — gateway and worker MUST agree, and the gateway should refuse to start without one if it intends to talk to a non-loopback worker. (Worker already enforces the loopback-vs-key constraint; gateway should refuse the symmetric "I have non-loopback workers but no key.")

### H4. Builder derives `userId` from an unverified JWT body after OAuth exchange
**File:** `apps/zeroship-builder/src/server/http.ts:106`, `apps/zeroship-builder/src/server/oauth.ts:99-111`

**Severity rationale:** After `exchangeCode` succeeds the builder calls `subjectFromAccessToken(tokenResponse.access_token)`, which `base64url`-decodes the second JWT segment and reads `sub` WITHOUT verifying the signature, audience, issuer, or expiry. The resulting string is then used as the persistence key in `saveTokens(userId, ...)` and as the value of the long-lived `BUILDER_USER_COOKIE` (90 days, signed via `signUserCookie`). The exchange-time call is reached over HTTPS to hydra so a network-level attacker can't directly forge it; but the code shape is wrong:
- If hydra ever issues a token with an attacker-controlled `sub` for a different identity provider (e.g. a misconfigured client), the builder will sign+set a long-lived cookie that pins the attacker to the victim's `userId`.
- The pattern propagates: any future refactor that calls `subjectFromAccessToken` on a freshly-supplied bearer (from a request, not a token-endpoint response) will silently accept forged identities.

**Reproducer:** In a unit test, hand-craft a JWT where the body is `{"sub":"attacker"}` and the signature is garbage. `subjectFromAccessToken` returns `"attacker"` (`oauth.ts:99-111` never touches the signature segment).

**Suggested fix:** Replace `subjectFromAccessToken` with a verifying call. Options: (a) call `userinfo` on hydra with the access token and read `sub` from the verified response; (b) verify the JWT locally using hydra's JWKS; (c) introspect the access token via `/oauth2/introspect`. Option (b) is closest to the current code's intent — keep the function but reject when the signature/expiry/audience checks fail. Add a regression test exercising the unsigned-JWT case.

## MEDIUM

### M1. `validate_control_key` length-mismatch path leaks the provided-key length via timing
**File:** `crates/core/src/auth.rs:18-38`

**Severity rationale:** The constant-time XOR-fold loop iterates `min_len = min(provided.len(), expected.len())` bytes. When the attacker controls `provided`, work is proportional to `min(|provided|, |expected|)`. With `|expected|` fixed (the operator's control_key length is constant per-deployment), an attacker measuring response time across requests can confirm `|provided| >= |expected|` vs `|provided| < |expected|`. The comment claims "the expected key length is not secret"; that's true, but the function as written DOES leak whether the provided key is at least as long as the expected one — a weak distinguisher, not a key-recovery primitive (HMAC outputs aren't predictable byte-by-byte), so this is M not H.

**Reproducer:** Microbenchmark: time `check_auth(req_with_bearer_of_length_k)` for `k=1, 16, 32, 64, 128`. The 1-byte case is markedly faster than the others when `|expected|=32`.

**Suggested fix:** Always iterate the longer of the two slices, padding the shorter side with zero bytes for the XOR. The result `diff` still folds correctly when `len_ok=false` because the explicit `len_ok && diff == 0` gate produces `false` regardless. Better: use `subtle::ConstantTimeEq` (already a dependency — `crates/sandbox/src/auth.rs:25`) which handles this correctly and reads cleaner.

### M2. Gateway forwards the inbound JSON envelope's `headers` array to V8 verbatim, including `authorization` and `zeroship-user` if the client sets them
**File:** `crates/gateway/src/router/dispatch.rs:1093-1098`, `crates/gateway/src/proxy.rs:202-234`

**Severity rationale:** `handle_dispatch` collects every inbound header (`for (name, value) in req.headers()`) and packs them into the JSON envelope's `headers` field. The worker passes this array to `call_fetch_handler_with_user` (`crates/worker/src/handler.rs:227-237`), which presents them to V8 as `request.headers`. So a creator's JS handler reading `req.headers.get("zeroship-user")` or `req.headers.get("authorization")` sees whatever the END CLIENT sent — NOT the gateway-trusted ZeroShip-User value. The worker's `verified_user_json` correctly sources the trusted user from the OUTER HTTP request (set by `build_request`'s `ZeroShip-User: {user_header}` line) and stashes it in `RequestCtx` for `zeroship.auth.getUser()`, so the AUTH primitive is safe. But any creator app that reaches for `req.headers.get("zeroship-user")` directly will see attacker-controlled JSON. Not exploitable via the auth primitive; IS exploitable against any creator who built a "fallback to header" pattern around the documented header name.

**Reproducer:** Deploy an app whose fetch handler reads `req.headers.get("zeroship-user")` and parses it as JSON. End user sends `curl -H "ZeroShip-User: {\"id\":\"usr_admin\"}" https://myapp.zeroship.ai/protected` — the inner JSON reaches the handler unmodified, while the SIGNED outer header (added by `build_request`) sits in a separate slot the handler doesn't see unless it calls `zeroship.auth.getUser()`.

**Suggested fix:** In `handle_dispatch` (`crates/gateway/src/router/dispatch.rs:1093`), strip the following headers from the envelope before forwarding: `zeroship-user`, `authorization`, `x-app-id`, `x-plan-id`, `x-request-id`, plus `cookie`'s `__Host-zs_app_session` / stash cookies (the worker should not see the session cookie; only the resolved user). The auth primitives stay correct because the gateway adds the trusted ones back via `build_request`'s explicit header lines.

### M3. Usage-report endpoint accepts arbitrary `worker_id` and `(app_id, delta)` pairs with no per-app authorization
**File:** `crates/control/src/internal.rs:152-180`, `crates/core/src/types.rs:80-93`

**Severity rationale:** `report_usage` gates on the control-key shared secret, then trusts every `(app_id, AppUsage)` row in the body. The `worker_id` field is stored but never checked against a registered worker list, and there is no rate / size cap on each delta. A buggy or malicious worker holding the control-key (or any operator who exfiltrates it) can write arbitrary usage to ANY app, including spiking a free-tier app over its quota or zeroing-out a paid app's metering. The `as i64` cast (line 164-168) means `u64::MAX` becomes negative and silently skips, but anything up to `i64::MAX` is accepted.

**Reproducer:** With control_key in hand, `curl -H "Authorization: Bearer $key" -d '{"worker_id":"forged","counters":{"<victim-app-uuid>":{"requests":9223372036854775000,"cpu_us":0,"wall_us":0,"egress_bytes":0,"ingress_bytes":0}}}' https://control/internal/usage`. The victim's usage row in PG bumps by ~10^18 requests.

**Suggested fix:** Two layers. (1) Require a per-worker identity, not just the shared control-key: have workers sign each report with an Ed25519 per-worker key registered with control, and have control authorize each `(worker, app_id)` against a "this worker is hosting this app" predicate (the version map already tracks which workers should be running which apps via CHWBL — the routing entry can encode the expected worker set). (2) Cap per-call delta size (`delta_max = plan_quota * margin`) so a single bad call can't blow past the plan's monthly budget. The shared-secret design makes (1) a bigger change; (2) is a one-line guard.

### M4. Worker's `verified_user_json` re-uses `worker_key` as the HMAC key, so an empty worker-key in dev disables BOTH the bearer and the user-header check at the same time
**File:** `crates/worker/src/handler.rs:48-76`, `crates/worker/src/main.rs:76-89`, `crates/core/src/auth.rs:99-118`

**Severity rationale:** `check_worker_auth` early-returns OK when `worker_key.is_empty()` (commented as "dev-only loopback bind enforces this"). `verified_user_json` then HMACs the user header with `worker_key.as_bytes()` — if worker_key is empty, the HMAC reduces to `HMAC-SHA256(b"", payload)` which any caller can compute. So a localhost dev worker bound on 127.0.0.1 accepts arbitrary user identities from any caller on the same host. The loopback bind is correct mitigation against the network attacker, but it means any OTHER process on the dev machine (a browser extension, a VS Code task, a misbehaving Vite plugin) can mint user identities and call the dispatch endpoint. Production guard (`main.rs:82-87`) prevents non-loopback bind without the key, so this is dev-only — but `cache::load_app` runs UNTRUSTED CODE so even on dev this is the difference between "any local process can run code as any user in any app" and "only the gateway can".

**Reproducer:** In dev (`WORKER_KEY=""`, bind 127.0.0.1), from any process on the box: `curl -H "ZeroShip-User: <forged HMAC w/ empty key>" -H "X-Request-Id: <uuid>" -X POST -d '{...}' http://127.0.0.1:8080/dispatch/<app-uuid>` — accepted, and the runtime's `zeroship.auth.getUser()` returns the forged identity.

**Suggested fix:** Decouple the bearer check from the HMAC key. When `worker_key.is_empty()`, refuse to honour any inbound `ZeroShip-User` header (return `Ok(None)`) instead of HMAC-verifying it against an empty key. Devs lose the ability to test as a specific user from `curl` in fully-no-auth mode, but they can either set a `WORKER_KEY` or get the user from a request body field. Better: make `WORKER_KEY` REQUIRED for any user-header processing and document the dev story as "unset → ignore ZeroShip-User entirely; set → enforce HMAC".

### M5. Sandbox preview WS authorizes on client-controlled `?user_id=` query parameter
**File:** `crates/sandbox/src/preview_ws.rs:166-191`, `crates/sandbox/src/preview_ws.rs:548-558`

**Severity rationale:** Sandbox preview WebSocket auth pairs the bearer token (operator-shared, constant per-deploy) with `?user_id=…` from the query string, and authorizes by comparing `Some(&info.user_id) == user_id.as_ref()`. The user_id is plaintext + unsigned; ANY caller with the operator bearer can claim to be ANY user. The preview share-cookie path is HMAC'd correctly; only the bearer path has this shape. Acceptable IF the bearer is operator-internal (and `SANDBOX_TOKEN`'s startup guard treats it that way — `crates/sandbox/src/auth.rs:9-21`), but the `?user_id=` design makes accidental token leakage convert directly into a "any user → any sandbox" privilege.

**Reproducer:** A builder server compromised at any depth can forward the operator bearer to a third party who then connects to `ws://controller/sandboxes/<id>/preview/<port>/?user_id=<owner>` and gets a working preview WS for someone else's sandbox.

**Suggested fix:** Require a signed user assertion (the same `ZeroShip-User` header pattern the gateway → worker leg uses, signed with a controller↔caller shared HMAC key) for the bearer-auth path. The share-cookie path already has proper binding; the bearer path should be brought in line. Pre-launch this is a single-day refactor.

## LOW

### L1. `worker_id` field on `UsageReport` is recorded but never authenticated; debugging signal only
**File:** `crates/core/src/types.rs:81`, `crates/control/src/internal.rs:152-180`

**Severity rationale:** Tied to M3 — separately called out because operators may believe `worker_id` is a security signal (it isn't; it's free-form attacker-controlled metadata).

**Suggested fix:** Rename to `worker_id_hint` in the wire type, document as "informational only," and never include it in security-sensitive logs without prefixing it `untrusted/`. Or just delete the field (it's unused on the read side — `report_usage` doesn't touch it).

### L2. Builder OAuth-store encryption secret has no minimum-length / entropy enforcement
**File:** `apps/zeroship-builder/src/server/oauth-store.ts:142-155`

**Severity rationale:** `BUILDER_TOKEN_ENCRYPTION_KEY` is fed straight into HKDF without a length check. A 1-byte secret produces a key that's nominally 32 bytes but with very low entropy; the AES-256-GCM ciphertext is then vulnerable to offline guessing on any builder-state dump. The fixture envs in dev/CI likely use a short literal — easy to leak into prod via a `.env` copy.

**Reproducer:** Set `BUILDER_TOKEN_ENCRYPTION_KEY=x`. The builder starts cleanly and saves tokens. An attacker who gets `persistGet` output can crack the master in seconds (`x`, `xx`, `password` …).

**Suggested fix:** In `deriveEncryptionKey`, throw if `secret.length < 32` (or `secret` doesn't decode to 32 random base64url bytes). Mirror the gateway / control posture (`validate_gateway_stash_key`, `validate_master_key_material`).

### L3. `sync_once` failure doesn't clear the gateway's route cache, so route deletes silently lag during a control-plane outage
**File:** `crates/gateway/src/sync.rs:102-109`

**Severity rationale:** `sync_once` returns Err on any HTTP/parse failure and `start_sync` just logs+continues. The RouteCache retains the last-known map indefinitely. If an app was deleted from control DURING the outage window, the gateway keeps routing to it (eventually 502s when the worker drops the app from its own cache, but the gateway-side stale-route window is unbounded). Not a confidentiality issue — the app is being deleted, not added — but a creator who DELETES an app expecting the cookies / endpoints to stop working will be surprised. Pre-launch this is just a documented limitation.

**Suggested fix:** Two reasonable shapes. (a) Add a TTL on the route cache — entries become stale after N missed reconciles. (b) Have control include a generation counter in `/internal/routes` and have the gateway log loudly when generation jumps backwards on recovery (so operators see "routes might be stale"). Neither is critical pre-launch.

## Subsystems audited but no findings

- **JWT issuer/audience consistency.** PAT (`crates/control/src/token_handlers.rs:27-28`: iss=`https://api.zeroship.ai`, aud=`control.zeroship.ai`, typ=`pat+jwt`) vs Wrapper (`crates/gateway/src/wrapper_token.rs`: iss=`{public_url}`, aud=`{app}.zeroship.ai`, typ=`at+jwt`) cannot be confused; verifiers pin all three of `typ`, `iss`, `aud`. Sharing the Ed25519 signing key between PAT and Wrapper is documented design (`crates/control/src/lib.rs:149-152`), not a finding.
- **Cookie scoping.** Three distinct names — `__Host-zsidp_session` (auth), `__Host-zs_console_session` (control), `__Host-zs_app_session` (gateway, per origin), plus `__Host-zsbx_share_<sbx>` (sandbox share) and `__Host-zs_oidc_stash` (gateway) / `__Host-zs_console_stash` (control). All carry `Path=/; HttpOnly; SameSite=Lax|Strict; Secure`, no Domain attribute. The `__Host-` prefix correctly enforces cross-origin isolation; dev mode strips both the prefix and `Secure`.
- **CORS.** `crates/gateway/src/router/cors.rs` correctly refuses `allow_origins=["*"]` whenever `allow_credentials=true`; the actual-response injector mirrors the preflight gates. No wildcards are baked into the gateway's own surface.
- **Request-ID propagation.** `Uuid::new_v4` is minted at the gateway's auth gate (`crates/gateway/src/router/dispatch.rs:496`), bound into the ZeroShip-User HMAC (`crates/core/src/auth.rs:99-109`), forwarded as `X-Request-Id` (`crates/gateway/src/proxy.rs:393`), and the worker's `verified_user_json` cross-checks the HMAC against the same value. End-to-end binding is correct.
- **Wire-format compatibility.** `RouteEntry`, `AppRecord`, `AppVersionInfo`, `UsageReport` all carry `#[serde(default)]` on the new fields (`env_version`, `manifest`) so a producer rolling forward won't break an older consumer mid-rollout. The pre-launch posture means this isn't required, but it's done anyway.
- **DPoP wrapper-token binding.** `crates/gateway/src/router/auth.rs:184-359` — wrapper-token path correctly enforces `cnf.jkt == proof.jkt`, rejects empty `sub`, refuses to fall through to introspection on a wrapper-shaped-but-bad token, and checks `wrapper_revoked_subjects`. Strong shape.
- **Sandbox controller→agent signing.** `crates/sandbox-agent/src/sig.rs:503` uses `verify_strict` (rejects malleable signatures), with per-sandbox Ed25519 keys minted on registration and AEAD-sealed on disk. Replay defence: nonce-and-timestamp on every request, fresh `(ts, nonce, sig)` per retry per `preview.rs:275-285`.

## Not fully audited

- `crates/control/src/audit.rs` — audit-log shipping. Touched at every authz_guard boundary but not traced end-to-end.
- `crates/control/src/backchannel_logout.rs` and `crates/gateway/src/backchannel_logout.rs` — back-channel logout fan-out completeness (does every gateway see every logout?). The `logout_jti_cache` is in-process per binary; a multi-binary deployment might double-process or drop.
- `crates/auth/src/mailer/` and `crates/auth/src/identity/oauth/` — federation providers' callback flows.
- `crates/sandbox/src/backend/nomad_ch.rs` — full Nomad+CH dispatch path.
- `crates/cli/` — `zeroship deploy` and `zeroship inspect`: how they talk to control during a deploy (auth, retry, error mapping).

## Status

- HIGH drained in `audit/r10-integration`:
  - H1 fixed by `a52b485f` (`fix(gateway): gate proxy client IP trust`).
  - H2 fixed by `688ec523` (`fix(sync): timeout control-plane fetches`).
  - H3 fixed by `68c08cef` (`fix(startup): require control key outside dev`).
  - H4 fixed by `6024b6f2` (`fix(builder): verify oauth access token subject`).
- MEDIUM / LOW findings remain open for a follow-up drain.
