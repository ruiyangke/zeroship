# Control-Plane Core — Security + Correctness Review (2026-07-27)

Scope: `crates/zeroship-control/src/{api,registry,lib,main,token_handlers,device_handlers,
oauth_handlers,oauth_grants_handlers,app_oauth_client,env_store,env_handlers,
authz_guard,internal,rate_limit,http_util,audit,identity_bridge,account_status}.rs`
plus the supporting `crates/zeroship-authn/src/lib.rs`, `crates/zeroship-authz/src/{eval,entities}.rs`,
`crates/zeroship-core/src/crypto.rs`.

Focus per the brief: tenant isolation / IDOR, secret-store crypto, internal-API
auth, token minting/verification, SQL injection, rate limiting. Billing/metering/
Stripe/pricing deprioritized.

**Bottom line: the auth + tenant-isolation + secret core is well-defended.** No
CRITICAL or HIGH exploitable finding was confirmed. The prior cross-tenant
secret-decrypt finding class is closed (AAD binds `(app_id, key_name)`). The
findings below are one MEDIUM fail-open ergonomic hazard, plus MEDIUM/LOW
hardening and availability gaps. Production-grade for the reviewed surface.

---

## What was verified sound (evidence)

- **Tenant isolation / IDOR — enforced structurally.** Every app-scoped handler
  (`get_app`, `delete_app`, `deploy`, `set_var`/`set_secret`/`delete_*`,
  `list_secrets`, `set_expose`, `set_plan`, `set_spend_limit`, billing reads,
  logs) calls `authz.require(<Action>, Resource::App { id }, …)` BEFORE any
  read/write (`api.rs:276,356,396`; `env_handlers.rs:82,109,142,179,199,278,365`).
  `authz::enforce` (`eval.rs:38`) resolves the principal's `app_members`-bound
  Cedar entities (`entities.rs:116`, default role `"none"` = zero-priv,
  `entities.rs:153`), so a principal with no membership row on app *B* is denied.
  No path found where principal A acts on app B.
- **`list_apps` data-scopes correctly.** The broad `apps:read` gate passes for
  every creator, but the DATA is re-scoped: only fleet-wide roles
  (`admin/readonly/support/billing`) call `list_apps()`, everyone else gets
  `list_apps_for_owner(principal_id)` (`api.rs:221-225`). Prevents the C1 list
  leak.
- **PAT privilege model — subset-only, no escalation.** `enforce` first requires
  the OWNER be allowed against static policies (`eval.rs:46-54`), THEN applies the
  token's narrowed policy loaded fresh from DB with revocation+expiry re-checked
  (`eval.rs:56-62,146-163`). A PAT can only ever be ⊆ the owner's authority.
  Mint-time `validate_grant_subset` (`token_handlers.rs:266`) blocks granting a
  permission the principal lacks; PATs cannot mint PATs (`token_handlers.rs:69`).
- **PAT verification** gates on `id + owner_id + policy_hash + kind='pat' +
  revoked_at IS NULL + expires_at > NOW()` (`authn/src/lib.rs:226-233`). The
  `policy_hash` binding means a rehashed/narrowed policy invalidates the old token.
- **OAuth bearer path** checks audience (`authn:336-342`), family-revocation
  (`authn:293-309`), and translates scope→policy as an upper bound
  (`authn:352`). GoTrue-role tokens must be `"authenticated"` and resolve via
  `identity_links` (`authn:355-366,380-404`).
- **Secret-store crypto — cross-tenant/cross-key decrypt prevented.** AES-256-GCM
  with AAD = `"zs:control:app_secret:v1\0" || app_id(16) || 0 || key_name`
  (`env_store.rs:79-86,279,364,509`). A ciphertext copied into another app's row
  fails GCM verification because the app_id/key differ. Wire format is versioned
  (`crypto.rs` `AAD_V1`), random 12-byte nonce per encrypt (`crypto.rs:75`),
  strict UTF-8 on decrypt (no lossy replacement), keys zeroized on drop
  (`env_store.rs:109-116`). Device-grant token encryption similarly binds AAD to
  `device_code_hash` (`device_handlers.rs:494`).
- **Internal API** requires a control-key bearer even when `control_key` is empty
  — empty key does NOT fail open (`internal.rs:29-34`); prod startup refuses to
  boot without `CONTROL_KEY`/`MASTER_KEY`/`SIGNING_KEY_FILE` (`main.rs:864-883`).
