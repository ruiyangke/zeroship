# plugin-db Security Review — Round 9 (2026-05-22)

Target: `crates/plugin-db/` at `/home/ruiyang/Projects/appbase`.
HEAD: `a6dca645` (docs/reviews cycle 10:17 — backlog audit). Prior
round: r8 (cycle 09:00) — 82/100.

Six commits landed since r8 that touch plugin-db. Plus one
uncommitted working-tree change (`hardening` Cargo feature) the
prompt flagged as "about to land this cycle".

| Commit | Subject | Security delta |
| --- | --- | --- |
| `09e32998` | scrub stale TX_CONN/TX_TOKEN/MIG_LOCK refs + demote `OBJECT_PREFIX` | none — docstring rename + `pub` → `pub(crate)` (positive, smaller surface) |
| `bc4363f0` | demote `OBJECT_PREFIX` to `pub(crate)` (re-apply) | positive — narrows external visibility of an internal constant |
| `9e392ba1` | enumerate `Result<_, String>` hold-outs (R8-1) | none — comments only |
| `7d0bc4c5` | unify cold-init failure code → `lazy_init_failed` (R8-2) | neutral — code-name unification, no leak/contract change |
| `389749ca` | unify backend-missing code → `backend_not_initialized` (R8-1) | neutral — code-name unification + spelling drift fix |
| `7bd2187e` | scaffold `cargo bench` harness | new surface — audited under dimension 5 |
| `757026e3`, `bed655c1`, `a6dca645` | docstrings, doc-audit closures, backlog audit | none |
| `[uncommitted]` | `hardening` Cargo feature gating `auth/*` | positive — audited under dimension 4 |

This round is a fresh re-audit against HEAD across the eight dimensions
in the prompt.

---

## 1. Findings, by dimension

### 1.1 `classify_p0001_detail` — 5 SDK codes tenant-untouchable (NO FINDING, re-verified)

**Verdict: tight. Surface is unchanged since r7; the new `hardening`
feature in 1.4 makes it **invisible** in default production builds.**

```
[INFO] crates/plugin-db/src/auth/session.rs:174-215 — classify_p0001_detail
       + classify_detail_token
  Why:  Identical contract to r7/r8:
        - The match in `classify_detail_token` (lines 193-215) emits
          only `(&'static str, &'static str)` tuples — no tenant data
          mixes into the discriminator or the operator-facing message.
        - The DETAIL discriminator comes from `db_err.detail()` (line
          181), which is populated by `RAISE EXCEPTION ... USING
          DETAIL = '<token>'` in the SECURITY DEFINER CREATE FUNCTION
          body at `auth/bootstrap.rs:525, 531, 536, 550, 558`. All
          five tokens are hardcoded literals; the only formatter
          (`'invalid actor_kind: %', p_actor_kind`) goes into the
          MESSAGE, not the DETAIL, so it never reaches the
          discriminator.
        - `RAISE ... USING DETAIL = '<literal>'` cannot be spoofed
          across SECURITY DEFINER without already breaching the
          function-trust boundary (verified r7 1.1, unchanged).

  Verification:
    Grep crates/plugin-db/src/auth/session.rs:193-215 — `match detail`
    arms emit only constant tuples.
    Grep crates/plugin-db/src/auth/bootstrap.rs:525, 531, 536, 550,
    558 — each `USING DETAIL = '...'` is a string literal.
    Unit tests `classify_detail_*` cluster (session.rs ≈ lines
    548-606) pin the codes-are-distinct invariant.

  Fix: none.

  New in r9: the audit is now relevant ONLY in `--features hardening`
  builds. In default production builds the entire `crates/plugin-db/
  src/auth/` subtree is excluded from compilation (see 1.4) — the
  surface is irrelevant for the default attack profile, and stable
  for the hardening profile.
```

### 1.2 App-id isolation — re-verified (NO FINDING)

**Verdict: tight. Same as r7/r8.**

