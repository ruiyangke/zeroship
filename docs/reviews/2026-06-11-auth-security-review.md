# Zeroship Auth & Security Review — 2026-06-11

> **Scope.** Whole-project review weighted to **auth & security**, run at `main @ d0436968`.
> Five parallel finder agents (auth-idp, gateway-auth, control-plane+authz,
> tenant-isolation+data-plane, runtime+builder), each adversarially self-verifying
> its critical/high findings and deduping against the two prior ledgers
> (`docs/archive/reviews/2026-06-03-codebase-security-audit.md` and
> `docs/reviews/2026-06-09-security-review.md`). The orchestrator independently
> re-verified the one new HIGH against source before filing.
>
> **This is a review only — no code was changed.** Remediation is a follow-up.

## 1. Executive summary

The 2026-06-09 SEC-1..10 remediation **landed on `main` and every fix holds at HEAD**
with no regression — including the crown-jewel SEC-1 cross-tenant transaction-hijack fix
(verified complete across the PG *and* SQLite paths; a residual interleave could not be
constructed) and the SEC-4/5/6/7/9/10 set. The core auth IdP lane is exceptionally
hardened: all three items the prior review flagged for a "dedicated look" (mailer
CRLF-in-Subject injection, account-linker takeover, hydra-admin SSRF) were **refuted with
concrete guard evidence**, and two ledger items previously listed as open — **L5 (account
lockout) and H1 (password-reset session termination) — are in fact already fixed** at HEAD.

The risk that remains is concentrated in two seams, both **known and pre-existing**, not
newly introduced:

1. **The control-plane `CT-*` class never merged.** The 2026-06-09 review noted the
   2026-06-04 control remediation branch was commit-only and unpushed; this review confirms
   **all 9 CT-* items + 2 lows are still open at HEAD**. The standout, **CT-A1 (plan
   self-escalation), is worse than recorded** — the authz engine *actively permits* a
   creator to set their own app's plan to `unlimited`, which nulls cpu/wall/heap limits, and
   **chains to RT-5** to yield an uncapped, unpreemptable worker thread (confirmed end to end).
2. **A new HIGH in the gateway fail-closed RPC default (NEW-1)** reopens the exact SEC-5
   class for an expressible, validator-blessed shape: a `*` web catch-all declared
   `auth: anon` silently suppresses the e919af51 fail-closed flip for every undeclared
   `rpc:` procedure in the app.

Result: **1 new high (NEW-1), 1 new medium (NAT64 SSRF, raised from a prior unverified-low),
1 new low (KV-3)**; the entire `CT-*` class reconfirmed open; the SEC-1..10 + driver fixes
reconfirmed holding; several ledger calibrations corrected (L5/H1 closed, RT-3 partial,
P2-B6/B7 downgraded).

## 2. New findings

### NEW-1 — HIGH — `*` catch-all with `auth: anon` defeats the RPC fail-closed default (reopens the SEC-5 class)
- **File:** `crates/gateway/src/compiled.rs:449-525` (`resolve_effective_policy`) + `:555-586` (`build_inheritance_chain`); enabling shape `crates/bundle/src/manifest.rs:304-313` (`Manifest::passthrough`).
- **Description:** `build_inheritance_chain` unconditionally prepends `*` to the chain of **every** key, including `rpc:` keys (compiled.rs:563-565, before the rpc-segment branch). In the resolve loop, the moment *any* ancestor declares `auth`, `auth_declared = true` (compiled.rs:453-454). The e919af51 fail-closed default only fires `if !auth_declared && key.starts_with("rpc:")` (compiled.rs:523). So when an app declares a root `*` resource with `auth: anon` + `publicly_accessible: true` — the normal public-web-surface shape, **and exactly what `Manifest::passthrough()` (every raw-JS deploy) emits** — every RPC procedure that doesn't declare its own `auth` inherits `auth_declared = true` from `*`, keeps the initial `AuthLevel::Anon` (anon rank 0 is not `>` anon rank 0, and the `override` branch needs `is_self`), and the fail-closed flip never runs. The procedure resolves to **`Anon` = unauthenticated**.
- **Attack:** A creator ships a public site with a `"*": { auth: "anon", publiclyAccessible: true }` catch-all (landing page / SSR / static) and writes `query("getOrders", …)` without an explicit `auth`, trusting the documented "RPC is authenticated-by-default" guarantee. The procedure is silently world-callable at `POST /__zeroship/v1/getOrders`. Identical class to SEC-5 (a forgotten/mistyped policy → unauthenticated exposure), reopened by a benign-looking parent.
- **Orchestrator-verified:** Confirmed against source: passthrough emits `*` = `auth: Some(Anon)` + `publicly_accessible`; the chain-builder prepends `*` to rpc keys; the flip is gated on `!auth_declared`. **Calibration:** the builder console (`apps/zeroship-builder/src/server/config.ts`) declares no `*` and is **not** affected; the example apps use React-Router `*` (not a manifest resource), so this is **not an every-app break**. It hits (a) every raw-JS / passthrough deploy unconditionally, and (b) any app that declares the blessed `*`+anon web shell alongside undeclared RPCs. HIGH as a latent platform-wide footgun that silently reopens the very class e919af51 was written to close.
- **Suggested fix:** Drive the fail-closed signal off the rpc procedure's **own** chain, excluding the global `*` web catch-all: track `rpc_auth_declared` ignoring the `*` ancestor, or simply don't inject `*` into `rpc:` chains (the web catch-all and the RPC API surface are disjoint namespaces). Add a regression test: `*` = `{auth:anon, publicly_accessible:true}` + `rpc:foo` (no auth) must resolve `rpc:foo` → `User`.

