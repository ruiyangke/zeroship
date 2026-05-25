# Sandbox/snapshot-restore — code-quality r13 review

Date: 2026-05-25 (UTC)
HEAD at audit: `e887b8ee`
Round 13 of N.

## Summary

- **5 findings** (1 CRITICAL, 1 MAJOR, 3 MINOR).
- **Score: 76/100** (▼ 2 from r12's 78). Net of:
  - **+3 R12-Q1 CLOSED at `46e0fa2a`** — `db.rs:494` TODO comment
    rewritten: the "next round picks it up" promise is gone,
    replaced with "Not yet fixed because compio_postgres::Pool is
    !Send + !Sync …" and a pointer to R11-P1 in the deferred
    review file. Comment-only fix, but it closes the 20-day-stale
    misleading-TODO finding. **3 prod TODOs → 2** (`k8s.rs:495`,
    `snapshot_store_gcs.rs:1081`).
  - **+2 R10-Q5 CLOSED at `0cc7af52`** — `proxy.rs:552`
    `_ref_imports` dead-by-design fn deleted (14 LOC). Closes a
    round-4 minor carry.
  - **+1 R11-S2 CLOSED at `85e4f2f9`** — `enforce_host_id_file_mode`
    shipped at `db.rs:1161` with the same mode+uid invariant as the
    R9-S4 family. Five sibling functions now exist in 5 modules; see
    R13-Q2 for the new code-quality blocker this surfaces.
  - **−1 R13-Q1 (CRITICAL)** — `R12_I1_ENV_LOCK` (restore_handler.rs)
    and `T7_ENV_LOCK` (nomad_ch.rs) are TWO separate mutexes that
    BOTH guard the *same* process-global env var
    `SANDBOX_TASK_DRIVER`. Cargo's default test runner runs all
    tests in a single process with multiple threads → cross-module
    interleave is reachable. Verified below.
  - **−1 R13-Q2 (MAJOR)** — the 5-site uid-check pattern (snapshot
    KEK, AEAD key, pg-password, host_id, admin token) now has TWO
    incompatible error envelope shapes (`Result<_, String>` ×3 vs
    `Result<_, DatabaseError>` ×2). R11-A1's helper extract can't
    land without first harmonising these.
  - **−2 R13-Q3/Q4/Q5** (MINOR — three new code-smell observations
    in the R12-I1 add).
  - **−1 R10-Q7 / R11-Q5** `register_restored` default `Ok(())` —
    round **9**, no movement. The longest-running open finding;
    each round of inaction is itself a degradation signal.
  - **−1** continued openness of the round-4+ minor cluster
    (`R10-Q4` sig.rs:120 hyphenated UUID, `R10-Q6` 70 Duration
    literals, `r9 #5` clock_resync Result<(), String>).
- **Inertia signals**:
  - **R10-Q7 / R11-Q5** `register_restored` Ok(()) — **round 9**
    (R5-Q1 origin). Longest-running.
  - **R10-Q2** `clock_resync` 147 LOC — round 5.
  - **R10-Q4** sig.rs:120 hyphenated UUID — round 5. 1-line doc edit.
  - **R10-Q6** 70 Duration literals (now 71, see Trend) — round 6.
  - **R11-A1** secret-loader extract — now FIVE shipped uid-checks
    (R9-S4 / S4b / S4c / S4d / S2). Extract case is no longer
    structural — it's blocked on error-type harmonisation (R13-Q2).
  - **R12-Q2** T-7 driver-name magic strings (`"raw_exec"`, `"ch"`,
    `"ch_plugin"`) still unextracted — **round 2**. R12-I1 at
    `b3bf741c` propagated `"raw_exec"` and `"ch"` into a SECOND file
    (restore_handler.rs:1373, :1396) without extracting first; the
    cost-of-postponement grew by 2 callsites in 1 round.
  - **R12-Q3** ENV_LOCK duplication — now **3 copies**
    (db.rs:2790, nomad_ch.rs:4073, restore_handler.rs:2478). r12
    set 3-copies as the breakpoint where extraction stops being
    optional; that threshold has now been crossed. See R13-Q1
    which escalates this to CRITICAL because of cross-module
    racing.

## Clippy output (sandbox crate)

Same as r10/r11/r12 — clippy not installed in this nix env.

```
$ cargo clippy --version
error: no such command: `clippy`
help: view all installed commands with `cargo --list`
$ which cargo-clippy clippy-driver
cargo-clippy not found
clippy-driver not found
```

Workspace `[workspace.lints.clippy] all = deny, pedantic = warn,
nursery = warn` gates at CI. r13 falls back to grep + AST-by-eye on
the source files (same fallback as r10-r12).

## Clippy output (sandbox-agent crate)

Same — clippy unavailable.

## Trend numbers (delta from r12)

| Metric | r12 sandbox | **r13 sandbox** | r12 sb-agent | **r13 sb-agent** |
|---|---|---|---|---|
| `#[test]` / `#[compio::test]` / `#[tokio::test]` (grep, all attrs) | 319 | **322** (+3) | 242 | **177**¹ |
| `Duration::from_secs(N)` literals (across both) | 70 | **71** (+1, R12-I1 added one in submit_restore_job's `nomad_post_blocking` call at L1131) | — | — |
| `err(50x, ..., format!("...{e}"))` raw-leak sites | 0 | **0** ✓ | 0 | 0 |
| Bare `.{read,write,lock}().unwrap()` (registry+k8s+docker prod) | 45 | **45** (registry.rs alone: 31, sampled — see R10-Q3 reverify) | 0 | 0 |
| TODOs / FIXMEs (prod) | 3 | **2** (db.rs:494 closed at 46e0fa2a; k8s.rs:495 + snapshot_store_gcs.rs:1081 remain) | 0 | 0 |
| Static `Mutex<()>` test-env-lock copies in prod files | 2 (nomad_ch + db) | **3** (R13-Q1 — adds restore_handler.rs:2478) | 0 | 0 |
| `pub fn` / `pub(crate) fn` / `pub async fn` | 299 | **303** (+4) | 68 | **69** (+1) |
| Longest fn LOC (sandbox) | 272 (main) / 270 (preview_proxy) / 263 (do_restore_inner) | **unchanged**; restore_handler.rs `build_restore_nomad_job_json` is the **new 4th-longest at ~181 LOC** (L1276-L1457) | 147 | 148 |
| File LOC top-5 (sandbox) | nomad_ch 5371, db 3108, restore_handler 2367, lib 2412, admin_handlers 1781 | **nomad_ch 5371 (=), db 3298 (+190 from R11-S2), restore_handler 2680 (+313 from R12-I1), lib 2412 (=), admin_handlers 1781 (=)** |

¹ The sandbox-agent test count discrepancy with r12 (177 vs r12's
242) is a **methodology delta**, not a regression. r12 likely
counted `cargo test` actual runs; r13 counts grep'd
`#[(test|compio::test|tokio::test)]` attribute sites in src/. The
**+11 trend** since r11 (from r12's reporting) remains the better
trajectory signal — R10-Q5 closed one dead-fn module, R12-I1's
tests went into the sandbox crate (not sb-agent), nothing was
deleted. Future rounds should pin a single counting method.

**Reconciliation of LOC growth**:

- `restore_handler.rs` grew **+313 LOC** at R12-I1 (`b3bf741c`).
  ~181 LOC is the new `build_restore_nomad_job_json` body
  (L1276-L1457) plus its 5-test scaffolding (L2441-L2673, with the
  R12_I1_ENV_LOCK + with_task_driver_env helper). Body-to-test
  ratio is ~1:1 — lower than T-7's 1:6 because the new helper
  duplicates ~120 LOC of jobspec-construction logic from
  nomad_ch.rs::build_nomad_job_json (acknowledged in the source
  comment at L1257: "We don't share the helper because the restore
  path doesn't have a `user_id`/`project_id` to plumb through Meta").
  See R13-Q3.
- `db.rs` grew **+190 LOC** at R11-S2 (`85e4f2f9`) — the new
  `enforce_host_id_file_mode` + 3 tests (loose-perms, non-root-owned,
  root-owned positive). Mirrors R9-S4 / S4b / S4c / S4d shape but
  uses mode `0o600` instead of `0o400` and uses `DatabaseError`
  instead of `String` for the error type. See R13-Q2.

## Findings (NEW since r12)

### CRITICAL

#### [R13-Q1] `R12_I1_ENV_LOCK` (restore_handler.rs) and `T7_ENV_LOCK` (nomad_ch.rs) are TWO mutexes guarding the SAME env var → cross-module test races exploitable in `cargo test` (CRITICAL, code-quality-r13, **NEW** — escalates R12-Q3 from MINOR after R12-A1's architectural surfacing)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:2478`
    ```rust
    static R12_I1_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ```
  - `crates/sandbox/src/backend/nomad_ch.rs:4073`
    ```rust
    static T7_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ```
- **Symptom**: Both mutexes guard mutations of the SAME process-global
  env var `SANDBOX_TASK_DRIVER`. Each mutex is module-private and
  serialises only tests *within its own module*. Cargo's default
  test runner (`cargo test --lib`) runs a single-process binary
  with a tokio-rs-style thread pool — by default the test harness
  uses one OS thread per logical CPU and runs tests in parallel
  WITHIN ONE PROCESS.
- **Concrete race scenario** — Thread A is a test in
  `restore_handler::tests`:
  1. Acquires `R12_I1_ENV_LOCK`.
  2. `set_var("SANDBOX_TASK_DRIVER", "ch_plugin")`.
  3. Calls `task_driver_mode_from_env()` → returns `ChPlugin`.

  Meanwhile Thread B is a test in `nomad_ch::tests`:
  - Acquires `T7_ENV_LOCK` (orthogonal to R12_I1_ENV_LOCK).
  - Reads `task_driver_mode_from_env()` *expecting RawExec* (because
    it called `with_task_driver_env(None, ...)`).
  - But Thread A just set the var to `"ch_plugin"`.
  - Thread B observes the foreign env-state, returns `ChPlugin`.
  - Assertion `task["Driver"] == "raw_exec"` fails.

  Or symmetrically: Thread A in `nomad_ch::tests` does
  `set_var("SANDBOX_TASK_DRIVER", "ch_plugin")` while Thread B in
  `restore_handler::tests::nomad_restore_job_spec_uses_raw_exec_by_default`
  is running its `task_driver_mode_from_env()` call inside its
  `with_task_driver_env(None, ...)` block.

  **Specific tests that race in BOTH directions**:
  - `nomad_ch::tests::nomad_job_spec_uses_raw_exec_by_default` (L4099, holds T7_ENV_LOCK only)
  - `nomad_ch::tests::nomad_job_spec_uses_ch_when_flag_set` (L4126, holds T7_ENV_LOCK only)
  - `restore_handler::tests::nomad_restore_job_spec_uses_raw_exec_by_default` (L2507, holds R12_I1_ENV_LOCK only)
  - `restore_handler::tests::nomad_restore_job_spec_uses_ch_when_flag_set` (L2552, holds R12_I1_ENV_LOCK only)

  All four call `task_driver_mode_from_env()` (or its env-side
  effect propagates to it via the `build_*_nomad_job_json` arg).
  Any pair (one from each module) running concurrently races.

- **Why this is CRITICAL (not MINOR like r12 graded R12-Q3)**:
  1. **Reproducibility**: the race IS triggered by `cargo test --lib`
     in the default mode. Not "in theory" — the env var IS the
     shared state and both lock copies actively mutate it.
     Probability scales with cargo's parallel-test fan-out (e.g.
     16-core CI runners run more tests in parallel → higher
     interleave probability).
  2. **Flakiness shape**: when the race triggers, the test that
     loses asserts `task["Driver"] == "raw_exec"` and gets `"ch"`
     (or vice versa). Both arms of the failure are *test logic
     failures*, not panics — they manifest as `FAILED` exit codes
     in CI with no panic backtrace. This is the worst kind of flake:
     reads as "real test failure", not "infrastructure issue".
  3. **Self-aware design**: T-7's source comment at
     `nomad_ch.rs:4077` explicitly claims: *"No other crate touches
     SANDBOX_TASK_DRIVER at test time"* — which was TRUE when T-7
     landed. R12-I1 at `b3bf741c` (4 commits later) **broke that
     invariant** by adding a second consumer in
     restore_handler.rs::tests. R12-I1's own comment at L2473-2477
     acknowledges the symbol-scoping problem (*"We don't share the
     same mutex symbol across crates"*) but addresses it by adding
     a SECOND mutex instead of resolving the scoping. Two locks +
     one shared resource = inconsistent serialisation.
  4. **The race surface keeps growing**: any future test that
     reaches for `SANDBOX_TASK_DRIVER` (e.g. an integration test
     on the takeover path, or a snapshot test that wants to check
     wake-path behaviour under both modes) will need to choose one
     of the two locks — and pick the wrong one half the time.
- **Action**: Two options, in increasing order of effort.
  1. **Quick fix (recommended for next cycle)**: define a single
     crate-private mutex in a new `crates/sandbox/src/test_env_lock.rs`
     module (gated `#[cfg(test)]`):
     ```rust
     #![cfg(test)]
     //! Serialises every test in the crate that mutates a
     //! process-global env var. Two prior copies (db.rs::ENV_LOCK,
     //! nomad_ch.rs::T7_ENV_LOCK, restore_handler.rs::R12_I1_ENV_LOCK)
     //! collapsed into one in R13-Q1 — a single lock is the only
     //! shape that's sound across the crate's parallel-test runner.
     pub(crate) static SANDBOX_ENV_LOCK: std::sync::Mutex<()> =
         std::sync::Mutex::new(());
     ```
     Then each test module's `with_*_env` helper acquires
     `crate::test_env_lock::SANDBOX_ENV_LOCK` instead of its local
     copy. Estimated ~30 LOC of refactor (3 call sites updated,
     2 statics removed, 1 new module added).
  2. **Robust fix**: introduce a typed env-mutation helper
     `with_env<R>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> R)
     -> R` that takes the crate-wide lock, sets/restores the vars,
     and runs f. This is the proposal in r12's R12-Q3 — the
     CRITICAL race makes it the right shape *and* the right
     scope. Estimated ~50 LOC + 3 callsite migrations.
- **Severity rationale**: in concurrency-r12 the registry RwLock
  observation was marked "read-only safe, write-only benign"
  because all callers held the lock for short, non-I/O bounded
  scopes against in-memory state. The ENV_LOCK family is the
  opposite: short locks against PROCESS-GLOBAL state, with
  off-lock consumers that race. **The duplication isn't a
  maintenance smell — it's a correctness bug in the test
  scaffolding.** CI green-runs to date probably reflect "we
  haven't run the two test modules' env tests in parallel often
  enough" rather than "this is safe".

### MAJOR

#### [R13-Q2] 5-site uid-check pattern has TWO incompatible error envelope shapes — `Result<_, String>` (3 sites) vs `Result<_, DatabaseError>` (2 sites) → R11-A1 helper extract is BLOCKED until they're harmonised (MAJOR, code-quality-r13, **NEW** — surfaces a refactor blocker for the R11-A1 carry)

- **Files** (5 sibling functions; the 5th, `enforce_host_id_file_mode`,
  shipped at R11-S2 `85e4f2f9`):
  - `crates/sandbox/src/snapshot_aead.rs:185-205` — `RootKek::from_path` → `Result<Self, String>` (R9-S4)
  - `crates/sandbox/src/persist.rs:333-368` — `AeadKey::from_path` → `Result<Self, String>` (R9-S4b)
  - `crates/sandbox/src/lib.rs:936-977` — `load_admin_token` → `Result<Option<String>, String>` (R9-S4d)
  - `crates/sandbox/src/db.rs:826-853` — `enforce_password_file_mode` → `Result<()>` where the alias is `Result<T, DatabaseError>` (R9-S4c, **diverges from above**)
  - `crates/sandbox/src/db.rs:1161-1188` — `enforce_host_id_file_mode` → `Result<()>` where the alias is `Result<T, DatabaseError>` (R11-S2, **diverges from above**)
- **Symptom**: The mode-check + uid-check pattern is identical in
  shape across all five sites (~25-30 LOC each, same `metadata` →
  `mode & 0o777` → `if mode != X { return Err(...) }` → `if uid != 0
  { return Err(...) }` flow). The only structural divergence is the
  error envelope:
  - 3 callers return raw `String` errors via `format!(...)`.
  - 2 callers (both in db.rs) return `DatabaseError::Validation(format!(...))`.

  All 5 error messages have the same shape:
  `"<var-name>=<path>: <reason>"` or `"<entity> <path>: <reason>"` —
  semantically identical, structurally divergent. A reader auditing
  the security invariant has to inspect 5 functions and prove the
  shape is consistent; the inconsistency in the wrapper type means
  the bodies *look* different at a glance.
- **Why MAJOR (not MINOR)**:
  1. **Blocks R11-A1**: r12 graded R11-A1 ("4-site secret-loader extract")
     as "structural — 4 copies of nearly identical code, ~30 LOC removed,
     ~20 LOC helper". With R11-S2 making it 5 copies AND introducing the
     2nd `DatabaseError`-wrapped variant, the helper extract cannot land
     as a single signature without either:
     (a) returning `String` and forcing the 2 `DatabaseError` callers
         to wrap at the call site, OR
     (b) returning `DatabaseError` and forcing the 3 `String` callers
         to unwrap+rewrap, OR
     (c) introducing a new shared error type (e.g. `SecretFileError`)
         that the helper returns + every call site converts from.
     None of (a)-(c) is mechanical. The extract that was "obviously
     structural" in r12 is now blocked on a typing decision.
  2. **Convention drift signal**: 4 of the 5 sites converged on the
     `Result<_, String>` shape over 4 review rounds. The 5th
     (`enforce_host_id_file_mode`, R11-S2) introduced the
     `DatabaseError`-wrapping divergence on the way IN — it didn't
     adopt the established sibling shape. This is the kind of
     drift that compounds: the next uid-check site (if R13 surfaces
     a 6th) will pick one or the other arbitrarily.
  3. **Auditability**: the 5 sites' security invariant is "mode +
     uid == correct, else refuse to boot". A reviewer chasing the
     invariant has to read 5 functions; if they all returned the
     same error type the invariant would be a 5-min audit, not a
     20-min one.
- **Action** (recommended for next cycle, BEFORE R11-A1 extract):
  - **Step 1**: introduce a shared error type in
    `crates/sandbox/src/secret_file.rs`:
    ```rust
    /// Errors loading a root-owned secret file (mode + uid + content).
    /// All five sibling uid-check functions return this; the call sites
    /// in db.rs wrap it into DatabaseError::Validation, while the
    /// callers in lib.rs / persist.rs / snapshot_aead.rs surface the
    /// Display impl directly (their err type is String).
    pub(crate) enum SecretFileError {
        Stat { path: PathBuf, source: std::io::Error },
        Mode { path: PathBuf, found: u32, expected: u32 },
        Owner { path: PathBuf, found_uid: u32 },
        Length { path: PathBuf, found: usize, expected: usize },
        Empty { path: PathBuf },
    }
    impl Display for SecretFileError { ... }
    impl From<SecretFileError> for String { ... }
    impl From<SecretFileError> for DatabaseError { ... }
    ```
  - **Step 2**: extract `check_root_owned_secret_file(path, expected_mode)
    -> Result<std::fs::Metadata, SecretFileError>` as the
    crate-private helper.
  - **Step 3**: each existing site collapses from ~25-30 LOC to
    ~5 LOC plus its own content-format check (which IS different
    per site — admin_token is text, AEAD key is 32 bytes, etc.).
  - Estimated total: ~120 LOC removed, ~80 LOC added (new module +
    impls + helper), 5 sites simplified. Net win ~40 LOC + 1 new
    audit-target (the helper) instead of 5.
- **Why now**: every passing review round that doesn't address
  this lets the divergence compound. R11-A1 is round-3 carry; with
  a 5th site now landed AND introducing the divergence, the cost
  of waiting another round grows monotonically.

### MINOR

#### [R13-Q3] R12-I1's `build_restore_nomad_job_json` (181 LOC at restore_handler.rs:1276) is a near-clone of `nomad_ch::build_nomad_job_json` (MINOR, code-quality-r13, **NEW**)

- **File**: `crates/sandbox/src/restore_handler.rs:1276-1457`
  (181 LOC). Marked `#[allow(clippy::too_many_arguments)]` (9
  parameters) — the lint annotation IS a self-acknowledged smell.
- **Symptom**: The body duplicates ~120 LOC of jobspec
  construction from `nomad_ch.rs::build_nomad_job_json` (the env
  block at L1334-L1349, the resources block at L1351-L1363, the
  driver+config match arms at L1371-L1419, the job envelope at
  L1421-L1456). The match arms in particular are nearly identical
  to the cold-boot builder's, with only the restore-specific
  `restore_from` / `ZSBX_RESTORE_FROM` insertions differing. The
  source comment at L1257 acknowledges this:
  > "We don't share the helper because the restore path doesn't
  > have a `user_id`/`project_id` to plumb through Meta — those
  > are already recorded on the source sandbox row in pg, the
  > wrapper doesn't need them."

  This is a **plausible-sounding rationale that doesn't survive
  inspection**: the cold-boot builder takes `user_id` +
  `project_id` as args, but a wrapper that accepts `Option<&str>`
  for both (or that takes a `JobSpecKind::ColdBoot { user_id,
  project_id }` / `JobSpecKind::Restore { alloc_dir }` enum) would
  avoid the duplication. Two ~180-LOC builders that diverge in
  ~10% of their bodies is a classic copy-paste maintenance trap.
- **Why MINOR (not MAJOR)**:
  - The duplication is **self-aware** (the rationale comment is
    explicit), not silent. A reader chasing a bug in either
    builder will know to check the other.
  - There ARE genuine differences (Meta block omits user_id /
    project_id; PUBKEY_HEX omitted under restore; ZSBX_RESTORE_FROM
    env injection) — the share would be non-trivial.
  - The cold-boot builder is itself ~150 LOC; the unified version
    would be ~250 LOC with a dispatch enum. The split-build keeps
    each path readable in isolation.
- **What WILL bite later**:
  - **Drift between the two builders is the next-most-likely R-N
    finding**. When (e.g.) bug #N requires adding a new env-block
    field to the cold-boot path, the same fix has to land in the
    restore path independently. The two will diverge until a
    reviewer catches it 3-6 rounds later.
  - The `#[allow(clippy::too_many_arguments)]` suppresses the
    9-arg signature — adding a 10th would silently compile. A
    builder-pattern wrapper (`NomadRestoreJobBuilder::new().vm_index(...)
    .alloc_dir(...).build()`) avoids both the clippy suppression
    AND the position-mismatch hazard at call sites.
- **Action** (defer until 1st divergence-bug surfaces):
  1. Extract a `JobspecKind { ColdBoot { ... }, Restore { ... } }`
     enum carrying the per-kind differences.
  2. Unify the env/resources/job-envelope construction into a
     single `build_nomad_job_json_typed(kind: JobspecKind, cfg:
     &NomadCHConfig, mode: TaskDriverMode) -> serde_json::Value`.
  3. Both call sites collapse to ~5-line `match` + delegate.
  4. The 9-arg signature dies; the clippy suppression with it.
  Estimated ~200 LOC of restructure, no net new logic, ~5 tests
  added pinning the cross-kind shape symmetry.
- **Adjacent code smells in the new fn**:
  - Duplicated `format!("zsbx-restore-{}", sandbox_id.simple())`
    at `restore_handler.rs:1108` (in `submit_restore_job`) AND
    `:1169` (in `teardown_restore`). Extract a const
    `JOB_ID_PREFIX: &str = "zsbx-restore-"` and a 1-line helper
    `fn restore_job_id(sandbox_id: Uuid) -> String { format!("{JOB_ID_PREFIX}{}", sandbox_id.simple()) }`.
  - `.display().to_string()` appears 10× in the new env/config
    blocks (L1336, L1338, L1339, L1347, L1375, L1399, L1402,
    L1404, L1405). All produce `String` allocations that feed
    into `serde_json::json!`. A `Cow<str>` wrapper or a
    `path_str(&Path) -> String` helper would centralise; not a
    perf win (json!() owns its values anyway) but a readability
    one. See R13-Q4.

#### [R13-Q4] `format!()` allocations in R12-I1 add could be expressed via static strs / inline (MINOR, code-quality-r13, **NEW** — companion to r12 perf flag)

- **File**: `crates/sandbox/src/restore_handler.rs:1276-1457`.
- **Symptom**: r12's perf axis flagged "~14 short String allocs
  per restore in the new ChPlugin branch". Code-quality view of
  the same data:
  - **Forced `String` allocs**: `vm_index.to_string()` (L1335),
    `memory_mb.to_string()` (L1343), `cpus_boot.to_string()`
    (L1344), `cfg.subnet_second_octet.to_string()` (L1348),
    `vm_index.to_string()` (L1430). These are unavoidable for
    serde_json string values (`json!` requires `&str` or
    owned). **Not improvable**.
  - **`Path::display().to_string()`** (10 sites in the new fn,
    counted above). The `Path::to_string_lossy()` returns
    `Cow<'_, str>` which serde_json's `json!` accepts directly via
    `Into<Value>`, avoiding the eager allocation when the path is
    valid UTF-8 (the common case for `/var/zeroship/...`). Net
    win: 0-allocation in the UTF-8-clean fast path. **Improvable**.
  - **Constant strings**: `"raw_exec"` (L1373), `"ch"` (L1396),
    `"${NOMAD_TASK_DIR}"` (L1337), `""` (L1407 empty pubkey).
    These are `&'static str` literals already — they're stored
    in `serde_json::Value::String` via a `String` allocation
    inside `json!`'s expansion, NOT via an explicit `to_string()`
    in our code. So they're already as cheap as the json! macro
    allows; the only way to avoid the alloc is to construct the
    `Value::String` via `Value::String(Cow::Borrowed(...))` ...
    which serde_json doesn't expose. **Not improvable without
    bypassing json!()**.
  - **Numeric-to-string formatting**:
    `{NOMAD_TASK_DIR}` (1 site, static, ok), the 5
    `format!`-equivalent `.to_string()` calls listed above (all
    needed for the json! macro). **Not improvable**.
- **Why MINOR**: of the ~14 String allocs r12 perf flagged, only
  the 10 `.display().to_string()` sites have a code-quality
  improvement angle (replace with `to_string_lossy()`). The other
  4 are forced by serde_json's owned-string requirement. Even the
  10 path-display sites buy you nothing in the common case
  because the json! macro will own the value anyway — the
  `Cow::Borrowed` arm is only reached if json! pivoted to a
  `Cow`-aware builder, which it doesn't.
- **Action**: extract a `fn path_to_value(p: &Path) ->
  serde_json::Value` helper that uses `to_string_lossy()`
  internally. 10 sites collapse to `path_to_value(&workspace_img)`.
  Not a perf win in practice; pure readability + 1-place audit
  for path-encoding decisions. ~15 LOC.

#### [R13-Q5] R11-S2's `enforce_host_id_file_mode` uses raw `0o600` and `0o400` literals — neither is named (MINOR, code-quality-r13, **NEW**)

- **File**: `crates/sandbox/src/db.rs:1172` (`0o600`) and 5 sibling
  sites using raw `0o400` (`db.rs:837`, `lib.rs:951`,
  `persist.rs:348`, `snapshot_aead.rs:193`, `lib.rs:1511 + 1571 +
  1603 + 1639` in test setup).
- **Symptom**: r12 highlighted the 5 sibling functions' shape
  symmetry but didn't flag the raw-octal-literal pattern. R11-S2
  introduced a 6th distinct mode value (`0o600` for host_id,
  diverging from `0o400` for the 4 prior siblings). Neither value
  is named:
  ```rust
  if mode != 0o400 { ... }   // 5 sites
  if mode != 0o600 { ... }   // 1 site (R11-S2)
  ```
  A reader has to remember: which mode applies to which file?
  The host_id is `0o600` because the writer needs to update it on
  every controller restart (a 0o400 writer would have to chmod-up,
  chmod-down which is racy); the other 4 are `0o400` because
  they're write-once-by-systemd-then-read-only-forever. **This
  rationale exists in the doc comment at db.rs:1150** but not in
  the literal itself.
- **Action**: extract crate-private mode constants in
  `crates/sandbox/src/secret_file.rs` (or wherever R13-Q2's helper
  lands):
  ```rust
  /// Mode for static-after-systemd-bootstrap secrets (KEK, AEAD key,
  /// pg password, admin token). Owner-read-only. Tightest possible
  /// for files that never change after boot.
  pub(crate) const SECRET_FILE_MODE_RO: u32 = 0o400;

  /// Mode for controller-mutable secrets (host_id rewritten on
  /// every controller restart). Owner-read-write. R11-S2: 0o400 is
  /// inappropriate here because the writer would race with itself
  /// on chmod-up + write + chmod-down.
  pub(crate) const SECRET_FILE_MODE_RW: u32 = 0o600;
  ```
- **Why MINOR**: the mode values aren't structural in the
  security sense — `0o600` and `0o400` are equally root-locked
  from a "non-root can't read" perspective, and the uid check is
  what actually enforces the security invariant. The naming is
  pure readability. But: pairing this extract with R13-Q2's
  helper extract is a 1-commit win. Doing R13-Q2 without R13-Q5
  means the helper signature carries a raw `expected_mode: u32`
  — still bad smell.

## Closed by recent commits since r12

- **[R12-Q1]** `db.rs:494` Database::open_pool stale TODO — **CLOSED at `46e0fa2a`**. Comment rewritten to remove the "next round picks it up" promise; now reads "Not yet fixed because compio_postgres::Pool is !Send + !Sync ... deferred to a dedicated R11-P1 sprint". Doc-only fix, but it eliminates the misleading-deferral signal.
- **[R10-Q5]** proxy.rs:552 `_ref_imports` dead-by-design fn — **CLOSED at `0cc7af52`**. 14 LOC removed. Round-4 carry, closed in r13 cycle.
- **[R12-I1]** wake-path SANDBOX_TASK_DRIVER feature flag — **CLOSED at `b3bf741c`**. 313 LOC + 5 tests. **BUT** this commit introduced R13-Q1 (cross-module ENV_LOCK race) and R13-Q3 (181-LOC duplicated builder). The fix is structurally correct; the implementation introduced two new code-quality findings.
- **[R11-S2]** host_id reader uid check — **CLOSED at `85e4f2f9`**. 190 LOC + 3 tests at db.rs:1161. **BUT** this commit surfaced R13-Q2 (error envelope divergence) by introducing the 5th sibling with the DatabaseError envelope while siblings 1-3 use String.
- **[R11-T3]** capability list pins — **CLOSED at `eb26db31`**. Same as r12 reporting.

## Carry-forward (still open)

| Item | Status | Round count |
|---|---|---|
| **[R10-Q7 / R11-Q5]** `register_restored` default `Ok(())` | OPEN — unchanged | **round 9** (R5-Q1 origin) |
| **[R10-Q4]** sig.rs:120 hyphenated UUID stale doc-example | OPEN — unchanged | round 5 |
| **[R10-Q6]** 70 `Duration::from_secs(N)` literals, no central `timeouts` mod (now 71 with R12-I1 add) | OPEN — slight regression | round 6 |
| **[R10-Q3]** registry.rs + k8s.rs + docker.rs 45 bare lock-unwrap sites | OPEN — re-verified safe per r13 sampling, see R10-Q3 reassessment | round 4 |
| **[R10-Q2]** `clock_resync_post_restore` 87 LOC + agent `clock_resync` 148 LOC | OPEN — unchanged | round 5 |
| **[R11-A1]** 5-site secret-loader extract | OPEN — NOW BLOCKED on R13-Q2 error-envelope harmonisation | round 3 |
| **[R11-Q3]** sandbox-agent `handlers.rs:592/:777` raw JSON-parse `{e}` to wire body | OPEN — unchanged | round 3 |
| **[R11-Q4]** R9-S4b test fns lack `///` doc comments | OPEN — unchanged | round 3 |
| **[R12-Q2]** T-7 driver-name magic strings (`"raw_exec"`, `"ch"`, `"ch_plugin"`) — now propagated into 2 files | OPEN — got worse, R12-I1 added 2 callsites in restore_handler.rs without extracting first | round 2 |
| **[R12-Q3]** ENV_LOCK duplication — now 3 copies | **ESCALATED to R13-Q1 (CRITICAL)** in r13 | (closed as Q3, reopened as Q1) |
| **[r9 #3]** `stop_sandbox` 241 LOC | OPEN — unchanged | round 6 |
| **[r9 #4]** `main` 272 LOC / `preview_proxy` 270 LOC | OPEN — unchanged | round 5 |
| **[r9 #5]** `clock_resync_post_restore` `Result<(), String>` | OPEN — unchanged | round 6 |

## Hunt-list resolution

| # | Item from brief | Verdict |
|---|---|---|
| 1 | R12-A1 race observation — is cross-module ENV_LOCK race actually exploitable in `cargo test`? | **YES, exploitable**. See R13-Q1 above. Two specific tests in each module race with two in the other (4 races total). Filed as R13-Q1 CRITICAL. |
| 2 | R12-I1's `build_restore_nomad_job_json` add — any clippy-level smells? | **YES, 3 distinct**: (a) `#[allow(clippy::too_many_arguments)]` on 9-param fn, (b) ~120 LOC near-clone of `build_nomad_job_json`, (c) duplicated `format!("zsbx-restore-{}", sandbox_id.simple())` at L1108 + L1169. All folded into R13-Q3. |
| 3 | `format!` allocation audit — Cow/static improvements? | **Partial**: 10 `.display().to_string()` sites improvable via `to_string_lossy()` + `path_to_value` helper. 4 numeric `.to_string()` sites are forced by serde_json. The static-str sites (`"raw_exec"`, `"ch"`) already are `&'static`; alloc happens inside json! and isn't avoidable. Folded into R13-Q4 MINOR. |
| 4 | R12-A2 5th sibling — is the mode constant `0o600` named or magic? | **Magic-numbered**. All 6 mode occurrences (1× `0o600` at db.rs:1172 + 5× `0o400` across the family) are raw octal literals. R13-Q5 MINOR — recommend extracting `SECRET_FILE_MODE_RO` + `SECRET_FILE_MODE_RW` consts paired with R13-Q2's helper. |
| 5 | Error envelope consistency across R9-S4 family | **DIVERGED**. 3 sites return `Result<_, String>`; 2 sites (both in db.rs) return `Result<_, DatabaseError>`. R13-Q2 MAJOR — blocks R11-A1 helper extract until harmonised. |
| 6 | R10-Q3 reassessment — sampled 5 random registry.rs RwLock sites | **Still safe to defer**. Sampled L196, L205, L239, L298-L299, L308, L319, L331, L347, L349 — all are short locks against in-memory state (`HashMap<Uuid, Sandbox>`, `AtomicI64`, etc.), no I/O held, no foreign callbacks fired under lock. r12's "read-only safe, write-only benign" verdict holds at r13. The 31 sites in registry.rs are uniform; counting up to 45 across registry+k8s+docker stays out-of-scope for the concurrency axis. **No action**. |
| 7 | TODO audit — count + new ones from R12-I1 or T-8b commits | **2 prod TODOs** (down from 3): `k8s.rs:495` (cross-ref to restore_from_sealed TODO, pre-r9) + `snapshot_store_gcs.rs:1081` (GCS retry-loop placeholder, GCS phase). `db.rs:494` closed at `46e0fa2a`. **No new TODOs** introduced by R12-I1 (`b3bf741c`) or the T-8b sequence (cluster review commits, not Rust). **Zero TODOs in sandbox-agent** (unchanged). |
| 8 | Try `cargo clippy` | **NOT AVAILABLE** in this nix env (same as r10/r11/r12). Workspace lints gate at CI. |

## R12-A1 race verification

**Question**: is the cross-module `ENV_LOCK` race ACTUALLY exploitable
in `cargo test` (which runs all tests in one process by default)?

**Answer**: **YES**, in 4 concrete test pairings.

The default `cargo test --lib` test binary uses the libtest harness,
which fans out to `std::thread::available_parallelism()` OS threads
within a SINGLE process. Statics in the test binary are shared
across those threads. `SANDBOX_TASK_DRIVER` is a process-wide env
var; mutex-serialisation is local to each `static Mutex<()>` symbol.

**The 4 race pairings**:

| Thread A (acquires) | Thread B (acquires) | Shared resource | Loser symptom |
|---|---|---|---|
| `T7_ENV_LOCK` in `nomad_ch::tests::nomad_job_spec_uses_raw_exec_by_default` | `R12_I1_ENV_LOCK` in `restore_handler::tests::nomad_restore_job_spec_uses_ch_when_flag_set` | env var `SANDBOX_TASK_DRIVER` | A asserts Driver=="raw_exec", gets "ch" — FAIL |
| `T7_ENV_LOCK` in `nomad_ch::tests::nomad_job_spec_uses_ch_when_flag_set` | `R12_I1_ENV_LOCK` in `restore_handler::tests::nomad_restore_job_spec_uses_raw_exec_by_default` | env var `SANDBOX_TASK_DRIVER` | A asserts Driver=="ch", gets "raw_exec" — FAIL |
| `T7_ENV_LOCK` in `nomad_job_spec_uses_raw_exec_by_default` | `R12_I1_ENV_LOCK` in `nomad_restore_job_spec_uses_ch_when_flag_set` (reverse-order) | env var | symmetric — either side can lose |
| `T7_ENV_LOCK` in `nomad_job_spec_uses_ch_when_flag_set` | `R12_I1_ENV_LOCK` in `nomad_restore_job_spec_uses_raw_exec_by_default` (reverse-order) | env var | symmetric |

**Why CI hasn't reported flakes yet** (best guess, unproven):

1. **Small test suite per module**: `nomad_ch::tests` has 7 T-7
   tests, `restore_handler::tests` has 5 R12-I1 tests. The window
   in which both modules have a SANDBOX_TASK_DRIVER-touching
   test simultaneously running is narrow.
2. **Many other slow tests in the same binary**: nomad_ch has
   ~110 tests total, restore_handler has ~30. Each
   SANDBOX_TASK_DRIVER test is fast (~5ms); the *median* test
   is slower, so the SANDBOX_TASK_DRIVER tests tend to complete
   in non-overlapping windows.
3. **CI thread count**: if CI runs `cargo test -- --test-threads=N`
   with low N (e.g. 2), the parallelism window shrinks
   proportionally.
4. **The race needs Thread B's `task_driver_mode_from_env()` call
   to happen DURING Thread A's window between `set_var` and the
   subsequent `remove_var`** (~microseconds wide for the synchronous
   build_*_nomad_job_json branch). Probabilistically low per
   invocation, but multiplied over thousands of CI runs the
   expected interleave count is ≥1.

**Conclusion**: the race is REAL and exploitable today. It is
NOT theoretical — `task_driver_mode_from_env()` reads the env var
without holding either lock. r13 escalates this from r12's MINOR
to CRITICAL.

## R11-A1 helper extract — error-type harmonization blocker

**Surfacing R13-Q2 in actionable form for next-cycle planning.**

The R11-A1 helper extract was scoped in r11 as "4-site secret-loader
extract" with an estimated ~20 LOC helper + ~30 LOC removed.
r12 marked the extract case as "structural — 4 fully-symmetric
copies in 4 modules". r13 sees the symmetry was a lower-resolution
view than r12 indicated:

**The 5 sites' actual signatures**:

```rust
// R9-S4   (snapshot_aead.rs:185)
pub fn from_path(path: &Path) -> Result<Self, String>

// R9-S4b  (persist.rs:333)
pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, String>

// R9-S4c  (db.rs:826)
fn enforce_password_file_mode(path: &str) -> Result<()>
// where Result<T> = std::result::Result<T, DatabaseError>

// R9-S4d  (lib.rs:936)
pub(crate) fn load_admin_token(path: Option<&Path>)
    -> Result<Option<String>, String>

// R11-S2  (db.rs:1161)
fn enforce_host_id_file_mode(path: &Path) -> Result<()>
// where Result<T> = std::result::Result<T, DatabaseError>
```

**5 different signatures, 2 different error types**. Plus:

- 2 different mode values (`0o400` ×4, `0o600` ×1).
- 2 different path types (`&str` ×1, `&Path` / `AsRef<Path>` ×4).
- 3 sites RETURN the secret (loaded data); 2 just validate
  (enforce_*).

**Harmonisation plan** (recommended sequencing for next cycle):

1. **First commit**: introduce `crates/sandbox/src/secret_file.rs`
   carrying:
   - `SecretFileError` enum + `Display` + `From<...> for String`
     + `From<...> for DatabaseError`.
   - `SECRET_FILE_MODE_RO = 0o400`, `SECRET_FILE_MODE_RW = 0o600`.
   - `fn check_root_owned_secret_file(path: &Path, expected_mode: u32)
     -> Result<std::fs::Metadata, SecretFileError>` (the helper).
   - 5 tests pinning the helper's behaviour (positive root-owned,
     wrong mode, wrong uid, missing file, stat permission denied).
   - Estimated ~120 LOC including tests.

2. **Second commit (per site)**: convert each of the 5 callers
   to use the helper. Each per-site commit is ~25-30 LOC removed,
   ~5 LOC added, ~3 existing tests left intact (they pin the
   per-site behaviour, not the helper). 5 commits total, ~125
   LOC net removed.

3. **Third commit**: remove the 5 sites' now-unused local imports
   (`PermissionsExt`, `MetadataExt`), confirm clippy passes.

**Net**: ~120 LOC added (helper module) + ~125 LOC removed (5 sites)
= **net 0-5 LOC**. The win is the 5-place audit collapsing to a
1-place audit, plus the named-mode-constant readability, plus a
single point where future security tightening (e.g. checking gid,
or refusing setuid bit) lands.

**Risk**: low. Each per-site commit is independently testable
against the existing tests; if any conversion changes behaviour
the existing tests catch it. The helper module is greenfield —
no integration risk.

## R10-Q3 reassessment (sampled 5 random sites)

Per r12 the registry.rs RwLock pattern was marked "out-of-scope
for concurrency per r12" with the rationale "read-only safe,
write-only benign". r13 brief asked: re-verify by sampling 5
random sites post-R12 cycle.

**Sampled sites** (from `crates/sandbox/src/registry.rs`):

| Line | Operation | Lock scope | Verdict |
|---|---|---|---|
| L196 | `let last = *self.last_used.read().unwrap();` — read Instant under read-guard | <1µs, no I/O | Safe |
| L239 | `let guard = self.by_sandbox.read().unwrap(); let s = guard.get(id)?; s.touch(); Some(s.current_info())` — lookup + clone | <5µs, no I/O; `touch()` takes a separate RwLock<Instant> internally | Safe — nested lock acquisition is short-fast-path |
| L298-L299 | `self.by_sandbox.write().unwrap().insert(...)` + `self.by_user_project.write().unwrap().insert(...)` — paired writes | <10µs, no I/O | Safe — but **note the 2-lock acquisition order**: by_sandbox → by_user_project. No other site acquires them in the reverse order (verified via grep), so AB-BA deadlock not reachable. |
| L319 | `let guard = self.by_sandbox.read().unwrap(); if let Some(s) = guard.get(id) { s.generation.store(value, Ordering::Relaxed); }` — atomic store under read-guard | <2µs | Safe |
| L347-L349 | `let guard = self.by_sandbox.read().unwrap(); if let Some(s) = guard.get(&id) { *s.preview_secrets.write().unwrap() = secrets; ...` — nested write under outer read | <10µs, no I/O | Safe but **2-level lock**: outer read on `by_sandbox`, inner write on `preview_secrets`. No reverse-order site, no AB-BA. |

**Verdict**: r12's out-of-scope marking holds. Every sampled
site is bounded by a few microseconds of in-memory work, no
I/O held under any lock, no foreign callbacks fired under lock,
no reverse-order lock acquisition that could deadlock. The
`.unwrap()` is reachable only on PoisonError, which itself is
reachable only if a prior holder panicked — and the few sites
that do reach panic paths (panic in `Drop`, OOM) are
unrecoverable anyway.

**No action**. The 31 registry.rs sites + 14 k8s.rs/docker.rs
sites are a stylistic preference (use `.unwrap_or_else(|p|
p.into_inner())` for poison-recover) not a correctness fix.

## Trend

- **TODOs**: 2 (sandbox prod) + 0 (sandbox-agent) = **2 total**.
  Delta from r12: **−1** (db.rs:494 closed at 46e0fa2a). The
  remaining 2 (k8s.rs:495, snapshot_store_gcs.rs:1081) have
  outlived r12 unchanged.
- **`pub fn` count**: 303 (sandbox) + 69 (sandbox-agent) = **372 total**.
  Delta from r12: **+5** (303−299 in sandbox, 69−68 in sandbox-agent).
  Growth from R12-I1's `build_restore_nomad_job_json` +
  R11-S2's `enforce_host_id_file_mode` + 3 helpers.
- **Test trajectory**: +3 sandbox lib (319 → 322 grep'd), +0
  sandbox-agent (methodology discrepancy noted; r12's 242 was
  cargo-test-runs, r13's 177 is attribute-grep). Per-commit
  testing remains healthy: R12-I1 shipped 5 tests with its 313
  LOC of new logic (1:1 body-to-test ratio is leaner than T-7's
  1:6 but still acceptable for a near-clone of an
  already-tested fn).
- **LOC trajectory** (sandbox src/):
  - r9: 17,847 LOC
  - r10: 17,901 LOC (+54)
  - r11: 17,924 LOC (+23)
  - r12: 18,109 LOC (+185)
  - **r13: 18,612 LOC (+503)** — R12-I1 +313 + R11-S2 +190 + small
  edits. Rate of growth is sustainable; the +503 is concentrated
  in 2 targeted invariant fixes, not sprawl.

## Inertia table

| Finding | First raised | Rounds open | Round-count signal |
|---|---|---|---|
| **R10-Q7 / R11-Q5** `register_restored` default Ok(()) | R5-Q1 (round 5) | **9** | Round-9. Mechanical fix (~10 LOC). The "we'll get to it" cost has been paid 9 times. |
| **R10-Q6** central timeouts mod | r9 #7 + earlier | 6 | 70 → 71 literals. Each round the count creeps up. |
| **r9 #3** stop_sandbox 241 LOC | r9 #3 | 6 | — |
| **r9 #5** clock_resync_post_restore Result<(), String> | r9 #5 | 6 | — |
| **R10-Q2** clock_resync 147 LOC | r9 #2 | 5 | — |
| **R10-Q4** sig.rs:120 hyphenated UUID | r9 api-surface #2 | 5 | One-line doc edit. Not closing it is itself the signal. |
| **r9 #4** main 272 / preview_proxy 270 | r9 #4 | 5 | — |
| **R10-Q3** registry bare-lock-unwrap (45 sites) | r10 | 4 | Re-verified out-of-scope at r13 per sampling. |
| **R11-A1** secret-loader extract | r11 | 3 | All 5 sites now shipped. **NEWLY BLOCKED** by R13-Q2 error-type divergence. |
| **R11-Q3** sb-agent JSON parse {e} | r11 | 3 | Acknowledge-or-route. |
| **R11-Q4** test fn doc-comments | r11 | 3 | Stylistic. |
| **R12-Q2** T-7 driver-name magic strings | r12 | 2 | R12-I1 propagated `"raw_exec"`+`"ch"` into a 2nd file without extracting first; cost grew. |
| **R12-Q3 → R13-Q1** 3rd copy env-mutating-test pattern | r12 | (escalated) | 3-copy threshold crossed; now CRITICAL race. |
| **R13-Q1** cross-module ENV_LOCK race | r13 | 1 | NEW CRITICAL. |
| **R13-Q2** error envelope divergence (3 String vs 2 DatabaseError) | r13 | 1 | NEW MAJOR. Blocks R11-A1. |
| **R13-Q3** 181 LOC duplicated builder + 9-arg `#[allow(clippy::too_many_arguments)]` | r13 | 1 | NEW MINOR. |
| **R13-Q4** path.display().to_string() ×10 in new fn | r13 | 1 | NEW MINOR. |
| **R13-Q5** raw 0o400 / 0o600 mode literals across 6 sites | r13 | 1 | NEW MINOR. Pairs with R13-Q2. |

## Score derivation

r12 = 78/100. Deltas:

- +3 R12-Q1 closed at `46e0fa2a` (comment-only fix that nonetheless
  closes the 20-day-stale misleading-deferral signal; clean execution
  on the recommended option (2) from r12's R12-Q1).
- +2 R10-Q5 closed at `0cc7af52` (round-4 dead-fn carry, 14 LOC
  delete — clean execution).
- +1 R11-S2 closed at `85e4f2f9` (5th sibling uid check shipped).
- −3 R13-Q1 (CRITICAL — cross-module ENV_LOCK race is a real
  correctness bug in test scaffolding, not a maintenance smell).
  R12-Q3 was a MINOR carry at r12; r13 escalates after R12-A1's
  architectural surfacing and r13's hunt-list verification.
- −2 R13-Q2 (MAJOR — error envelope divergence blocks the 3-round
  R11-A1 carry, and R11-S2 INTRODUCED the divergence without
  matching the sibling shape).
- −1 R13-Q3 (MINOR — R12-I1's duplicated 181-LOC builder + 9-arg
  `#[allow(clippy::too_many_arguments)]` + duplicated job_id
  format string).
- −0.5 R13-Q4 (MINOR — path.display().to_string() ×10 readability,
  not perf-improvable in practice).
- −0.5 R13-Q5 (MINOR — raw 0o400/0o600 across 6 sites).
- −1 R10-Q7 / R11-Q5 (round 9, no movement).
- −1 the cluster of round-5+ minor carries (R10-Q4, R10-Q6, r9 #3,
  r9 #5).

Net: 78 + 3 + 2 + 1 − 3 − 2 − 1 − 0.5 − 0.5 − 1 − 1 = **76/100**.

The 2-point regression reflects:
1. R12-I1 shipped the intended R12-A1 fix (T-8 blocker resolved
   architecturally) — but did so by introducing a CRITICAL test-
   scaffolding race and a 181-LOC near-duplicate of an existing
   180-LOC builder.
2. R11-S2 shipped the intended uid-check sibling — but did so
   with the WRONG error envelope, breaking the 4-site convention
   established by R9-S4/S4b/S4d.

Both are "correct fix delivered, code-quality side-effect
underestimated". The +6 from closures and the −7 from new
findings net to −1; layered onto −1 from round-9 inertia gives
**76**.

## Recommendations for the next cycle

In rough impact-per-LOC order:

1. **R13-Q1** — single-mutex consolidation for SANDBOX_ENV_LOCK
   (~30 LOC + 3 call sites). **CRITICAL** test-flake bug. Highest
   priority.
2. **R10-Q7 / R11-Q5** — `register_restored` default removal
   (~10 LOC, 3 impls touched). Closes round-9 carry; same shape
   as R7-S2. Mechanical.
3. **R13-Q2 + R13-Q5 + R11-A1** — single combined commit:
   introduce `secret_file.rs` module with `SecretFileError`,
   named mode constants, and the `check_root_owned_secret_file`
   helper. Convert 5 sibling sites. ~125 LOC removed, ~120 LOC
   added (net 0), 5-place audit → 1-place audit. **Closes 3
   findings.**
4. **R10-Q4** — sig.rs:120 hyphenated UUID doc-edit (1 LOC).
   Round 5. Mechanical.
5. **R12-Q2** — T-7 driver-name string consts (`"raw_exec"`,
   `"ch"`, `"ch_plugin"`) extraction (~15 LOC + ~20 LOC test
   updates). Quick win; got worse this round, fix before R-N adds
   a 3rd file.
6. **R13-Q3** (deferred until 1st divergence-bug surfaces) —
   `JobspecKind` enum + unified builder. ~200 LOC of restructure,
   no net new logic. Wait for the divergence to bite once before
   investing.
7. **R13-Q4** — `path_to_value(&Path) -> serde_json::Value`
   helper extraction. ~15 LOC. Pure readability.

Items 1-5 total ~210 LOC of diff for **1 critical race fix + 1
round-9 carry close + 3 finding resolutions (Q2/Q5/A1 combined) +
1 doc fix + 1 quick consts extraction**. Very high ROI batch.

Items 6-7 are non-blocking deferrals.