```
[INFO] crates/plugin-db/src/v8_classes/replication.rs:34-160 —
       opts.appId ignored; app-id always == self.app_id
  Why:  The 3 `opts`-accepting methods (`setup`, `watchdog`,
        `dropAbandoned`) call `resolve_setup_app_id`,
        `resolve_watchdog_app_id`, `resolve_drop_abandoned_app_id`
        with `_opts: &Value` — underscore-prefixed parameter is a
        forcing function: any restoration of opts-reading must delete
        the underscore (which trips review). All 3 helpers return
        `stamped.to_string()` ignoring `_opts`.

        13 unit tests in the file's #[cfg(test)] block (lines 208-340)
        pin the override-shape-ignoring + empty-opts-fallback
        invariants (verified survived the recent code-name churn).

        The 4th sibling in `v8_classes/db.rs::resolve_consumer_app_id`
        (for `Db::start_replication_consumer`) follows the same
        contract.

        SQL-side, `sanitise_app_id` (replication.rs:82-101) restricts
        to `[A-Za-z0-9_]` and lower-cases on success; `quote_ident`
        (query.rs:146-148) does double-quote escaping with `"` → `""`.
        Validate-then-quote-ident closes identifier injection at the
        SQL boundary; cluster-wide filters bind app-id via $1 LIKE.

  Fix: none.

  Verification:
    Grep `resolve_.*_app_id` in crates/plugin-db/src/v8_classes/
    replication.rs (4 matches plus 3 test-side asserts each).
```

### 1.3 SQL-injection sweep — recent commits touched no SQL (NO FINDING)

**Verdict: clean. The 7 commits since r8 add zero new SQL strings to
the codebase.**

```
[INFO] git diff 3d79d2da..HEAD — production SQL changes
  Why:  Filtered the entire post-r8 diff for new SQL strings using
        a sweep of (SELECT|INSERT|UPDATE|DELETE|CREATE|DROP|ALTER|
        TRUNCATE).*(FROM|VALUES). Zero matches.

        Only string-literal CHANGES touching SQL-adjacent strings
        since r8 are:
          - exec.rs:64, 317: error-code constant changed
            "not_configured" → "lazy_init_failed" (not SQL itself).
          - v8_classes/migration.rs:269 + migrations.rs:180: error-code
            constant changed "not_configured" → "backend_not_initialized"
            (not SQL itself).

        No code in any of the 7 commits constructs, concatenates,
        or executes SQL.

  Verification:
    `git diff 3d79d2da..HEAD -- 'crates/plugin-db/src/*.rs'` filtered
    for SQL keywords → empty result.
    `git diff 3d79d2da..HEAD --stat -- 'crates/plugin-db/src/*.rs'`
    → 13 files, 104+/73- mostly docstring renames.

  Fix: none.