### NEW-2 — MEDIUM — SSRF blocklist misses NAT64 (`64:ff9b::/96`) and IPv4-compatible IPv6 → cloud-IMDS reach on IPv6 networks
- **File:** `crates/runtime/src/transport/ssrf.rs:56-65` (`is_blocked_ip` IPv6 arm).
- **Description:** Empirically reproduced against the real `is_blocked_ip`: `64:ff9b::a9fe:a9fe` (NAT64 of `169.254.169.254`, the cloud metadata service) is **not blocked**; `::7f00:1` (IPv4-compatible `::127.0.0.1`) is **not blocked** (`is_loopback()` is false for it); `::a9fe:a9fe` is **not blocked**. The v4 arm correctly blocks link-local/loopback/private, but the v6 arm only handles v4-*mapped* (`::ffff:…`), not NAT64 or v4-*compatible* embeddings.
- **Attack:** On an IPv6-only / NAT64 cloud subnet (common in modern deployments), creator `fetch()` code targets a literal IPv6 NAT64 address that the upstream translates to the link-local IMDS / internal services, bypassing the v4 link-local block and reaching cloud credentials. (Raised from the 2026-06-09 unverified-low to a verified MEDIUM.)
- **Suggested fix:** In the v6 arm, block `64:ff9b::/96` (`segments[0]==0x0064 && segments[1]==0xff9b`); for v4-compatible/v4-mapped embeddings (`segments[0..6]==0`), extract the trailing 32-bit v4 and recurse into the v4 blocklist. `::a.b.c.d` is deprecated/low-reach but cheap to cover.

