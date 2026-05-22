# plugin-db Security Review — Round 8 (2026-05-22)

Target: `crates/plugin-db/` at `/home/ruiyang/Projects/appbase`.
HEAD: `3d79d2da` (docs-audit r6 fixes — coded_db preamble + IMPORTANT
drift sites). Prior round: r7 (cycle 08:00) — 82/100.

Four commits landed since r7:

| Commit | Subject | Security delta |
| --- | --- | --- |
| `deeefe18` | `migrations::coded_db` dedup | none — routes through shared `prefix_message`; pure refactor |
| `f6043126` | SQLSTATE-typed checks (replication.rs:213, 257) + `classify_detail_token` unit tests | **positive** — closes remaining substring-classification fragility |
| `f1c5184e` | `Configuration` variant gains `hint: Option<String>` | new surface — audited below |
| `3d79d2da` | docs fixes (coded_db preamble + drift sites) | none — comments only |

This round is a fresh re-audit against HEAD, focused on the five
dimensions in the prompt.

---

## 1. Findings

### 1.1 Configuration hint field — content audit (NO LEAK)

**Verdict: no leak.** Audited all five `DbError::Configuration`
construction sites against HEAD. The two sites that ship a non-`None`
hint use **static `&'static str`-class literals** — no tenant input,
no operator-config interpolation, no secret material.

```
[INFO] crates/plugin-db/src/replication.rs:282-285 — wal_level_not_logical hint
  Why:  hint = "set wal_level=logical in postgresql.conf and restart"
        Content: references the canonical Postgres config filename
        ("postgresql.conf") — public knowledge baked into the PG
        distribution layout. No path to a deployment-specific config
        directory, no credentials, no db_url, no tenant data.
  Fix:  none — operator remediation prose, safe to surface to SDK.
  Verification:
    Grep crates/plugin-db/src/replication.rs lines 277-289;
    `hint: Some("set wal_level=logical in postgresql.conf and restart".to_string())`

[INFO] crates/plugin-db/src/wal_consumer.rs:352-356 — not_provisioned hint
  Why:  hint = "replication requires a connected runtime context — set
        DATABASE_URL or pass --db-url so the runtime can mint a
        replication=database connection"
        Content: env-var NAME ("DATABASE_URL") and CLI flag NAME
        ("--db-url") — neither is sensitive. The VALUE of db_url is
        not interpolated. No tenant data, no secrets.
  Fix:  none — operator remediation prose, safe to surface to SDK.
  Verification:
    Grep crates/plugin-db/src/wal_consumer.rs lines 349-358;
    `hint: Some("replication requires a connected runtime context …".to_string())`
```

The other three Configuration sites (`backend/postgres.rs:593`,
`orchestrator/register_model/mod.rs:119,126`) explicitly set
`hint: None`. They are invariant-class errors the SDK cannot remediate
(CIC retry budget exhausted, lazy-init failed, backend uninitialised),
so there is nothing useful to surface even if a hint were safe.

The `lazy_init_failed` site at `mod.rs:121` IS worth a closer look
because it formats `format!("db: lazy init failed: {e}")` where `e`
is the `String` returned by `init_pool_async()`. See finding **1.2**.

### 1.2 lazy_init_failed message body — hostname info-disclosure (LOW)

**New since r7.** Not introduced by f1c5184e specifically, but the
hint-field commit drew attention to the message-body interpolation
the SDK now reads alongside it.