- **No SQL injection.** Every query in the reviewed files uses bound parameters.
  The only `format!` reaching SQL is `"{DEVICE_TTL_SECS} seconds"` bound as a
  `$4::TEXT` param with a compile-time const (`device_handlers.rs:132`) — not
  interpolated identifier/value. The prior CT-A1 `plan_id` class is gone:
  `set_plan` binds `plan_id` as `$1` throughout (`api.rs:742,766,782`).
- **XFF spoofing** defended: `source_ip` ignores `X-Forwarded-For` unless
  `--trust-proxy` and then reads the last (proxy-set) hop (`http_util.rs:26-43`).
- **Identity account-merge** requires server-side `email_verified` from GoTrue's
  admin API (fail-closed to `false` on any error) before binding a provider
  subject to an existing user by email (`identity_bridge.rs:98-116,230-269`).

---

## Findings (ranked)

### MEDIUM-1 — `--dev-insecure` on a non-loopback bind only warns; full auth bypass if misconfigured
`crates/zeroship-control/src/main.rs:1524-1530` · `crates/zeroship-control/src/internal.rs:15-17`

`check_auth` returns `None` (allow) unconditionally when `state.insecure_dev` is
true (`internal.rs:16`), disabling ALL `/internal/*` auth — including
`GET /internal/apps/:id/env`, which returns every app's **decrypted secrets**.
The admin `AuthzGuard` is likewise effectively neutralized in dev. The only guard
against exposing this on a network is a **`tracing::warn!`** at `main.rs:1524`
when `insecure_dev && bind_host ∉ {127.0.0.1, ::1, localhost}` — the process
still binds and serves.

Attack/failure: an operator who runs `--dev-insecure` (or sets
`ZEROSHIP_DEV_INSECURE=1`) and binds `0.0.0.0`/a routable address — a plausible
copy-paste from a dev compose file into a shared/staging host — exposes a fully
unauthenticated control plane that hands out decrypted per-app secrets to anyone
who can reach the port. A log line is trivially missed.

One-line fix: make it fatal — `if insecure_dev && bind_host is non-loopback {
eprintln!(...); std::process::exit(1); }` (dev-insecure MUST be loopback-only).

### MEDIUM-2 — Deploy + device-flow endpoints have no rate limiting
`crates/zeroship-control/src/api.rs:380` (deploy) · `crates/zeroship-control/src/device_handlers.rs:103,163` (`device_auth`, `device_approve`)

`api.rs` never calls `http_util::rate_limit`/`admin_rate_limit` — only
`env_handlers` and the Stripe handlers do. So `POST /api/apps/:id/deploy`,
`create_app`, and the whole app-CRUD surface are unthrottled beyond Cedar authz.
`device_auth` (which INSERTs a `device_grants` row per call) and `device_approve`
are also unthrottled. `device_token` self-throttles via the RFC-8628
`slow_down`/`POLL_INTERVAL_SECS` gate (`device_handlers.rs:383-399`), but the
grant-minting and approval endpoints do not.

Attack/failure: an authenticated-but-hostile (or credential-stuffed) principal
can spam `device_auth` to churn grant rows, or hammer deploy to exhaust tmp-disk
/ ingest CPU. Not a privilege bypass, but a per-tenant DoS + audit-noise vector
the admin surface is otherwise protected against.

One-line fix: wrap `deploy`, `create_app`, and `device_auth`/`device_approve`
with the existing `http_util::rate_limit` (a dedicated `deploy`/`device`
namespace + quota).

### MEDIUM-3 — Rate limiter fails open when the source IP cannot be resolved
`crates/zeroship-control/src/http_util.rs:56-59`

`rate_limit` returns `None` (allowed) when `source_ip` yields `None` or the string
fails to parse as an `IpAddr`. With `trust_proxy=false` (the default), `source_ip`
depends solely on `req.peer_addr()`; any request whose peer address is absent
(certain proxy/UDS front-ends, or the ntex quirk noted in the test at
`http_util.rs:132-140`) bypasses the DB-backed limiter entirely.

Attack/failure: if the control plane is fronted by a Unix-socket or a front-end
that doesn't surface a peer IP and `--trust-proxy` is off, per-IP limits silently
no-op on every request — the exact throttle the admin/webhook endpoints rely on
is disabled without any signal.