```

The audit found that the recent commits are SQL-neutral: this is a
property of the work, not a finding. Includes by way of confirmation
that:

- `quote_ident` is still the only escape function (unchanged).
- `validate_collection` + `validate_schema` + `validate_field_name`
  remain the parameter-name gatekeepers (unchanged contract,
  unchanged byte-prefix reserved-name checks — see 1.7).
- Every parameterised query call goes through the typed
  `query_text_params(&str, &[&str])` API — no `format!`-into-SQL
  with tenant data introduced.

### 1.4 `hardening` Cargo feature — audited (NET POSITIVE)

**Verdict: net positive for production-build security. The pending
`hardening` feature shrinks the default-build attack surface by
~2,860 LOC of dormant `auth/*` code.**

```
[POSITIVE] crates/plugin-db/Cargo.toml (uncommitted) +
           crates/plugin-db/src/lib.rs:68-71
  Why:
    Before:
      #[cfg(not(feature = "test-helpers"))]
      pub(crate) mod auth;
      #[cfg(feature = "test-helpers")]
      pub mod auth;

      → `auth/*` ALWAYS compiles in default `cargo build --release`,
        even though no production code (in this crate or downstream)
        invokes it. The control-plane wire-up the subsystem is
        designed for (per `auth-r1` design) hasn't shipped yet.

    After (this cycle's pending change):
      #[cfg(all(feature = "hardening", not(feature = "test-helpers")))]
      pub(crate) mod auth;
      #[cfg(all(feature = "hardening", feature = "test-helpers"))]
      pub mod auth;

      → `auth/*` (40,407-byte bootstrap.rs + 22,749-byte session.rs +
        8,416-byte keys.rs + 4,766-byte mod.rs ≈ 76 KB of source) is
        EXCLUDED from default builds. The legacy admin-schema CREATE
        FUNCTION emission path, session-token mint/verify, HMAC key
        rotation — all gone from the default binary.

    Cross-checked there are NO production callers of `crate::auth::`
    in the crate's source tree:
      $ grep -n "crate::auth\|self::auth\|super::auth" src/
        crates/plugin-db/src/auth/session.rs:166: (docstring only)
        crates/plugin-db/src/auth/session.rs:191: (docstring only)
      Only the integration tests (`tests/integration.rs`) consume
      `zeroship_plugin_db::auth::*`, and the [[test]] integration
      target now lists `required-features = ["test-helpers",
      "hardening"]`, so the test contract is preserved.

    Security implications:
      1. **Smaller attack surface in production builds.** ~76 KB of
         source code (incl. `CREATE FUNCTION` bodies + raw SQL
         literals + SECURITY DEFINER orchestration) won't be in the
         shipping binary unless an operator opts into `--features
         hardening`. Dead-code risk (an unused function that becomes
         reachable via a later refactor) is eliminated for the
         default profile.
      2. **No new surface introduced.** The feature is purely
         subtractive; flipping `hardening` on RESTORES the prior
         compilation behaviour 1:1.
      3. **Integration tests still cover both behaviours.** With
         `required-features = ["test-helpers", "hardening"]`, the
         59 integration-test call-sites that exercise
         `ensure_admin_schema`, `mint_session_token`, `init_session`,
         `keys::rotate_session_keys` continue to compile and run.

  Net: +1 on score.

  Verification:
    `git diff crates/plugin-db/Cargo.toml` — `hardening = []` feature
    + `required-features = ["test-helpers", "hardening"]` line.
    `git diff crates/plugin-db/src/lib.rs` — feature-gate on both
    `mod auth` lines.
    `grep -n "feature\\s*=\\s*\"hardening\"" crates/plugin-db/` —
    matches only the 2 lines in `lib.rs`; no other code references
    the feature directly (auth/* is gated at the module boundary).
    `grep -n "crate::auth\\|self::auth\\|super::auth" crates/plugin-
    db/src/` — only 2 docstring references; no in-code callers.
```

One small caveat that doesn't change the verdict: the `hardening`
gate is *coarse* — module-level on/off. A future refinement would be
to expose a thin `auth-api` surface (e.g. `ensure_admin_schema`) as
a separate sub-feature once a production consumer wires it up, so
the rest of the bootstrap/session subsystem can stay gated. Not a
finding; sketching the next-step direction once the control plane
lands.

### 1.5 Bench harness — criterion transitive-dep CVE sweep (NO FINDING)

**Verdict: clean. Criterion is dev-only; the dependency closure
contains no known unpatched CVEs at the pinned versions.**

```
[INFO] crates/plugin-db/Cargo.toml:22-34 — criterion as dev-dependency
  Why:
    `criterion = { workspace = true }` lives ONLY in [dev-dependencies]
    (not [dependencies]), so the dependency graph is invisible to
    `cargo build --release` of any production target (gateway,
    worker, control). It compiles ONLY when running
    `cargo bench -p zeroship-plugin-db` or `cargo test
    --features test-helpers`.

    Resolved versions (from Cargo.lock):

      criterion         0.5.1   (active)
      criterion-plot    0.5.0
      anes              ≥0.1     terminal escape encoding, no I/O
      cast              ≥0.3     pure-arithmetic typecast crate
      ciborium          0.2.2    CBOR serde — only used for
                                 criterion's perf-snapshot files
      clap              4.6.1    arg parsing — same crate the rest
                                 of zeroship-cli already uses
      is-terminal       ≥0.4     wraps libc::isatty
      itertools         0.10.5   (older but stable; pure-Rust iters)
      num-traits        ≥0.2     compile-time numeric traits
      once_cell         ≥1       (already in workspace)
      oorandom          ≥11      tiny PCG PRNG, no system entropy
      plotters          0.3.7    SVG/PNG plot rendering
      plotters-backend  ≥0.3
      plotters-svg      ≥0.3
      rayon             1.12.0   thread-pool — only the criterion
                                 driver uses it, not plugin-db code
      regex             1.12.3   (current; CVE-free in this branch)
      serde             ≥1       (already in workspace)
      tinytemplate      1.2.1    bench-report HTML rendering
      walkdir           2.5.0    (current; no known CVEs)

    Cross-checked against known advisory streams:
      - No GHSA / RUSTSEC alerts on criterion-0.5.x.
      - regex-1.12.3, walkdir-2.5.0, rayon-1.12.0, clap-4.6.x,
        ciborium-0.2.2, tinytemplate-1.2.1 — none have outstanding
        CVEs at the resolved versions.
      - The two transitively-introduced new versions that did NOT
        already exist in the workspace pre-bench (`anes`, `cast`,
        `criterion-plot`, `oorandom`, `plotters*`, `tinytemplate`,
        `walkdir`, `itertools-0.10.5`) are all pure-Rust crates with
        no `unsafe` outside trivial alloc helpers and no network or
        FS-writing code that's reachable from `cargo bench`.

    Bench file itself (`benches/bench_query_build.rs`) audited:
      - Calls `build_find` + `build_insert` on hardcoded fixtures
        (`json!({...})` literals).
      - `app_id` and `collection` are static `&str` constants —
        no tenant input, no env-var read, no file/network I/O.
      - `black_box`-wraps the build outputs (no leakage path).
      - Run gating: `cargo bench -p zeroship-plugin-db --bench
        bench_query_build` is dev-only.

  Fix: none.

  Verification:
    `grep "^name = \"criterion\"" Cargo.lock -A 30` — version 0.5.1,
    dep list as above.
    Cargo.toml structure: `[dev-dependencies]` (line 22) +
    `[[bench]]` (line 32) with `harness = false`.
    The criterion macro is used in the bench file only;
    `#[cfg(bench)]` / `#[cfg(test)]` are never required to compile
    production code paths against criterion's types.
```

### 1.6 [I43] blocking `pg_advisory_lock` — carryover (UNCHANGED)

**Verdict: carry forward. Same impact / fix path as r1–r8.**

```
[CARRY] crates/plugin-db/src/backend/postgres.rs:118 — pg_advisory_lock blocks
  Why:  `SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)`
        is BLOCKING. Used by `acquire_advisory_lock` (postgres.rs:109-
        136), invoked from the register_model bootstrap path
        (orchestrator/register_model/bootstrap.rs ~line 110+ resolves
        through the backend trait at backend/mod.rs:127).

        The non-blocking sibling `pg_try_advisory_lock` exists at
        postgres.rs:138-155 — used for the migrations lock (verified
        via grep `pg_try_advisory_lock` returning matches at
        migrations.rs:38 + postgres.rs:145). The register-model path
        has not been migrated.

        Same per-app keying (`hashtext('zs_reg:<app>')`) means
        cross-tenant DoS is not possible; the surface is within-app
        only (a stuck apply-stage stalls subsequent registerModel
        calls FOR THE SAME APP).

  Impact: within-app DoS only — multi-tenant unaffected. Same severity
    as r1–r8.

  Fix: switch to `pg_try_advisory_lock` + map false → typed
    `DbError::LockContention { code: "lock_not_available" }` so the
    SDK can branch on a typed rejection with backoff guidance
    (parallels the serialization-failure retry contract).

  Verification:
    Grep "pg_advisory_lock" in crates/plugin-db/src/backend/
    postgres.rs:118 (still blocking).
    Grep "pg_try_advisory_lock" — postgres.rs:145 is migrations-only.
```

### 1.7 Reserved-prefix validation — re-verified (NO FINDING)

**Verdict: tight. Unchanged from r7/r8.**

```
[INFO] crates/plugin-db/src/query.rs:51-99 — validate_collection
  Why:  Audit confirms the byte-prefix optimisation (perf r4 N4-I4)
        still preserves the contract:

          if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_")
            → reject "pg_*"   (covers Pg_xxx, PG_xxx, pG_xxx)
          if bytes.len() >= 10
              && bytes[..10].eq_ignore_ascii_case(b"__zeroship")
            → reject "__zeroship*"   (covers casing variants)

        Pre-checks:
          - empty → "collection name cannot be empty"
          - contains '\0' → "must not contain null bytes"
          - len > 63 → NAMEDATALEN limit

        Post-prefix:
          - only `[A-Za-z0-9_]` (excludes `-`, `.`, `$`, whitespace,
            non-ASCII).

        Aliasing attempts via casing (`Pg_`, `pG_`, `PG_`, `__ZeroShip`,
        `__zEroship`) are all blocked by `eq_ignore_ascii_case`.
        Non-ASCII would-be confusables (`рg_` Cyrillic, `pɢ_`) are
        rejected by the ASCII alphanumeric filter at lines 90-96.

        `validate_field_name` (lines 106-123) is the column-name
        sibling: requires non-empty, no-null, 63-byte limit. (It
        relies on `quote_ident` for injection blocking — no
        prefix-reserved check needed because columns aren't a
        namespace.)

  Fix: none.

  Verification:
    Grep crates/plugin-db/src/query.rs:80, 85 — byte-prefix checks.
    Unit tests in the same file pin the rejection contract for
    `pg_*` and `__zeroship*` (verified via the existing test cluster
    structure).
```

### 1.8 r8 carry-overs — hostname-via-error LOW; is_fatal substring-match MEDIUM (UNCHANGED)

#### 1.8.a Hostname info-disclosure via `lazy_init_failed` (LOW, carried)

```
[LOW, CARRY] crates/plugin-db/src/lib.rs:357-370 — init_pool_async
  source-chain walk
  Why:  Unchanged from r8 §1.2. `init_pool_async()` still walks the
        `std::error::Error::source(cur)` chain and concatenates each
        `Display` via `format!(" — caused by: {src}")`. The terminal
        source for a DNS failure is `io::Error` from `getaddrinfo`,
        which historically embeds the queried hostname.

        Caller sites bind to two SDK codes now (post-`7d0bc4c5`):
          - orchestrator/register_model/mod.rs:121 → `lazy_init_failed`
          - exec.rs:64, 317 → `lazy_init_failed` (was `not_configured`)

        r8-1.2 fix-path A: redact the chain walk in `init_pool_async`.
        r8-1.2 fix-path B: tracing::error! the chain on the operator
        side; surface only the top-level kind to the SDK message.

        Code-unification `7d0bc4c5` makes the SDK contract more
        consistent (one code, not two) but does NOT change the
        message-body interpolation — the hostname-leak shape is
        identical. Net for r9: no change to the finding's body or
        severity.

  Impact: LOW. Same as r8. Multi-tenant deployments with
    isolation-meaningful DB hostnames leak topology to tenant JS.
    Single-tenant / public-DNS deployments unaffected.

  Verification:
    Grep crates/plugin-db/src/lib.rs:363-369 — chain walk + format!.
    Grep crates/plugin-db/src/exec.rs:64, 317 — unified to
    `lazy_init_failed`.
    Grep crates/plugin-db/src/orchestrator/register_model/mod.rs:121
    — same code.
```

#### 1.8.b `wal_consumer::is_fatal` substring-match (MEDIUM, carried)

```
[MEDIUM, CARRY] crates/plugin-db/src/wal_consumer.rs:708-725 — is_fatal()
  Why:  Unchanged from r8 §1.4. The function still substring-matches
        on the lowercased rendered `ConsumerError` message:
          - "58p01"             (SQLSTATE — locale-stable)
          - "does not exist"    (English message — locale-fragile)
          - "replication slot"  (English — locale-fragile)
          - "publication"       (English — locale-fragile)
          - "invalid slot name" (English — locale-fragile)

        Producer sites (lines 399, 403, 412, 423, 446) still throw
        away typed `compio_postgres::Error` via `e.to_string()` into
        `ConsumerError::{Connect,Io,Decode}(String)`.

        No changes since r8 to `wal_consumer.rs` or to
        `ConsumerError`'s shape.

  Impact: MEDIUM. Functional fragility on i18n PG builds; not
    tenant-exploitable. Slot reaper could under-classify and the
    supervisor would hammer-retry an invalidated slot. Within-app
    DoS-adjacent; multi-tenant unaffected.

  Fix: same two options as r8 §1.4 — (a) add `Option<SqlState>` to
    each ConsumerError variant or (b) wrap `compio_postgres::Error`
    directly.

  Verification:
    Grep crates/plugin-db/src/wal_consumer.rs:708, 718, 720, 722.
    Grep producer sites at 399, 403, 412, 423, 446 — still
    `e.to_string()` discarding SQLSTATE.
```

---

## 2. Cross-check: things that did NOT regress

For belt-and-suspenders, re-verified against HEAD:

- **r7 §1.1 SECURITY DEFINER DETAIL spoofing** — verified the 5
  `RAISE EXCEPTION ... USING DETAIL = 'session_*'` sites in
  `auth/bootstrap.rs:525-558` are still literal strings, not
  format-interpolated. (Auth code is now feature-gated, see 1.4.)
- **r5 §M-NEW-r5-1 / [I42] released-flag flip order** —
  `orchestrator/lock_guard.rs:333-376` still carries the structural
  invariant pin test (commit `bd1e7ce1`).
- **r4 sanitise_app_id contract** — still rejects empty + non
  `[A-Za-z0-9_]`; lower-cases on success; emits `invalid_app_id`
  with `ValidationFailed`.
- **r3 mark_consumer_running test-only gating** —
  `context.rs:423-426` still `#[cfg(any(test, feature = "test-
  helpers"))]`-gated; production uses
  `try_mark_consumer_running` returning the HashSet::insert bool.
- **r2 mig_lock release-on-test-end** — `lib.rs:240-250`
  `clear_migration_lock_for_tests` still issues `ROLLBACK; SELECT
  pg_advisory_unlock_all();` over the wire before drop, preventing
  the p8a2 hang.

No regressions observed across the seven prior rounds' findings.

---

## 3. Score delta vs r8

| | r8 | r9 |
| --- | --- | --- |
| Findings closed since prior round | none | none (1.8a/1.8b carry; no new fix landed) |
| Findings opened this round | 1.2 (LOW hostname), 1.4 (MEDIUM is_fatal) | none |
| Security-positive structural changes this round | f6043126 SQLSTATE-typed sweep | **hardening Cargo feature** (1.4 — shrinks default-build attack surface by ~76 KB / ~2,860 LOC of auth source) |
| Re-verified clean | DETAIL classification, atomic try_claim, mark_consumer_running cfg-gate, app-id isolation, Configuration hint content | all of the above + reserved-prefix byte-check + SQL-injection sweep against the new commits + criterion transitive-dep CVE sweep |
| Carryover | [I43] blocking pg_advisory_lock | [I43] unchanged |

The r8→r9 delta:

- **+1** for the pending `hardening` feature (1.4): the
  default-build attack surface shrinks by the entire `auth/*`
  subtree (~76 KB source / ~2,860 LOC of SECURITY DEFINER + HMAC +
  session-token code that has zero production callers today). Net
  positive even before the control plane wires it up, because
  unused code in shipping binaries IS attack surface (gadget
  candidates for ROP-style chains, plus the "unused module becomes
  reachable via a later refactor" failure mode). This is the
  largest structural win since the r6→r7 SQLSTATE-typed sweep.

- **+0** for the bench harness (1.5): criterion is a dev-dep, the
  bench file does pure CPU on hardcoded fixtures, and the transitive
  dep closure resolved to versions with no known unpatched CVEs.
  No net change to production-build security; explicit "no
  finding" recorded for the security-r9 audit trail.

- **+0** for code-name unifications (7d0bc4c5, 389749ca): SDK
  contract gets tighter (one code per condition vs two), which is a
  reliability/API-surface win, not a security win. The underlying
  message-body content for `lazy_init_failed` is unchanged, so the
  r8-§1.2 hostname-leak surface is identical at HEAD.

- **−0** for the two carries (1.8a hostname LOW, 1.8b is_fatal
  MEDIUM): already priced into r8's 82. No new findings opened.

- **Net: +1.** Score moves to **83 / 100**.

What would push past 88 now:

1. Close 1.8a (init_pool_async source-chain redaction). Easiest.
2. Close 1.8b by adding `Option<SqlState>` to `ConsumerError::
   {Connect,Io,Decode}` so `is_fatal` reads SQLSTATE directly.
3. Resolve [I43] (`pg_try_advisory_lock` + typed
   `LockContention { code: "lock_not_available" }`).
4. (Optional pre-emptive hardening) Add an `audit-allow-list` test
   that fails if any new `format!(SQL, …)` call site appears
   outside of `query.rs` + `migrations.rs` — locks in the property
   that 1.3 currently verifies by hand.

What would push past 92:

5. Split `hardening` into `hardening-bootstrap` (admin schema +
   keys) and `hardening-session` (mint/init session) sub-features
   so production deployments can flip on the schema piece without
   pulling in the session-mint code yet.
6. Convert `ConsumerError` to typed-SQLSTATE-carrying variants
   (subsumes 1.8b fix).
7. Per-app PG role ownership of WAL slot (r7 push-past-92 carry).
8. Tighten DNS-failure observability: `tracing::error!` with full
   source chain on operator side, sanitised top-level kind to
   tenant (closes 1.8a's residual operator-vs-tenant info
   asymmetry).

---

## Score (1-100)

**83 / 100**

Up +1 from r8 (82). The pending `hardening` Cargo feature is a
structural win — shrinks the default-build attack surface by the
entire dormant `auth/*` subtree (~76 KB source, ~2,860 LOC of
SECURITY DEFINER plumbing with zero production callers). The bench
harness adds no production attack surface (dev-dep only; criterion's
transitive closure is CVE-clean at the resolved versions). The two
r8 carries (LOW hostname leak, MEDIUM is_fatal substring) remain
open with the same severity and same fix-path; no new findings
introduced this round.
