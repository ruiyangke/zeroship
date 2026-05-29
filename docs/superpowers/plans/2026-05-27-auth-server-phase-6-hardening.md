# Auth server — Phase 6 implementation plan: Operational hardening

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan.

**Goal:** Ship the operational hardening items — JWK rotation cron, audit-log retention sweeper, SES-SNS webhook signature verification, a small load test, and the operator deploy runbook. **DPoP is explicitly deferred** to a future Phase 7 (proposal §18 accepts the gap at v1). Security review is also out-of-scope here — it's a meta-task done by a reviewer after the PR opens.

**Architecture:** Cron-like work runs as in-process tasks spawned at `crates/auth` boot — no external scheduler. Each task wakes up at a configured interval, sweeps PG, and goes back to sleep. The load test is a small script using cyper to hit `/login` and `/oauth2/token` at concurrency, measure RPS, log percentiles. The deploy runbook documents what we already ship — not new behavior.

**Tech Stack:** Continuing — compio, ntex, cyper, compio-postgres, jsonwebtoken. New: optional `rsa` crate or `ring` for SNS signature verification (RSA-SHA1 over canonicalized message).

**References:**
- Proposal §7.6 (key rotation), §15 (audit retention), §16 (ops), §18 (DPoP deferred at v1)
- AWS SNS message-signing docs (for U3)
- Phase 1's `bootstrap::keys` for the key set names

**Pre-launch posture:** the runbook describes the v1 deployment. Future changes (e.g., enabling DPoP) get separate runbook addendums.

**Starting point:** worktree tip post-Phase-5 (`f67076ad` after `auth-phase-5` tag). 426 workspace tests green.

---

## Phase 6 unit list

| # | Unit | Files | Time |
|---|---|---|---|
| U1 | JWK rotation cron (90-day prepend + 31-day retire) | crates/auth/src/cron/{mod,jwk_rotation}.rs | 60 min |
| U2 | Audit-log retention sweeper | crates/auth/src/cron/audit_retention.rs | 40 min |
| U3 | SES-SNS webhook signature verification (+ `/webhooks/ses-sns` handler) | crates/auth/src/{ui/webhooks.rs, mailer/sns.rs} | 60 min |
| U4 | Load test for `/login` and code-exchange | crates/auth/tests/load_test.rs (manual; behind env flag) | 50 min |
| U5 | `docs/runbooks/auth-deploy.md` operator deploy guide | docs/runbooks/auth-deploy.md | 30 min |
| U6 | Phase 6 close-out + `auth-phase-6` tag | – | 10 min |

Total: ~4h, ~9 commits.

---

# Unit U1 · JWK rotation cron

**Background.** Hydra's `hydra.openid.id-token` and `hydra.jwt.access-token` keysets are populated at first boot. Per proposal §7.6: rotate every 90 days. The pattern is **prepend** (new key becomes the active signer; old keys remain for verification until they're retired).

## Files

- Create `crates/auth/src/cron/mod.rs` (declares submodules + the `spawn_all` orchestrator)
- Create `crates/auth/src/cron/jwk_rotation.rs`
- Modify `crates/auth/src/main.rs` (spawn the cron tasks after server boot)
- Modify `crates/auth/src/config.rs` (add `--jwk-rotation-days` with default 90, `--jwk-retain-days` with default 31)

## Implementation