One-line fix: fail CLOSED (or fall back to a single shared bucket) when the client
identity can't be resolved, rather than returning `None`.

### MEDIUM-4 — Audit-log write failure is swallowed after the mutation commits
`crates/zeroship-control/src/audit.rs:170-171,196-197` · callers in `env_handlers.rs`
(`set_secret`, `delete_secret`, `set_var`, `set_expose`)

`audit::log` logs a `warn!` and returns `()` on connect/insert failure; the
secret/var mutation has already committed (`env_store.set_secret` runs first, then
`audit::log` best-effort). The audit trail is documented "append-only … every
mutation … writes one row," but a DB hiccup on the audit insert produces a
committed secret change with **no durable audit row**.

Failure: undermines the security-audit guarantee — a secret can be set/rotated/
deleted without a persisted record. Not exploitable for access, but defeats
forensic attribution ("when did we let X out of the secret namespace").

One-line fix: write the audit row in the SAME transaction as the mutation (or at
minimum surface a 500 so the client knows the audited write was not durably
recorded).

### LOW-1 — Single platform master key (no per-app HKDF); master compromise leaks all apps
`crates/zeroship-core/src/crypto.rs:53-69` (`derive_key`) · `crates/zeroship-control/src/env_store.rs:91,147`

`derive_key` is `SHA-256("zeroship-secret-key-v1" || master)` — one key for ALL
apps. AAD prevents cross-tenant decrypt *within* the platform, but a master-key
compromise decrypts every app's secrets at once (the code already documents this
as a "planned G-track hardening"). Noted here so it is tracked, not because it is
newly introduced.

Fix (tracked): expand per-app with HKDF mixing `app_id` into the key, bounding
blast radius to one app on key exposure.

### LOW-2 — `merged_env_for_worker` 500s the entire env fetch on one un-decryptable secret
`crates/zeroship-control/src/env_store.rs:365,510` (`?` on `decrypt_with_keys`)

If a single stored ciphertext fails to decrypt (corruption, a key dropped from the
rotation set before `rotate_app` drained it), the `?` aborts the whole
`merged_env`/`merged_env_for_worker`, so the worker gets a 500 and the app boots
with NO env at all — one poisoned secret takes down every deploy of that app.

This is correct fail-closed for confidentiality but a sharp availability edge.
Consider surfacing which key failed and gating rotation drain on a verified
re-encrypt count so `previous_keys` is never dropped early. Low severity: only
reachable via operator rotation error or DB corruption.

### LOW-3 — `device_approve` discards the CSRF field it accepts
`crates/zeroship-control/src/device_handlers.rs:57-61,175`

`DeviceApproveRequest.csrf` is deserialized then bound to `_csrf` and never used;
the comment defers CSRF/origin hardening to the auth-service browser page. This is
acceptable IF the auth page is the sole caller and enforces origin, but the field
being present-yet-ignored invites a false sense of protection and lets any holder
of a valid `authenticated` GoTrue bearer approve any pending `user_code` directly
against control (bypassing the browser page). Confirm the auth page is the only
sanctioned path and that control is not directly reachable by app-scoped bearers;
otherwise enforce CSRF/origin here too.

---

## Notes / non-issues checked

- `decrypt_with_keys` iterates all keys without early return for constant-time
  behavior across rotation state (`crypto.rs:101-126`) — reasonable, and each
  discarded plaintext is zeroized.
- `set_expose` replaces the list in a two-statement transaction on an owned
  connection (`env_store.rs:406-449`) — no torn read.
- `revoke_grant_cascade` runs the grant DELETE + alias revoke + family-marker in
  ONE txn on an owned connection, scoped to `authz.principal_id`
  (`oauth_grants_handlers.rs:170-232`) — no IDOR, no interleave on the shared
  pipelined handle.
- `oauth_handlers` client CRUD is operator-gated (`PlatformPoliciesWrite` on
  `Resource::Any`) with strict redirect-URI validation (https or loopback-http,
  no fragment, length-capped) (`oauth_handlers.rs:277-318`).
- `recent_for_app` clamps `limit` to `1..=500` (`audit.rs:207`).
- Error bodies for `Db`/`Crypto` are genericized to the client; raw SQLSTATE/
  crypto internals only hit stderr (`env_handlers.rs:52-60`).