```
[LOW] crates/plugin-db/src/orchestrator/register_model/mod.rs:121
      (and crates/plugin-db/src/exec.rs:64, 317 via the same pattern)
  — db_url HOSTNAME may surface to tenant JS on cold-start connect
    failure
  Why:
    `init_pool_async()` returns `Result<(), String>`. On Postgres
    `Pool::connect(&url, …)` failure, it formats:
        "db: failed to connect: {e} — caused by: {src}"
    walking the error source chain (lib.rs:361-367). The chain's
    Display for `compio_postgres::Error::Kind::Connect` is the static
    string "error connecting to server", but the SOURCE under
    `Kind::Connect` is the wrapped `io::Error` from
    `connect_socket.rs::connect_socket` and (more importantly) from
    `connect.rs::to_socket_addrs_async` (line 118) during DNS
    resolution. `getaddrinfo`'s `io::Error` text typically embeds the
    hostname being resolved (e.g. "failed to lookup address
    information: db-internal.example.com: Name or service not
    known"). That hostname is then propagated to JS as the `.message`
    of a `Configuration { code: "lazy_init_failed", … }` error.

    Tenant JS calls `env.db.registerModel(...)` (or any CRUD) at
    cold start; the FIRST call lazy-inits the pool. If the
    operator-configured DB host is unreachable, the tenant can
    observe the internal hostname — small info-leak of the
    deployment topology.

    NOT a credential leak (the db_url itself is never interpolated
    into the error text — verified `compio_postgres::Error::Display`
    at crates/compio-postgres/src/error/mod.rs:381-406 contains no
    URL or credential formatter). Just the resolved hostname.

  Impact: LOW. Multi-tenant deployments where the DB host is
    isolation-meaningful (e.g. per-region replicas, internal-only
    DNS names) leak topology to tenant JS via an error message.
    Single-tenant / public-DNS deployments are unaffected.

  Fix: At the dispatch boundary, redact the source chain on
    Configuration { code: "lazy_init_failed" } before flowing to JS.
    Two shapes:
      (a) In `init_pool_async()`, strip the `format!(" — caused by:
          {src}")` walk for the Configuration path — the static
          "db: failed to connect: {e}" tells the SDK enough
          (Kind::Connect / Kind::Tls / Kind::Authentication are
          all action-discriminable without the hostname); OR
      (b) Add a tracing::error! with the full chain (operator-side
          observability) and surface only the top-level Kind
          Display to the SDK message body.

  Verification:
    Grep crates/plugin-db/src/lib.rs lines 349-372 — the error
    formatter walks `std::error::Error::source(cur)` and pushes the
    `Display` of each source via `format!(" — caused by: {src}")`.
    The terminal source for a DNS failure is `io::Error` from
    `getaddrinfo`, which historically embeds the queried hostname.
```

### 1.3 classify_detail_token — tenant-untouchable (NO FINDING)

**Verdict: no finding.** The 5 SDK-facing codes are
`&'static str` literals; tenant cannot influence the discriminator.

```
[INFO] crates/plugin-db/src/auth/session.rs:193-215 — classify_detail_token
  Why:  All 5 detail-token arms emit (&'static str, &'static str)
        constants. The DETAIL discriminator the function matches on
        comes from `compio_postgres::Error::detail()` borrow, which is
        populated by the SECURITY DEFINER CREATE FUNCTION body in
        auth/bootstrap.rs (r7 1.1 covered the unspoofability). The
        unit tests at session.rs:548-606 pin the 5 codes plus the
        codes-are-distinct invariant; r7 already validated the
        DETAIL-vs-tenant boundary.
  Fix:  none — security-positive refactor (unit-testable contract).
  Verification:
    crates/plugin-db/src/auth/session.rs lines 193-215 (the `match`)
    + lines 548-606 (test cluster).
```

### 1.4 SQLSTATE-typed checks — sweep audit (ONE CARRYOVER)

**Verdict: replication.rs sweep is complete. One remaining substring
site lives in `wal_consumer::is_fatal` — typed but on a `String`
already lossily-rendered by the producer.**

```
[MEDIUM] crates/plugin-db/src/wal_consumer.rs:708-725 — is_fatal()
  substring-matches "58p01" / "does not exist" / "replication slot"
  / "publication" / "invalid slot name" on the rendered ConsumerError
  message
  Why:
    `ConsumerError::{Connect,Io,Decode}(String)` already drops the
    typed `compio_postgres::Error` via `e.to_string()` at all four
    producer sites (lines 399, 403, 412, 423, 446). By the time
    `is_fatal` runs, the source-chain SQLSTATE is unrecoverable —
    so the substring match is forced by the upstream lossiness.

    Risk: "58p01" is the SQLSTATE constant string, stable across
    locales. But the other three patterns ("does not exist",
    "replication slot", "publication", "invalid slot name") are
    English-only error MESSAGES. A future PG version, an i18n PG
    build, or a formatter tweak in compio-postgres could cause
    `is_fatal` to under-classify — the supervisor would then
    hammer-retry a slot that's actually been dropped, increasing
    replication churn (DoS-adjacent, not a confidentiality risk).

  Impact: MEDIUM. Functional fragility in the watchdog
    classification, not a tenant-exploitable vector. The slot-
    invalidated condition is unambiguous via SQLSTATE 58P01; we just
    can't read it because the producer threw the type away.

  Fix:
    (a) Change `ConsumerError` to carry `Option<SqlState>` alongside
        the String, set at the producer sites that go through
        `Error::as_db_error()?.code()`. `is_fatal` then matches on
        the SqlState variant + a much smaller residual substring
        set; OR
    (b) Change the variants to wrap `compio_postgres::Error`
        directly (the Display string can still be rendered for
        logging via `format!("{e}")`).

  Verification:
    Grep crates/plugin-db/src/wal_consumer.rs:
      - line 399: ConsumerError::Connect(e.to_string()) — `e` is
        compio_postgres::Error (typed) → throws away SQLSTATE.
      - line 403: same producer pattern.
      - line 412, 423, 446: same.
      - line 708: is_fatal(&ConsumerError) reads the rendered string.

    Confirmed no other substring-on-message classification exists in
    production code:
      Grep "\\.contains\\(\"[0-9]{5}\"|\\.to_lowercase\\(\\)\\.contains|msg\\.contains|message\\.contains"
      → all remaining hits are in #[cfg(test)] assertions, not in
        classification paths.
```

This is a carry from a pre-r1 design choice (lossy `ConsumerError`),
not a regression introduced by `f6043126` — that commit fixed the
two `replication.rs` sites that were within reach. The
`is_fatal` substring set is the last fragility surface on the
SQLSTATE-classification rail.

### 1.5 App-id isolation — re-verified (NO FINDING)

**Verdict: tight.** Audited all three v8_class methods that take an
`opts` object and could in principle accept a caller-controlled
`appId` override. All three route through `resolve_*_app_id`
helpers that return `self.app_id` verbatim, with unit-test guards
pinning the invariant.

```
[INFO] crates/plugin-db/src/v8_classes/replication.rs — opts.appId ignored by setup/watchdog/dropAbandoned
  Why:
    Replication::setup     → resolve_setup_app_id(&self.app_id, _opts)         [line 66]
    Replication::watchdog  → resolve_watchdog_app_id(&self.app_id, _opts)      [line 87]
    Replication::dropAbandoned → resolve_drop_abandoned_app_id(&self.app_id, _opts) [line 109]

    All three helpers ignore `_opts` (the underscore parameter
    convention is preserved as a forcing function — a future
    contributor who restores opts-reading has to delete the
    underscore, which trips review).

    The dispatchers in replication_ops.rs forward the resolved
    String unchanged; the pool-driven helpers
    (`replication::watchdog_query`, `replication::drop_abandoned_slots`)
    bind the per-app slot prefix via $1 LIKE — no string
    interpolation of app_id into SQL.

  Fix:  none — invariant pinned by 12 unit tests in
    `crates/plugin-db/src/v8_classes/replication.rs:208-340`
    (override-shape ignoring + empty-opts fallback).

  Verification:
    Grep "resolve_.*_app_id" in crates/plugin-db/src/v8_classes/replication.rs
    + crates/plugin-db/src/v8_classes/db.rs::resolve_consumer_app_id
    (the 4th sibling for `Db::start_replication_consumer`).
```

The `start_replication_consumer` dispatcher (Db::v8 method) routes
through `resolve_consumer_app_id` with the same opts-ignored
contract (db.rs:323-333).

For SQL-side, `sanitise_app_id` (replication.rs:82-101) restricts
to `[A-Za-z0-9_]` and returns a typed `DbError::ValidationFailed
{ code: "invalid_app_id" }`. `quote_ident` (query.rs:146-148) does
proper double-quote escaping. The combination closes identifier
injection at the SQL boundary.

### 1.6 mark_consumer_running test-only gating — re-verified (NO FINDING)

**Verdict: tight.** Verified in r7; nothing has changed since.

```
[INFO] crates/plugin-db/src/context.rs:423-426 — mark_consumer_running is cfg-gated
  Why:  Non-atomic `mark_consumer_running` carries
        `#[cfg(any(test, feature = "test-helpers"))]`. Production
        code uses `try_mark_consumer_running` (context.rs:433-435)
        which returns the `HashSet::insert` bool (true on win, false
        on race-loss). The `try_claim` in `ConsumerRunningGuard`
        (replication_ops.rs:356-359) is the only production caller.

  Fix:  none — gating is correct and `test-helpers` is opt-in
        (`Cargo.toml` features section line 28; integration tests
        list it under `required-features`).

  Verification:
    Grep "#\[cfg\(any\(test, feature = \"test-helpers\"\)\)\]"
    against crates/plugin-db/src/context.rs:423.
    Grep crates/plugin-db/Cargo.toml features list — confirms
    test-helpers is NOT a default feature; only integration tests
    require it.
```

### 1.7 [I43] blocking pg_advisory_lock — carryover (UNCHANGED)

**Verdict: carry forward. Within-app DoS bound only — multi-tenant
unaffected.**

```
[CARRY] crates/plugin-db/src/backend/postgres.rs:118 — pg_advisory_lock blocks
  Why:  `pg_advisory_lock(hashtext('zs_reg:<app>'), hashtext('register_model'))`
        is BLOCKING (not `pg_try_advisory_lock`). A concurrent
        registerModel call for the same app waits indefinitely on
        the lock. The hash key is per-app
        (`hashtext('zs_reg:<app>')`), so App A blocking on its own
        registerModel cannot stall App B.

        Within-app DoS surface: if a tenant's apply-phase stage
        hangs, every subsequent registerModel for the same app
        queues until the holder's PG session releases the lock (on
        connection close or pool recycle). The catastrophic-path
        Drop log at lock_guard.rs:225-237 alerts an operator.

  Impact: within-app DoS only (carryover from r1+). Same severity
    as r7.

  Fix: switch to `pg_try_advisory_lock` + return
    `DbError::LockContention { code: "lock_not_available" }` so
    the SDK can branch on a typed rejection with backoff guidance
    (parallel to the existing serialization-failure retry path).

  Verification:
    Grep "pg_advisory_lock" in crates/plugin-db/src/backend/postgres.rs:118
    (the obtain side) and crates/plugin-db/src/orchestrator/lock_guard.rs:162
    (the release side). No `pg_try_advisory_lock` variant exists in
    the obtain path for register_model — only for the migrations
    lock at backend/postgres.rs:145.