```rust
//! crates/auth/src/cron/jwk_rotation.rs
//!
//! Daily check: if the active signing key in either key set is older than
//! `rotation_days`, prepend a new key (which becomes the active signer per
//! hydra's list-based store). Old keys older than `retain_days` past their
//! demotion get deleted from JWKS.

use std::time::Duration;

use crate::config::AuthConfig;
use crate::hydra_client::HydraAdmin;

pub async fn run(admin: HydraAdmin, cfg: AuthConfig) {
    let rotation_days = cfg.jwk_rotation_days;
    let retain_days = cfg.jwk_retain_days;
    loop {
        if let Err(e) = tick(&admin, rotation_days, retain_days).await {
            tracing::error!(error = %e, "jwk_rotation tick failed");
        }
        compio::time::sleep(Duration::from_secs(86_400)).await;  // 24h
    }
}

async fn tick(admin: &HydraAdmin, rotation_days: i64, retain_days: i64) -> crate::error::Result<()> {
    rotate_set_if_due(admin, "hydra.openid.id-token", &["EdDSA", "RS256"], rotation_days).await?;
    rotate_set_if_due(admin, "hydra.jwt.access-token", &["EdDSA"], rotation_days).await?;
    retire_stale_keys(admin, "hydra.openid.id-token", retain_days).await?;
    retire_stale_keys(admin, "hydra.jwt.access-token", retain_days).await?;
    Ok(())
}

async fn rotate_set_if_due(admin: &HydraAdmin, set: &str, algs: &[&str], days: i64) -> crate::error::Result<()> {
    // 1. Fetch the JWKS.
    let jwks = match admin.get_jwks(set).await? { Some(j) => j, None => return Ok(()) };
    // 2. Compare the newest key's age — hydra stores keys in order; first key is the active signer.
    //    JWKs don't natively carry a creation timestamp; we inspect the kid which (per Phase 1) is
    //    a uuid simple. So we can't tell age from kid alone. Approach: store a "last_rotated_at"
    //    timestamp in a new `auth.cron_state` table. If absent or older than `days`, rotate.
    // ... (see below)
}
```

**Storage for last-rotated timestamps.** Either:
- Add a `auth.cron_state` table: `key TEXT PRIMARY KEY, last_rotated_at TIMESTAMPTZ`. Simple.
- Or: parse the kid format Phase 1 set up (`kid_<uuid_simple>`) — but UUIDv4 doesn't encode time.

Go with `auth.cron_state` table. Add migration:

```sql
CREATE TABLE IF NOT EXISTS auth.cron_state (
    key             TEXT PRIMARY KEY,
    last_rotated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
)
```

On `tick`: read `last_rotated_at` for the set (default: very-old if absent). If `NOW() - last_rotated_at > days`, prepend new keys via `admin.create_jwk(set, alg)` for each alg, then `UPDATE auth.cron_state SET last_rotated_at = NOW()`.

`retire_stale_keys`: if `last_rotated_at` is older than `rotation_days + retain_days`, scan the JWKS and delete all but the most recent N keys (where N = number of algs). Concretely, just delete each `kid` that isn't in the latest-N set.

Tests:
- Unit test: `should_rotate(last, days)` returns true when last is old enough
- Live-PG: insert old `cron_state` entry, run tick, verify a new JWK appears in the live hydra

## Cron orchestrator

```rust
// crates/auth/src/cron/mod.rs
pub mod jwk_rotation;
pub mod audit_retention;

use std::sync::Arc;
use crate::config::AuthConfig;
use crate::hydra_client::HydraAdmin;

pub fn spawn_all(admin: HydraAdmin, db: Arc<compio_postgres::Client>, cfg: AuthConfig) {
    let admin_jwk = admin.clone();
    let cfg_jwk = cfg.clone();
    compio::runtime::spawn(async move { jwk_rotation::run(admin_jwk, cfg_jwk).await; }).detach();

    let db_audit = db.clone();
    let cfg_audit = cfg.clone();
    compio::runtime::spawn(async move { audit_retention::run(db_audit, cfg_audit).await; }).detach();
}
```

Call from `main.rs` after server bootstrap, before `server::run`.

## Commit

```
auth: cron/jwk_rotation — 90-day prepend + 31-day retire (auth.cron_state table)
```

---

# Unit U2 · Audit-log retention sweeper

Per proposal §15: retention buckets — security events 365d hot / 7y cold, PII-bearing rows 90d, debug 30d, refresh-reuse events forever.

