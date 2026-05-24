# Sandbox/snapshot-restore — security r16 review

Date: 2026-05-25 (UTC)
HEAD at audit: `a0888d9e` (8 commits past r15's `2e9ae598`).
Round 16 of N (security lens catching up). Read-only.
Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`,
`crates/sandbox/migrations/**`.

## Summary

1 new IMPORTANT (R16-S1, `0009_wake_jobs.sql:128-130` grants
SELECT on `wake_jobs` to `sandbox_audit` — directly contradicts
0004 role-split which strips ALL read access from
`sandbox_audit` and reserves it INSERT-only on `events`). 2 new
MINOR posture (R16-S2: `error_message TEXT` + `agent_url TEXT`
in `wake_jobs` are free-form, no length cap, no
controller-side redaction strategy — PR2 wiring is the leak
point; R16-S3: `WakeResponseMode::from_env` silent fallback on
unrecognized values + a fourth disjoint test ENV_LOCK).

Two prior items now CLOSED: R15-S1 at `da951dd9`
(`lib.rs:1003-1028::assert_kek_required_for_remote_store`),
R15-S2 at `801ae449` (wrapper `assert_under_task_dir` guard).
R14-S1 subsumed by R14-A1 extract.

The `detach_isolated` helper at `crates/sandbox/src/detach.rs`
is the right shape (single-call surface, factory closure,
kernel-truncated thread name, panic-contained). **Detach
posture strictly unchanged from r15 §4** — `Arc<AppState>`
clones cross the thread boundary the same way the open-coded
`std::thread::Builder::spawn` did pre-extract. No new
credential surface, no new zeroize gap. PR1 added no admin
endpoint (verified).

## Hunt disposition (terse)

1. **`detach_isolated` (`detach.rs:76-110`)** — Arc<AppState>
   crosses 8 migrated sites (`admin_handlers.rs:1354`,
   `snapshot_store_gcs.rs:1143`, `sweep.rs:233/575`,
   `registry.rs:836`, `lib.rs:1117/1204/1421`). Credentials
   identical to pre-refactor — `admin_token: Zeroizing`,
   `persist` AEAD key, `database` DSN+pw, GCS `cached_token.bearer:
   String` (plain). **No zeroize fires on thread exit** (Arc
   refcount > 0 elsewhere); same posture as r15 §4 documented
   for open-coded shape. uid/gid shared (CLONE_THREAD); threads
   run uid 0 (raw_exec boundary). Panic-contained per
   `detach.rs:171-188`; spawn/runtime-new failure logs +
   drops factory. **No new finding from the helper.**

2. **`wake_jobs` role grants** — `0009_wake_jobs.sql:128-130`
   grants SELECT on `wake_jobs` to `sandbox_audit`;
   `0004_role_split_phase3.sql:62-70` explicit invariant:
   *"sandbox_audit: INSERT-only on events (no SELECT, no
   DELETE)"* + REVOKE ALL ON ALL TABLES. **0009 contradicts
   0004.** → R16-S1.

3. **`error_message` / `agent_url` privacy** —
   `0009_wake_jobs.sql:44,49`: free-form TEXT, no cap, no
   redaction. PR2 wiring is the leak materialization point
   (today's wake-path errors carry RFC1918 IPs, agent URLs,
   ureq bodies, filesystem paths). → R16-S2.

4. **`WakeResponseMode` kill-switch
   (`config.rs:880-895`)** — unrecognized values silently
   fall back to `Sync` with only `tracing::warn!`. Env
   /proc/<pid>/environ reachability already gates on
   root-equivalent → no new exposure. Silent misconfig is the
   failure mode. Also: `wake_response_mode_tests` at L910-940
   adds a **fourth** disjoint test ENV_LOCK (R12-S1 family).
   → R16-S3.

5. **#24 audit** — `nomad_ch.rs:2373` injection intact;
   regression pins at L4091-4156 + L4402-4429.
   CLOSED at `a4c481e1` stands.

6. **Carry**: R13-S1 unchanged (`provision-gcp-cluster.sh:286`
   `--scopes=storage-rw` no `--service-account`); R9-S3
   unchanged (`lib.rs:1018`/`db.rs:2457,2469` stamp dek_id
   unconditionally); R13-S2/R10-S1-3/R9-S2/S6/S7/S8/R11-S3/R12-S1/R14-S2
   unchanged. R9-S1 partially closed by R15-S2. R15-S3 still
   doc-only.

7. **Clock-resync** (`handlers.rs:68` `RESYNC_CHALLENGES` +
   `:762-867::clock_resync`) unchanged from r15 §2. No
   regression.

8. **Admin endpoint surface** (`main.rs:194-235`): same 10
   routes as r15. **No new route in PR1** (expected — PR2
   adds wake-polling).

## Findings (NEW since r15)

### [R16-S1] `0009_wake_jobs.sql:128-130` grants SELECT on wake_jobs to `sandbox_audit` — violates 0004 role-split invariant (IMPORTANT, security-r16)

- **Files**: `migrations/0009_wake_jobs.sql:123-131` vs
  `migrations/0004_role_split_phase3.sql:62-70`.
- **Symptom**: 0004's design comment (lines 6-17) + the
  `REVOKE ALL ON ALL TABLES IN SCHEMA sandbox FROM
  sandbox_audit` at L67 codify the role as INSERT-only on
  `events`. 0009 grants `SELECT` on `wake_jobs` to the same
  role. A future operator reading the role's grant list will
  see a read surface and reasonably conclude
  `sandbox_audit` is a "read-only audit reader" — the
  OPPOSITE of 0004's design.
- **Threat model**: small impact today (no in-tree code uses
  `sandbox_audit` to read anything). Risk is **doctrine
  drift** — future migrations copying 0009's grant pattern by
  example, expanding the role's read surface by accretion,
  until the invariant is dead letters and the deferred-T1
  read-only role lands into a confused role taxonomy.
- **Why IMPORTANT**: (1) explicit security-boundary in 0004;
  (2) silent violation of a versioned design comment in a
  prior migration; (3) fix is a 3-line drop of the IF/GRANT/END
  block.
- **Action**: (a) drop the `sandbox_audit` GRANT in 0009 —
  either via forward-only 0010, or in-place if 0009 hasn't
  shipped to prod pg yet. (b) defer wake-job reads to T1's
  `sandbox_admin_ro` OR ship an ADR amending 0004; pick one.
  (c) add CI gate that diffs role grants across migrations
  and flags any role gaining a privilege it lost in a later
  migration.

### [R16-S2] `wake_jobs.error_message` + `agent_url`: free-form, unbounded, no redaction strategy at PR1 schema level — high PR2-wiring leak risk (MINOR posture → IMPORTANT-on-PR2, security-r16)

- **Files**: `migrations/0009_wake_jobs.sql:44-49`;
  `crates/sandbox/src/db.rs:1400-1412,2917-3012`.
- **Symptom**: `error_message TEXT` / `agent_url TEXT` lack
  length caps, format constraints, or redaction helpers. PR2
  wiring per `c7-lt-async-wake.md` will map per-state failure
  to `(WakeErrorCode, Option<error_message>)`; the temptation
  will be `format!("{e:?}")` straight into the column.
  Wake-path errors today include cluster-internal IPs
  (`10.x.y.z`), agent URLs, ureq error bodies (often with
  wrapped agent journald output), filesystem paths.
- **Threat model**: with R16-S1 closed, column is SELECT-able
  by `sandbox_app` and any future `sandbox_admin_ro`. Data
  retained indefinitely until `gc_expired_wake_jobs` reaps
  it (PR1 doesn't pin a retention cadence).
- **Why MINOR-now / IMPORTANT-on-PR2**: PR1 only lands shape.
  PR2 is where the leak materializes.
- **Action**:
  (a) Pre-PR2 land a `sanitize_wake_error_message` helper
      (max 256 bytes, replace non-printable, strip RFC1918 +
      IPv6 link-local + agent URLs).
  (b) Every `update_wake_job_state(_, _, _, Some(msg), _)`
      site routes through it. Reject PR2 if
      `format!("{e:?}")` appears as an arg.
  (c) Column-level CHECK on `agent_url` shape
      (`^https?://[a-zA-Z0-9.-]+(:[0-9]{1,5})?(/.*)?$`).
  (d) PR2 must specify retention period + GC cadence.

### [R16-S3] `WakeResponseMode::from_env` silent fallback on unrecognized values + introduces a fourth disjoint test ENV_LOCK (MINOR posture, security-r16)

- **Files**: `crates/sandbox/src/config.rs:880-895` (from_env);
  `:910-940` (test ENV_LOCK).
- **Symptom (a)**: operator setting
  `SANDBOX_WAKE_RESPONSE_MODE=ASYNC` (uppercase),
  `=true`/`=1`/`=on` silently lands in `Sync`. Fail-safe
  direction is correct; the silence is the failure.
- **Symptom (b)**: `wake_response_mode_tests` adds a new
  module-private `static ENV_LOCK: std::sync::Mutex<()>` at
  L920 — fourth disjoint per-key env-lock alongside R12-S1's
  family. Cross-module concurrent tests touching overlapping
  env vars under disjoint locks remains R12-S1 posture under
  Rust 2024.
- **Why MINOR**: (a) is operability papercut; (b) test-only
  UB risk (R12-S1 family).
- **Action**:
  (a) Change `from_env`'s `Ok(other)` arm to return `Err`
      (or `panic!`) — mirror R15-S1's fail-CLOSED pattern for
      AEAD. Misconfigured feature flags should refuse to boot.
  (b) Fold `wake_response_mode_tests::ENV_LOCK` into R12-S1's
      tracked list — single crate-level `ENV_LOCK` proposal.

## Carry-forward open at HEAD `a0888d9e`

R13-S1 (IMPORTANT) · R12-S1 partial (IMPORTANT) ·
R10-S1/S2 (IMPORTANT) · R9-S2/S3 (IMPORTANT) · R14-S2 ·
R13-S2 · R11-S3 · R10-S3 · R9-S6/S7/S8 · R15-S3 (all MINOR).
R9-S1 partially closed by R15-S2.

Closed since r15: R15-S1 (`da951dd9`), R15-S2 (`801ae449`),
R14-S1 / R14-A1 (`3d8acc23` + migrations), R16-I1
(`96fa5f0f`), R16-A2 (`417cd6cd`).

## Counts

- CRITICAL: 0 new; carry: R9-S1 (partial).
- IMPORTANT: 1 new (R16-S1); carry: R13-S1, R12-S1 partial,
  R10-S1, R10-S2, R9-S2, R9-S3.
- MINOR: 2 new (R16-S2, R16-S3); carry: R14-S2, R13-S2,
  R11-S3, R10-S3, R9-S6/S7/S8, R15-S3.
- Total NEW this round: 3.

## Cross-lens consensus

- **arch-r16/PR1**: `detach_isolated` extract is the right
  shape. No security objection.
- **concurrency-r16**: R16-I1 closure verified; C-7-LT is
  security-orthogonal but introduces R16-S1/S2/S3.
- **test-cov**: R16-S2 closability via proptest on the future
  PR2 sanitizer; R16-S1 closability via migration-grant CI
  diff.
- **api-surface**: 0009's role grant violates an immutable
  invariant from 0004. Treat 0004's comment block as a
  versioned API contract.

## Lens hand-off

- **Architecture**: PR2 design needs explicit decision on
  error_message redaction (R16-S2) — `WakeJobErrorBuilder`
  newtype OR pg CHECK. Pick one; document.
- **Concurrency**: R16-S3 (b) — fold new test ENV_LOCK into
  R12-S1's tracked list.
- **Test-cov**: migration-grant CI gate (R16-S1 (c)) +
  PR2 sanitizer proptest (R16-S2 (a)).

## PR2 security gates (must-block-merge until verified)

1. **No `error_message` sink without sanitizer**: every
   `update_wake_job_state(_, _, _, Some(msg), _)` site routes
   through a redactor (≤256 bytes, RFC1918 + IPv6 link-local
   stripped, agent-URL stripped). Reject PR2 if
   `format!("{e:?}")` appears as an arg.
2. **0009 role grant resolved**: either revert
   `sandbox_audit SELECT` via 0010, or ship an ADR amending
   0004. No middle ground.
3. **`agent_url` constraint**: column-level CHECK enforcing
   the URL shape OR controller-side regex gate before INSERT.
4. **`WakeResponseMode` strict boot**: `from_env`'s
   `Ok(other)` arm returns `Err` (mirror R15-S1's pattern).
5. **GC retention bound documented**: PR2 must specify a
   default `gc_expired_wake_jobs` cadence + retention period,
   wired through a configurable. Don't ship an unbounded
   TEXT-blob accumulator that nobody calls.