### NEW-3 — LOW — `kv.list` prefix skips the brace/control-char validation every other KV op applies
- **File:** `crates/plugin-kv/src/v8_class.rs:324` (`list`); `crates/plugin-kv/src/limits.rs:167` (`escape_glob`).
- **Description:** `list()` passes `prefix` to `dispatch_list` without the `validate_key` brace/NUL/control rejection that `get/set/delete/incr/.../persist` all apply. The Redis backend `escape_glob`s the prefix (`backend/redis.rs:335`) but `escape_glob` escapes only `* ? [ ] \ ^` — **not `{`/`}`** — so a brace-bearing prefix lands literally in the `SCAN {<app_id>}:<prefix>*` pattern.
- **Not a cross-tenant bleed (adversarially checked):** the pattern is always anchored at the literal `{app_id}:` (the app's own id, from its own isolate), Redis Cluster hash-tags on the *first* `{…}` group, and routing passes `app_id` explicitly — a forged second brace group lands after `{app_id}:` and cannot escape the keyspace or redirect routing. Realized impact is a malformed/over-narrow MATCH within the caller's own keyspace (self-inflicted), plus a latent footgun if a future change moves prefix interpolation ahead of the hash-tag.
- **Suggested fix:** Apply `validate_key`-style rejection (allowing empty) to `prefix` in `list()`, and add `{`/`}` to `escape_glob` for defense-in-depth.

## 3. Control-plane `CT-*` class — all confirmed STILL OPEN at HEAD

The 2026-06-04 control remediation branch never merged; verified against current code. Fix priority: **CT-A1 first** (authz-permitted self-escalation → resource exhaustion via RT-5), then **CT-B1/B2** (revenue integrity), then the rest.

| Item | Verdict | Evidence |
| --- | --- | --- |
| **CT-A1** plan self-escalation | **OPEN — exploitable, worse than recorded** | `api.rs:85,563` pass `body.plan_id` raw → `registry.rs:160-167`/`341-347` (no allowlist); `registry.rs:531-535` `unlimited`/`enterprise` → cpu/wall/heap = `None`. **Authz permits it:** `set_plan` gates `BillingWrite` on `App{id}`; `policies/creator/app_owner.cedar` grants the owner `action` **unconstrained** (incl. `billing:write`); owner auto-bound at `registry.rs:177`. Chains to **RT-5**. |
| **CT-A2** no deploy rate-limit/concurrency cap | OPEN | `api.rs:292 deploy` has authz + content-type + per-deploy size cap only; no per-creator rate-limit or in-flight cap. |
| **CT-A3/A4** case-variant + reserved app names | OPEN | `registry.rs:144-153` length+charset only; `0004_control.sql:26` `name TEXT UNIQUE` case-**sensitive**; no reserved-name denylist (`console`/`auth`/`api`). |
| **CT-B1** 15% fee not server-enforced; creator_id forgery | OPEN | `stripe_handlers.rs:444` fee = webhook-reported `application_fee_amount`; `:451` creator_id self-attested; `stripe_store.rs:212-236` validates only `0≤fee≤gross`, no 15% floor. SDK `checkout.ts` `applicationFeePercent ?? 15`, caller-settable to 0. |
| **CT-B2** metering blind-add, body app_id, no dedupe | OPEN | `internal.rs:152-179 report_usage` keys on request-body `app_id`, shared control-key; `registry.rs:490-493` blind `value += EXCLUDED.value`, no event-id dedupe. |
| **CT-B3** insecure_dev disables auth/sig; WARN-only bind | OPEN (operator opt-in) | `internal.rs:17-19` bypasses ALL `/internal/*` auth; `stripe_handlers.rs:403-409` skips webhook sig; `main.rs:947` only WARNs on non-loopback bind. |
| **CT-B4** Connect callback links `acct_` without ownership verify | OPEN (staff-gated) | `stripe_handlers.rs:110-138` links `body.stripe_account_id`→creator via `BillingWrite` on `Any` (staff) with no Stripe-API ownership check. |
| **CT-C1** no reserved env-name denylist | OPEN (low impact) | `env_store.rs:67-77` format-only; `DATABASE_URL`/`WORKER_KEY`/`NODE_OPTIONS`/`APP_ID` pass. Impact bounded (APP_ID env-map disjoint from user snapshot; V8 ignores NODE_OPTIONS). |
| **CT-C2** single platform-wide secret key | OPEN (architectural) | One `master_key` for P5 crypto; per-app HKDF roadmapped. |
| **low** inert `platform_policies` override | OPEN | `admin_handlers.rs:333,394` write+audit `zeroship.platform_policies`, but `enforce` always uses the boot-time static `.cedar` set (`main.rs:920`); no DB-merge. Emergency-override lever silently no-ops. |
| **low** CONTROL_KEY no strength floor | OPEN | `main.rs:538` non-empty check only, while master/worker/stash/pairwise enforce ≥32B + dev-sentinel rejection. Online-brute-force from internal network only. |

**Authz engine itself re-verified sound:** default-deny / fail-closed (`authz_guard.rs:83-92`), PAT subset TOKEN⊂USER (`token_handlers.rs:420-492`), self-service scoping (`self_service.cedar` matches only synthetic `Resource::Any`, cross-tenant DENY preserved). Two recorded minors persist, both below the bypass bar: `lower.rs:122-137` multi-CIDR `IpRange` missing outer parens (`A || B && Time` → self-restriction-weakening on the creator's own PAT only); `eval.rs:241-261` `audit_decision` INSERT failure swallowed (forensics gap, not a bypass).

## 4. Confirmed cross-cutting chain

**RT-5 ↔ CT-A1 — confirmed worker DoS.** A creator self-escalates to `unlimited` (CT-A1) → `AppRuntimeLimits.cpu_limit_ms = None` → `cache.rs:206` maps to `cpu_limit: None` → `runtime.rs:1383` never registers the POSIX CPU timer (`arm_cpu_timer` no-op, `check_v8_terminated` early-false). **`wall_timeout` does not backstop a synchronous runaway** — it is consumed only on the async `FetchOutcome::Pending` path (`serve.rs:762`); a tight `while(true){}` inside `fetch()` takes the synchronous `FetchOutcome::Response` path (`serve.rs:743`) with no timeout wrapper, and V8 sync execution can't be preempted without the timer interrupt. Net: a `None`-cpu plan + sync loop hangs a worker thread that LRU-multiplexes many tenants' isolates, zero preemption. Fix: clamp `cpu_limit` to a hard ceiling regardless of plan, **or** add a wall-clock watchdog that calls `terminate_execution` on the sync path too.

## 5. Regression checks — all landed fixes HOLD

| Fix | Verdict | Note |
| --- | --- | --- |
| **SEC-1** cross-tenant tx hijack (`f9609496`) | **HOLDS** | Every tx/savepoint/emit/mig slot app-keyed in `context.rs`; `has_tx_for`/`take_tx_client_for`/etc. filter on owning app; PG **and** SQLite arms fixed; cancellation guards restore to the same app's slot. `app_id` sourced from the real per-isolate `APP_ID` (never attacker-controllable); synchronous `RefCell` borrows can't swap slots across isolates. Residual interleave could not be constructed. Per-app PG role grants backstop as a second layer. |
| **SEC-2** path-traversal auth bypass (`82a68c42`) | **HOLDS** | `canonicalize_dispatch_path` rejects (400) any dot-segment (`.`/`..`/`%2e`, case-insensitive) or empty interior segment; the same canonical string feeds both the auth match and the worker-forwarded URL — no desync. |
| **SEC-4** masked-column plaintext via aggregate (`f692872a`) | **HOLDS** | `$group.by`/`$sum`/`$avg`/`$min`/`$max`/`$first`/`$sort`/**`$having`** all substitute the `_masked` sibling; `wrap_row_on_read` re-applies the mask instead of trusting the parent slot. No remaining leak shape found. |
| **SEC-5** projects.* RPC auth (`1e355ce0`+`57f8d18c`) | **HOLDS** | `config.ts` keyed `rpc:projects`; backstopped by the e919af51 fail-closed default (which itself has the NEW-1 gap for `*`+anon parents — see §2). |
| **SEC-6** sandbox dev-user fail-open (`525a64f2`) | **HOLDS** | Owner derived from the `pws_` pairwise subject; dev fallback gated on positive `ZEROSHIP_DEV === "1"`, not absence of NODE_ENV. |
| **SEC-7** env/secret rotation not reaching isolate (`49973fb5`) | **HOLDS** | `sync.rs:183 needs_reload` includes `env_changed`; PHASE 2 does a full isolate swap, destroying captured module-level secrets. |
| **SEC-9** app Set-Cookie Domain injection (`c2a3a138`+`d41ef79c`) | **HOLDS** | `strip_cookie_domain` preserves a cookie literally named `domain`, strips only trailing `Domain=` attrs; count + 8 KiB byte caps drop overflow whole. |
| **SEC-10** builder IDOR (`e9e10240`) | **HOLDS** | `getQualityScores`/`listIssues` keyed `…:${currentCreatorId()}:${appId}`; swept all KV access points — no remaining appId-keyed shared-KV RPC without an ownership check. |
| e919af51 RPC fail-closed default | **PARTIAL** | Holds for isolated-rpc and self-anon cases; **defeated by an `auth:anon` `*` ancestor → NEW-1**. |
| BFF `session_token`/`auth_token` verifier | **SOUND** | EdDSA pinned, kid accept-list, `zeroship-sess+jwt` typ gate, iss/exp/client_id bound after sig; `?mint=1` requires `X-ZS-Auth` + same-origin Origin; no ACAO emitted. |
| oidc_rp token exchange / JWKS / open-redirect | **SOUND** | Stash HMAC + exact state, PKCE verifier, ID-token sig+iss+aud+nonce+at_hash/c_hash; `sanitize_oidc_original_path` rejects `//`/`/\`/protocol-relative; browser `redirect_uri` exact-matched. |

## 6. Ledger calibrations (corrections to the 2026-06-09 / 2026-06-03 records)

- **L5 (no account lockout) → CLOSED.** `store::users::record_login_failure` + `lockout` module (threshold 5, exponential 60s→3600s backoff), wired in `credentials.rs:264-287`, cleared on success/reset/federated/magic; folded into opaque `InvalidCredentials` (no enumeration). Test `account_lockout_test.rs`.
- **H1 (password-reset session termination) → CLOSED for the reset path.** `password_reset::complete` bumps `credential_version`, writes the `token_revocations` family marker, revokes all `app_session_anchors` (closes `?mint=1`); IdP tier covered by the `sessions::validate` credential_version join.
- **M1 (RP-logout app-session teardown) → still OPEN.** `logout::post` revokes the local `idp_sessions` row + Hydra sid but **not** the `token_revocations` family marker or `app_session_anchors`, so a live gateway `__Host-zeroship_app_session` / `?mint=1` anchor survives an RP-initiated logout until its own expiry. Gateway/anchor-layer item.
- **RT-3** — ledger says wall_timeout "never enforced"; it **is** enforced on the async Pending path (`serve.rs:762`), just not on the sync CPU path (see §4). Partial mis-statement.
- **P2-B6/B7** — the 5xx-sanitization-keyed-on-`AUTH_INSECURE_DEV` framing is **partially refuted**: the rail reads host-process env via `std::env::var`; app `process.env` is built only from worker env + opt-in exposed secrets + user vars (the `std::env::vars()` leak was removed at `init.rs:1853`), and apps have no syscall to mutate `std::env`. So it's an operator-misconfig risk (setting it in prod), **not app-triggerable**. Suggest downgrading.
- **W-1/P2-D1** (ZeroShip-User HMAC not app-bound) — confirmed still open; the signed envelope binds `request_id` + freshness but no `app_id`, keyed by the single global `worker_key`. Reachability is **defense-in-depth only**: the `ZeroShip-User.id` is the per-app `pws_` (a replay presents the wrong identity elsewhere), inbound forged identity headers are stripped at the trust boundary, and the authoritative header is gateway-minted on the outer request — exploitable only with `worker_key` compromise or direct worker network reach.
- **GW-13** (empty-identity mint) — mitigated on the cookie/OIDC-callback/popup arms (non-UUID `sub` rejected); re-confirm the raw-Hydra Bearer arm `build_worker_user_from_access_claims` (`router/auth.rs:1089`), which builds `WorkerUser` from `sub` without an obvious UUID guard — flagged for follow-up.

## 7. Confirmed still-open (unchanged from ledger, not re-filed)

- **Gateway:** GW-1 (no apex allowlist), GW-2 (`/apps/{name}` decouples tenant from origin), GW-3 (XFF leftmost under trust_proxy), GW-4 (unbounded `PerRuleRateLimitRegistry`), GW-5 (no idle-timeout chunked read), GW-6 (content-length not hop_by_hop on chunked path).
- **Worker:** W-1 (above), W-2 (redeploy drops in-flight work, no abort fan-out), W-3 (secrets not zeroized; `EnvSnapshot` cloned per request).
- **Runtime:** RT-2 (`ZEROSHIP_DEV.is_ok()` SSRF fail-open on var *presence*, any value), RT-5 (§4), RT-6 (no external-memory accounting → off-heap OOM), P2-A1 (randomFill detach panic), P2-B4 (build-time `new Function()` over config source on the builder host).
- **Data-plane:** DB-5/RT-4 (capability handle at module top-level, own-app), ST-1 (path validator latent-CRITICAL once S3 lands), ST-2/ST-3/ST-5/ST-6 (size cap / unbounded get-list / `"default"` app_id fallback / symlink walk), KV-1/KV-2 (no per-app quota / unbounded list scan).
- **Sandbox:** SB-A1..A6, SB-1, admin-token strength floor (all per ledger).
- **Secrets/transport:** DR-6 (no TLS on internal PG / inter-service), P4-C-F4 (`control_key` over plaintext internal segment).

## 8. Method & coverage

Each finder read the actual source (not the ledger) and adversarially refuted its own critical/high findings. The auth IdP lane was the deepest (mailer/linker/hydra traced to their sinks). The orchestrator independently re-verified NEW-1 against `compiled.rs` + `manifest.rs` and the builder/example shapes before filing. **`crates/platform` remains dead** (workspace-excluded, no binary, no dependents) and is not attacker-reachable; spending-limit enforcement lives only there, so the live path still has none (consistent with the metering stub, and the reason CT-A1/CT-B2 matter). Not exercised this round (no live infra): a two-app interleave PoC for SEC-1, a live aggregate PoC for SEC-4, live Stripe, the `restore_handler.rs` lifecycle state machines, and a standing console for live builder-RPC reachability.