```

---

## 2. Configuration variant new-surface summary

The new `hint: Option<String>` field on `DbError::Configuration` is
**security-neutral** at HEAD:

- Both populated hints are static literals (1.1).
- The three `None` sites are invariant-class errors with no
  remediable hint available.
- The dispatch boundary forwards the hint via
  `OpError::coded(code, message, hint)` (error.rs:245-247), the
  same mechanism that was already shipping hints from
  `Serialization`, `LockContention`, `Transient`, and
  `Coded` variants. No new flow path.
- The `lazy_init_failed` site interpolates `{e}` from
  `init_pool_async()` into the MESSAGE (not the hint), which is
  the path 1.2 flagged for the hostname info-disclosure.

**The hint field itself is fine. The pre-existing message
interpolation around it is the surface to harden.**

---

## 3. What changed for the score

| | r7 | r8 |
| --- | --- | --- |
| Findings closed since prior round | — | none (no NEW carryover; one existing carry promoted to MEDIUM with concrete actionable fix path) |
| Findings opened this round | — | 1.2 (LOW: hostname info-disclosure on lazy_init_failed), 1.4 (MEDIUM: wal_consumer::is_fatal substring) |
| Findings re-verified clean | DETAIL classification, atomic try_claim, mark_consumer_running cfg-gate, app-id isolation | all of the above + Configuration hint content (1.1) + classify_detail_token (1.3) |
| Carryover | [I43] blocking pg_advisory_lock | [I43] unchanged |

The r7→r8 delta:

- **+2** for `f6043126`: the replication.rs:213,257 SQLSTATE
  cutover is the last substring-classification site in PRODUCTION
  error-routing logic. r7's "what would push past 88" item 1
  (re: locale-fragile classification) is closed for the
  ensure_publication_and_slot path. The 7 new
  `classify_detail_token` unit tests pin the contract.
- **+0** for `f1c5184e`: net-zero. Hint content is safe (1.1) and
  the new field doesn't add a leak surface — but the audit
  exposed the latent hostname info-leak in 1.2 (the surrounding
  message-body interpolation), so net the rounding leaves the score
  unchanged on this commit.
- **−1** for 1.2 (LOW finding, NEW in r8): hostname info-disclosure
  on lazy_init_failed in deployments with isolation-meaningful DB
  hostnames. Low severity (no credential leak; static-DNS
  deployments unaffected), but a fresh finding.
- **−1** for 1.4 (MEDIUM finding, promoted from latent-not-flagged
  in r7): `wal_consumer::is_fatal` substring classification was
  not flagged in r7 because the dimension audit focused on
  classification-into-DbError paths. r8's broader sweep
  caught it. Functional fragility (under-classification on
  i18n PG builds), not tenant-exploitable.
- Net: 0 (from 82 to 82).

Score is held at **82 / 100**. The substring-classification cleanup
is offset by two new findings the dimension-by-dimension audit
exposed.

What would push to 88+ now:

1. **Close 1.2** (lazy_init_failed source-chain redaction at
   dispatch boundary). Easiest of the new items.
2. **Close 1.4** by adding `Option<SqlState>` to
   `ConsumerError::{Connect,Io,Decode}` so `is_fatal` reads
   SQLSTATE directly.
3. Resolve [I43] (carryover from r1+).
4. The four carryover items from r7's "would push to 88+" list
   (audit/schema length cap, wildcard-arm replacement,
   `slot_status` pub(crate), [I43]). Items 1-3 above subsume the
   I43 piece; the audit/wildcard pieces are independent.

What would push past 92:

5. The four r7 push-past-92 items remain open (DDL builder
   consolidation, per-app conn-acquire semaphore, per-app PG role
   ownership of WAL slot, `compile_error!` on
   `mark_consumer_running` rename).
6. Convert `ConsumerError` to typed-SQLSTATE-carrying variants
   (subsumes 1.4 fix).
7. Tighten DNS-failure observability: a structured
   `tracing::error!` with the full source chain on the operator
   side, and a sanitised top-level kind on the tenant side
   (closes the residual operator-vs-tenant info asymmetry
   surfaced in 1.2).

---

## Score (1-100)

**82 / 100**

Unchanged from r7 (82). Two security-positive refactors this cycle
(`f6043126` SQLSTATE-typed checks; `f1c5184e` Configuration hint
field with safe content) are offset by two new findings (1.2 LOW,
1.4 MEDIUM) that the r8 dimension-by-dimension sweep exposed but
that pre-existed the commits under review. No regression; no new
attack surface introduced by the round's commits; latent fragility
in two adjacent paths now made visible.