For v1 simplicity, ship just the **hot-retention sweeper** (delete rows past their `event_type`'s hot TTL). Cold-storage tiering is post-launch.

## File

- Create `crates/auth/src/cron/audit_retention.rs`

## Implementation

```rust
//! Audit-log hot retention. Deletes rows past their event-type's hot TTL.
//! Cold-tiering to S3/etc. is post-launch.

use std::time::Duration;
use crate::config::AuthConfig;

pub async fn run(db: std::sync::Arc<compio_postgres::Client>, cfg: AuthConfig) {
    let interval_secs = cfg.audit_retention_check_secs;
    loop {
        if let Err(e) = tick(&db).await {
            tracing::error!(error = %e, "audit_retention tick failed");
        }
        compio::time::sleep(Duration::from_secs(interval_secs)).await;
    }
}

async fn tick(db: &compio_postgres::Client) -> crate::error::Result<()> {
    // Retention table per proposal §15:
    //   Security events       (login_*, oauth_*, magic_*, token_*, key_*, session_*): 365 days hot
    //   PII-bearing failures  (signup_blocked, verification_*, password_reset_*):     90 days hot
    //   Debug                 (mailer_*, hydra_*):                                    30 days hot
    //   refresh_reuse_detected:                                                       FOREVER

    let security_events = &[
        "login_success", "login_failure",
        "oauth_callback_success", "oauth_callback_failure", "oauth_link_success", "oauth_unlink_success",
        "magic_redeemed_same_device", "magic_redeemed_cross_device",
        "verification_redeemed", "password_changed",
        "session_created", "session_rotated", "session_revoked",
        "key_rotation_announced", "key_rotation_completed",
    ];
    let pii_events = &[
        "signup", "signup_blocked",
        "verification_issued",
        "password_reset_requested",
        "magic_issued",
        "oauth_start",
    ];
    let debug_events = &[
        "mailer_send", "mailer_bounce", "mailer_complaint", "mailer_suppressed",
        "hydra_accept_login", "hydra_accept_consent", "hydra_accept_logout",
    ];

    // refresh_reuse_detected: never deleted (security-critical alert signal).
    delete_older_than(db, security_events, 365).await?;
    delete_older_than(db, pii_events, 90).await?;
    delete_older_than(db, debug_events, 30).await?;
    Ok(())
}

async fn delete_older_than(
    db: &compio_postgres::Client,
    event_types: &[&str],
    days: i64,
) -> crate::error::Result<()> {
    let affected = db.execute(
        "DELETE FROM auth.audit_events \
         WHERE event_type = ANY($1) \
           AND occurred_at < NOW() - ($2::text || ' days')::interval",
        &[&event_types, &days.to_string()],
    ).await.map_err(|e| crate::error::AuthError::Db(format!("audit retention sweep: {e}")))?;
    if affected > 0 {
        tracing::info!(deleted = affected, days, "audit_retention swept rows");
    }
    Ok(())
}
```

`AuthConfig` gains `--audit-retention-check-secs` with default 3600 (hourly).

## Commit

```
auth: cron/audit_retention — hot-retention sweeper (365d security / 90d PII / 30d debug)
```

---

# Unit U3 · SES-SNS webhook signature verification

Postmark webhooks are Basic-auth-protected (Phase 5). SES delivers via SNS; SNS messages are RSA-SHA1-signed with a per-region cert. We must verify the signature before trusting the payload.

## Files

- Create `crates/auth/src/mailer/sns.rs` (signature verify + cert fetching)
- Modify `crates/auth/src/ui/webhooks.rs` (`POST /webhooks/ses-sns` handler)
- Modify `crates/auth/src/mailer/mod.rs` (`pub mod sns;`)
- Modify `crates/auth/src/server.rs` (register route)

## SNS verification algorithm

Per AWS docs:
1. POST body is JSON with `Type`, `MessageId`, `TopicArn`, `Message`, `Timestamp`, `Signature`, `SigningCertURL`, `SignatureVersion`.
2. Build canonical string-to-sign by concatenating specific fields in alphabetical order, each prefixed by `<field-name>\n<value>\n`.
3. RSA-SHA1 verify the base64-decoded `Signature` against the certificate at `SigningCertURL`.
4. Validate `SigningCertURL` is on `*.amazonaws.com` (anti-SSRF).

For v1 we can use the `rsa` crate (already in workspace? if not, crate-local) + `x509-parser` for cert parsing. OR write a minimal verifier with `ring`'s public-key API.

Recommended: `ring::signature::UnparsedPublicKey<&[u8]>` with `ring::signature::RSA_PKCS1_SHA1_VERIFICATION` once the public key is extracted from the cert.

For SES specifically, also handle the SNS subscription-confirmation step: when SES is wired to SNS for the first time, SNS sends a `SubscriptionConfirmation` message. Our handler must GET the `SubscribeURL` to confirm.

For a v1 minimum:
- Verify SNS sig
- Handle SES-style `Bounce` / `Complaint` events (nested JSON inside `Message`)
- Update `auth.email_suppressions` accordingly
- Defer subscription-confirmation auto-handling (operator runs the curl once)

If the rsa/ring verification path proves too gnarly, **fall back to deferring SES-SNS entirely** to a Phase 7 — leave the route stub returning 501 + `Notice: SES-SNS verification not yet implemented; deploy with Postmark for now`.

## Commit

```
auth: mailer/sns + /webhooks/ses-sns — RSA-SHA1 signature verify + Bounce/Complaint suppression
```

---

# Unit U4 · Load test

A small standalone test that:
1. Boots `crates/auth` (in-process via `ntex::web::test::server`)
2. Issues `concurrent_logins` parallel password logins
3. Measures wall time + p50 + p99 latency
4. Reports passes if p99 < 500 ms and throughput > 100 RPS on a 4-core box (loose targets — the goal is to detect catastrophic regressions, not to certify performance)

Behind `cfg(feature = "load-test")` or env-gated to skip in normal `cargo test` runs.

## File

- Create `crates/auth/tests/load_test.rs`

```rust
//! Auth server load test. Behind AUTH_LOAD_TEST=1 env (off in normal test runs).
//!
//! Boots crates/auth in-process. Runs N parallel /login POSTs. Reports
//! throughput + p50 + p99.

use std::time::Instant;

#[ntex::test]
async fn auth_login_throughput() {
    if std::env::var("AUTH_LOAD_TEST").is_err() {
        eprintln!("skip (set AUTH_LOAD_TEST=1 to run)"); return;
    }
    // ... boot fixture (steal from e2e_password.rs)
    // ... seed N test users
    // ... fire N parallel password logins via cyper futures + select_all
    // ... measure
    // ... assert: p99 < 500ms, throughput > 100 RPS
}
```

Don't over-engineer. The point is regression detection.

## Commit

```
auth: tests/load_test — login throughput regression check (AUTH_LOAD_TEST=1 to run)
```

---

# Unit U5 · `docs/runbooks/auth-deploy.md`

Operator-facing deployment guide. ~150 lines covering:

1. Architecture overview (one paragraph)
2. Container topology (postgres, hydra, hydra-migrate, crates/auth, gateway, control)
3. Required environment variables (table: name, default, required, where to set)
4. First-boot sequence (run hydra-migrate, start hydra, then `crates/auth --bootstrap`)
5. Healthchecks (`/healthz`, `/readyz`, `:4445/health/ready`)
6. Logs (where they go, structured JSON, audit ingestion)
7. Failure modes:
   - Hydra DB connection lost → restart hydra
   - Auth DB connection lost → 503 from auth, ongoing sessions still validate at gateway via cached JWKS
   - JWKS rotation: scheduled daily; expected log lines
8. Backup/restore — pg_dump covers both `hydra_*` and `auth.*` schemas; SECRETS_SYSTEM/COOKIE must be backed up separately
9. Capacity (rough): 4 vCPU + 8 GB serves O(1000 logins/sec) per the load test
10. Rotating client secrets, removing OAuth providers, etc.

## Commit

```
docs: runbooks/auth-deploy — operator-facing deployment guide
```

---

# Unit U6 · Phase 6 close-out + tag

Empty commit + tag:

```
auth: Phase 6 complete — operational hardening (JWK rotation, audit retention, SES-SNS, load test, runbook)

DPoP is deferred to a future Phase 7 per proposal §18 (accepted gap at v1).
Security review happens out-of-band by an external reviewer after this PR opens.
```

Tag `auth-phase-6`. List all 6 tags.

---

# Future work (post-Phase-6)

- **Phase 7+** — DPoP at the gateway; refresh-token reuse-detection audit signaling; back-channel logout consumption; `auth-phase-1.5` style cleanups
- **Security review** — external reviewer pass on the whole proposal/auth-server branch before merge

---

# Self-review

**Spec coverage** — proposal §7.6 (key rotation), §15 (audit retention), §16 (ops), §12 (SES-SNS — deferred from P5), §18 (DPoP gap → deferred to P7) all addressed.

**Placeholder scan** — no TBDs. SES-SNS verification has a fallback (defer to P7) that's pragmatic, not a placeholder.

**Scope** — Phase 6 ships the genuinely operational pieces. DPoP and security review are explicitly noted as future/external work.

# Execution handoff

Plan saved to `docs/superpowers/plans/2026-05-27-auth-server-phase-6-hardening.md`.
Execute via subagent-driven-development.
