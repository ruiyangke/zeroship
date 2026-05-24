# Sandbox/snapshot-restore — architecture r12 review

Date: 2026-05-25 (UTC)
HEAD at audit: `46e0fa2a` (the prompt pinned `9f1dfc99` but `46e0fa2a`
sits one commit past it as a pure-doc R12-Q1 refresh; the r12 deltas
below treat the audit point as `46e0fa2a`, with `9f1dfc99` carrying
the last code-bearing change to scripts and `b3bf741c` carrying the
last code-bearing change inside `crates/sandbox/**`).
Round 12 of N (architecture lens).
Prior round: `sandbox-snapshot-restore-architecture-2026-05-25-r11.md`
at `4f441a20`.

Scope read: `crates/sandbox/**`, `crates/sandbox-agent/**` only.

## Summary

**5 NEW findings (1 critical, 2 important, 2 minor).** The 6 commits
landed since r11 (`5fe36805` T-7 jobspec flag, `b4c3ef27` R9-S4d,
`85e4f2f9` R11-S2 host_id, `b85c1edd` cluster doc, `b4500576` scripts,
`b3bf741c` R12-I1, `9f1dfc99` scripts, `46e0fa2a` doc-only) include
**zero structural movement against the 6-cycle carry-forward stack.**
T-7 / R12-I1 landed the working-tree-flagged delta r11 predicted —
**now committed** at 5371 LOC for `nomad_ch.rs` (was 4923 at r11
HEAD) plus a **NEW second TaskDriverMode-branched builder** at
`restore_handler.rs:1276-1457` (181 LOC, mostly mirroring the cold-
boot builder at `nomad_ch.rs:2311-2502`). R11-A2's prediction —
"split nomad_ch.rs at the existing seams NOW, before T-8" — has
become more urgent: the duplication has crossed module boundaries
without the trait/module refactor that R11-A2 said should land first.
**R11-S2 (host_id reader mode+uid check at `db.rs:1161`) added a
FIFTH copy-pasted root-owned-secret-file loader** — the R11-A1 helper
extraction's call-site count is now **5, not 4** (R9-S4d closed the
4th sibling, but R11-S2 immediately reopened the seam at a 5th site).

Counter-evidence: R9-S4d (load_admin_token uid check at `lib.rs:957`,
`b4c3ef27`) and R11-S2 (host_id reader at `db.rs:1161`, `85e4f2f9`)
both **closed gaps by copying the same 6-line block** instead of
extracting it once. Per r11's framing, this is the dominant copy-
paste anti-pattern in the controller crate; it has now grown by 1
caller and gained a second mode (0o600 alongside the existing 0o400).

## Module size table (compared to r11 baseline at `4f441a20`)

| File | r11 LOC | r12 LOC | Δ | Action |
|---|---:|---:|---:|---|
| `crates/sandbox/src/backend/nomad_ch.rs` | 4923 | **5371** | **+448** | R10-A4 carry-forward + R11-A2 — T-7's working-tree delta is now committed. **Crossed the 5000-LOC boundary.** First file in the crate to do so. |
| `crates/sandbox/src/db.rs` | 3108 | **3298** | **+190** | R10-A1 carry-forward; +190 LOC for R11-S2's host_id reader (`enforce_host_id_file_mode` + 3 tests) — a NEW 5th secret-loader site. |
| `crates/sandbox/src/restore_handler.rs` | 2367 | **2680** | **+313** | **NEW THRESHOLD**: crossed 2500 LOC. R12-I1 added the parallel TaskDriverMode builder + the `r12_i1_tests` module (157 LOC of tests with a distinct `R12_I1_ENV_LOCK` mutex and duplicate `with_task_driver_env` helper). See R12-A1. |
| `crates/sandbox/src/lib.rs` | 2267 | **2412** | **+145** | R9-S4d landed the uid==0 check + 2 tests at `:944-960` and `:1083-1146`. R4-A1 still open: 7 `with_*` + `new_fixture` unchanged (`grep -nE "^\s*pub fn with_\|^\s*pub fn new_fixture" lib.rs` = 8). |
| `crates/sandbox-agent/src/handlers.rs` | 2224 | 2224 | 0 | Out of architecture scope. |
| `crates/sandbox/src/admin_handlers.rs` | 1781 | 1781 | 0 | T9/T10/R10-A7 carry-forward. |
| `crates/sandbox/src/snapshot_store_gcs.rs` | 1640 | 1663 | +23 | r11-P2/P3 BufReader/BufWriter closure (per r11 perf review). |
| `crates/sandbox-agent/src/sig.rs` | 1590 | 1590 | 0 | Out of arch scope. |
| `crates/sandbox/src/backend/k8s.rs` | 1580 | 1580 | 0 | Out of arch scope. |
| `crates/sandbox/src/handlers.rs` | 1371 | 1371 | 0 | r11-flagged err_safe sites stable. |
| `crates/sandbox-agent/src/proxy.rs` | 1331 | 1331 | 0 | Out of arch scope. |
| `crates/sandbox/src/snapshot_aead.rs` | 1277 | 1277 | 0 | Healthy. |
| `crates/sandbox/src/persist.rs` | 1216 | 1216 | 0 | R9-S4b sibling stable. |
| `crates/sandbox/src/config.rs` | 1051 | 1051 | 0 | `pub fn new_fixture` carry-forward. |

**Two files crossed thresholds this cycle**: `nomad_ch.rs` crossed
5000 (now 5371), `restore_handler.rs` crossed 2500 (now 2680).
**Combined `nomad_ch.rs + restore_handler.rs = 8051 LOC**, both
holding `TaskDriverMode` machinery. The seam is now load-bearing
across two oversized files.

## Findings (NEW since r11)

### [R12-A1] R12-I1's wake-path TaskDriverMode add (b3bf741c) deepened the cold-boot ↔ wake-path duplication — approach (b)'s parallel-branch shape produced a near-byte-for-byte mirror of `build_nomad_job_json_with` in a SECOND module, with a SECOND env-mutex that does NOT synchronise with the first (CRITICAL, architecture-r12)

- **Files**:
  - Cold-boot builder: `crates/sandbox/src/backend/nomad_ch.rs:2311-2502`
    (`build_nomad_job_json_with`, ~190 LOC).
  - Wake-path builder: `crates/sandbox/src/restore_handler.rs:1276-1457`
    (`build_restore_nomad_job_json`, ~181 LOC).
  - Cold-boot test env-lock: `crates/sandbox/src/backend/nomad_ch.rs:4073`
    (`T7_ENV_LOCK: Mutex<()>`).
  - Wake-path test env-lock: `crates/sandbox/src/restore_handler.rs:2478`
    (`R12_I1_ENV_LOCK: Mutex<()>`).
  - Cold-boot test helper: `crates/sandbox/src/backend/nomad_ch.rs:4082`
    (`with_task_driver_env(value, f)`).
  - Wake-path test helper: `crates/sandbox/src/restore_handler.rs:2485`
    (`with_task_driver_env(value, f)` — same name, different module).
  - Commit `b3bf741c`'s message confirms approach (b): *"Approach (b)
    over (a): the wake-path builder differs from cold-boot in
    substantive ways (no ZSBX_SANDBOX_ID, no ZSBX_PUBKEY_HEX,
    restore-specific Meta, memory/cpus passed externally to match
    snapshot-saved values). Folding into the cold-boot builder would
    broaden the Env contract under restore for marginal dedup."*
- **Symptom 1 — code duplication**. A diff of the two `match mode` blocks
  (`nomad_ch.rs:2402-2464` vs `restore_handler.rs:1371-1419`)
  finds them structurally identical: same `(&str, serde_json::Value)`
  pair return, same `RawExec → ("raw_exec", json!({"command": …}))`
  arm, same `ChPlugin → ("ch", json!({"vm_index"…, "kernel"…, …,
  "disks": [], "fs": [], "net": []}))` arm. The differences enumerated
  by the commit message are ALL in the **caller-supplied parameters**
  (`pubkey_hex` arg = `""` on restore, `sandbox_id.simple()` on
  restore, the missing Meta field) — the **shape** of the dispatch
  block is duplicated verbatim. The Env block (`nomad_ch.rs:2334-2374`
  vs `restore_handler.rs:1334-1349`) is similarly near-identical
  (12 of 15 keys are the same; restore drops `ZSBX_PUBKEY_HEX` +
  `ZSBX_SANDBOX_ID` and adds `ZSBX_RESTORE_FROM`). The Resources block
  (`nomad_ch.rs:2382-2397` vs `restore_handler.rs:1351-1363`) is
  identical except CPU is hardcoded 500 in restore vs
  `NOMAD_CPU_MHZ_ADVISORY` (also 500) in cold-boot.
- **Symptom 2 — TEST surface duplicated, with a CORRECTNESS BUG**:
  the wake-path test module at `restore_handler.rs:r12_i1_tests`
  defines its own `R12_I1_ENV_LOCK` and its own
  `with_task_driver_env`. Cargo runs tests in `nomad_ch::tests` and
  `restore_handler::r12_i1_tests` **on the same process / different
  threads in parallel by default**. Both modules `set_var` and
  `remove_var` on the SAME process-global env table for the SAME key
  `SANDBOX_TASK_DRIVER`, but their locks are different `Mutex<()>`
  instances — they do NOT serialise against each other. The comment
  at `restore_handler.rs:2473-2477` documents the choice:
  *"We don't share the same mutex symbol across crates
  (`nomad_ch::tests::T7_ENV_LOCK` is `pub(crate)`-scoped to that
  test mod), but we DO need serialisation against tests in this
  module — define a module-local mirror."* This explains the
  intent but accepts the unsynchronised cross-module race as a
  deliberate trade-off. Under `cargo test --lib -p zeroship-sandbox`
  with the default 2+ test-thread pool, a cold-boot test and a wake
  test can interleave their `set_var`/`remove_var` calls, producing
  flaky `Driver` assertions. The current pass rate is incidental —
  there are only 2-3 env-touching tests per module and the windows
  are small. **Adding a third caller (e.g., a sweep-path job
  rebuilder, an admin-CLI dry-run) without a unified lock makes
  flake inevitable.**
- **Why CRITICAL**: this is the same shape as R3-A1's
  Backend-enum-5-Err-returners and R11-A1's 4-site secret-loader —
  a pattern duplicated across the codebase that should be one
  function. The cost is higher than the secret-loader case because:
  (1) the duplication is ~200 LOC per copy (not ~20); (2) the test
  duplication has a real race-condition hazard, not just stylistic
  inconsistency; (3) post-T-8 cleanup must touch BOTH copies (delete
  the RawExec arm from both builders, delete the env block from
  both, drop the wrapper_path field reference from both); (4) every
  future per-VM input that lands under ChPlugin must be threaded
  through TWO typed Configs in TWO modules — the Go driver's
  `TaskConfig` schema evolves once but the controller-side has to
  mirror it in two places. The fixer's approach-(b) rationale is
  reasonable for the diff under review, but the right shape is
  **approach (c): a shared `build_jobspec` lower-level helper
  carrying `JobspecRequest { mode, vm_index, env_extra,
  pubkey_hex, …, restore_from: Option<&Path> }`** in a NEW
  `crates/sandbox/src/backend/nomad_ch/jobspec.rs` module — the
  same module R11-A2's split sketch already named.
- **Action**:
  1. **Promote R11-A2** (nomad_ch.rs module split → `jobspec/{mod,
     rawexec, chplugin, common}.rs`) **from "IMPORTANT" to "T-8
     prerequisite"**. The two-builder shape is now BLOCKING T-8
     cleanup: post-T-8, the RawExec deletion must touch both
     modules AND the test fixtures in both. Doing the split first
     reduces T-8 to "delete `rawexec.rs`, the wake-path's match
     arm becomes a no-op". Doing T-8 first means a 5-zone surgical
     edit across two oversized files.
  2. **Unify the env-lock**: move `SANDBOX_TASK_DRIVER_ENV_LOCK`
     out of either test mod into a `pub(crate)` symbol in a new
     `crates/sandbox/src/backend/nomad_ch/jobspec.rs` (the natural
     home, alongside `TaskDriverMode`). Both test mods reference
     the same `Mutex<()>`. This closes the cross-module race
     trivially as a side-effect of step 1.
  3. **Approach (c) at the function level**: collapse the two
     builders into a single `build_nomad_job_json_for(req:
     JobspecRequest)` carrying ~10 fields. Cold-boot calls it with
     `restore_from = None`, `meta_kind = "create"`, `pubkey_hex =
     Some(&hex)`. Wake calls it with `restore_from = Some(&dir)`,
     `meta_kind = "restore"`, `pubkey_hex = None`. The Env block
     becomes a single `make_env(req)` call; the dispatch block
     becomes a single `make_driver_config(req)` call. Estimated
     cost: ~280 LOC of new helper, ~330 LOC removed across the two
     builders + their tests. Net ~**-50 LOC across the crate**,
     -50% of the post-T-8 cleanup surface, eliminates the cross-
     module env-lock race entirely.

### [R12-A2] R11-S2's `enforce_host_id_file_mode` (db.rs:1161) is the FIFTH copy-pasted root-owned-secret-file loader — R11-A1's helper extraction is now mandatory, not optional (IMPORTANT, architecture-r12)

- **Files**:
  - 1st site: `crates/sandbox/src/snapshot_aead.rs:185-217`
    (`RootKek::from_path`, 0o400 + uid==0; closed R9-S4a).
  - 2nd site: `crates/sandbox/src/persist.rs:333-369`
    (`AeadKey::from_path`, 0o400 + uid==0; closed R9-S4b at
    `e4e5db60`).
  - 3rd site: `crates/sandbox/src/db.rs:826` (`enforce_password_file_mode`,
    0o400 + uid==0; closed R9-S4c at `2c10f63a`).
  - 4th site: `crates/sandbox/src/lib.rs:936-977` (`load_admin_token`,
    0o400 + uid==0; closed R9-S4d at `b4c3ef27` **this cycle**).
  - **5th site (NEW this cycle)**: `crates/sandbox/src/db.rs:1161-1188`
    (`enforce_host_id_file_mode`, **mode 0o600** + uid==0; landed
    R11-S2 at `85e4f2f9`).
- **Symptom**: r11 documented the duplication at 3 sites with a 4th
  pending (R9-S4d). The closing trajectory: r11→r12 closed R9-S4d
  (the 4th site) at `b4c3ef27` by **copying the same 6-line block a
  fourth time**, and simultaneously opened a 5th site (R11-S2,
  `85e4f2f9`) with a **6-line block that varies the mode constant
  from 0o400 to 0o600** but is otherwise structurally identical.
  Three of the five sites now share a doc-comment lineage:
  - `persist.rs:325-332` → "Mirrors `RootKek::from_path`".
  - `db.rs:812-823` (`enforce_password_file_mode`) → "Mirrors
    `persist::AeadKey::from_path`".
  - `db.rs:1149-1160` (`enforce_host_id_file_mode`) → "matches the
    R9-S4 family invariant (snapshot KEK, sealed-records AEAD key,
    pg-password file, admin token) and the systemd-style root-secret
    convention."
  - `lib.rs:907-935` (`load_admin_token`) → "Mirrors
    `Persistence::AeadKey::from_path`".

  Every new site documents the existing siblings; nobody extracts
  them. The 5th site validates R11-A1's prediction that *"a fifth
  secret-file caller is already plausible — e.g., the forthcoming
  K8s service-account token loader, a Stripe webhook signing-key
  loader, or any operator-private bearer added during the cluster-
  driver scaffold work. Each one will copy the pattern by
  precedent."* The pattern is now indexed in 3 ways:
  - **By mode**: 4 × 0o400 (KEK, AEAD key, pg-password, admin
    token), 1 × 0o600 (host_id).
  - **By expected length**: 2 × fixed-32 (KEK, AEAD key), 1 × free-
    length-string (admin token), 1 × validation-only-no-return
    (pg-password), 1 × validation-only-no-return (host_id).
  - **By error type**: `Result<_, String>` × 3 (KEK, AEAD key, admin
    token), `Result<(), DatabaseError>` × 2 (pg-password, host_id).
- **Why it matters**: R11-A1 was already CRITICAL; the 5th site
  promotes it to **MANDATORY before any further root-owned secret-
  file caller lands**. The recommended `read_root_owned_secret_file`
  surface from r11 needs ONE addition to handle the host_id case
  (`expected_mode: u32 = 0o400 | 0o600`) — that's the only axis r11
  didn't anticipate. Updated sketch:

  ```rust
  // crates/sandbox/src/secret_io.rs
  pub(crate) fn read_root_owned_secret_file(
      env_name: &str,
      path: &Path,
      expected_mode: u32,             // NEW vs r11: 0o400 (most) | 0o600 (host_id)
      expected_len: Option<usize>,
  ) -> Result<Vec<u8>, String> { … }
  ```

  Five callers convert mechanically; the 5th site picks the helper
  for free.
- **Action**: Same as r11 R11-A1, with one parameter addition
  (`expected_mode`). Cost ~80 LOC new module, ~150 LOC removed
  across 5 sites; closes R11-A1 + R11-Q2 + makes future caller #6
  a 3-line wrapper. **The cost-of-doing has not changed; the cost-
  of-not-doing grew by 1 caller (host_id) + 1 axis (mode constant).**

### [R12-A3] `TaskDriverMode` should be a field on `NomadCHBackend` (and on `RealRestoreBackend`), populated once at construction — subsumes R12-M2 with the architectural shape (IMPORTANT, architecture-r12)

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:150-171` — `NomadCHBackend`
    struct (no `task_driver_mode` field today).
  - `crates/sandbox/src/restore_handler.rs:934-982` — `RealRestoreBackend`
    struct (no `task_driver_mode` field today).
  - Production call sites of `task_driver_mode_from_env()`:
    - `crates/sandbox/src/backend/nomad_ch.rs:2301` (called from
      `build_nomad_job_json`, invoked once per `NomadCHBackend::
      create` at `nomad_ch.rs:718`).
    - `crates/sandbox/src/restore_handler.rs:1116` (called from
      `RealRestoreBackend::submit_restore_job` at
      `restore_handler.rs:1101`, invoked once per restore).
- **Symptom**: every CREATE and every RESTORE reads
  `std::env::var("SANDBOX_TASK_DRIVER")` afresh. Under libstd, env
  reads acquire a global mutex around the env-table
  (`std::sys::pal::unix::os::ENV_LOCK`). The concurrency-r12 review
  (R12-M2) flagged this from the perf-and-footgun lens; here it
  surfaces as an **architectural smell**: the flag is a deployment-
  time invariant of the controller process (set once in
  `gcp-worker-startup.sh:442` via the systemd unit's `Environment=`),
  but the code treats it as a runtime-mutable input. The two backend
  structs hold every other resolved-at-boot input as a field
  (`cfg`, `memory_mb`, `cpus`, `alloc_running_timeout`,
  `agent_livez_timeout`, `shared_allocator`, `nomad_handle`,
  `persist`) — `task_driver_mode` is the lone exception.
- **Why it matters**:
  1. **Single source of truth**: the field replaces a "two places
     might disagree" shape with a "construct once, immutable
     forever" shape. The current code already has the *theoretical*
     split-brain hazard the concurrency-r12 review flagged (env
     mutates between a CREATE and a same-backend RESTORE moments
     later → mixed-driver allocs).
  2. **Subsumes R12-M2** (concurrency-r12 MINOR): R12-M2 is the
     same finding through the libstd-env-mutex lens. Both fixes
     are the same 3-line change. The architecture lens makes the
     case stronger: this is part of the broader **"resolve env at
     construction, never read it from a handler"** invariant the
     controller follows elsewhere (`SandboxConfig::from_env` at
     `config.rs:340-540` is the canonical example).
  3. **Aligns with the eventual R10-A4 split**: if `TaskDriverMode`
     becomes a struct field, R10-A4's `jobspec/mod.rs` dispatcher
     no longer carries the env-reader at all — the field travels
     with the backend handle and the dispatcher is pure logic.
- **Action**:
  1. Add `pub(crate) task_driver_mode: TaskDriverMode` to
     `NomadCHBackend` (constructor path: `nomad_ch.rs::NomadCHBackend::
     new` — populate from `task_driver_mode_from_env()` once).
  2. Add same to `RealRestoreBackend` (constructor path: wherever
     `RealRestoreBackend { … }` is built; `crate::AppState::from_config`
     can pass the value through, OR — preferred — have
     `RealRestoreBackend::with_nomad_handle` read it from
     `nomad_handle.task_driver_mode` so the two structs stay in
     lock-step).
  3. Drop `build_nomad_job_json` (the env-reading wrapper at
     `nomad_ch.rs:2279`) entirely; callers pass the field
     explicitly to `build_nomad_job_json_with`.
  4. Drop the production env read at `restore_handler.rs:1116`;
     `submit_restore_job` reads `self.task_driver_mode` instead.

  Net: ~3-5 LOC delta; closes R12-M2 + closes the theoretical
  split-brain footgun; reduces the env-table mutex pressure to
  zero on the hot CREATE/RESTORE paths.

### [R12-A4] Post-T-7-commit `nomad_ch.rs` crossed 5000 LOC — R11-A2's "split before T-8" recommendation is now T-8 prerequisite, not just an ordering preference (IMPORTANT, architecture-r12)

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs` at 5371 LOC (HEAD
    46e0fa2a), +448 LOC since r11 baseline (4923 at 4f441a20).
  - The +448 LOC is exactly T-7's working-tree delta r11 measured
    at +149 LOC of feature code PLUS +299 LOC of tests (`grep -c
    "    #\[test\]" crates/sandbox/src/backend/nomad_ch.rs` is the
    measure, not run here; the test growth lives at
    `nomad_ch.rs:4099-4327` — 7 new tests around `TaskDriverMode`
    and the typed `ch` Config shape).
- **Symptom**: r11 predicted the file would land at 5072 LOC
  committed; it shipped at 5371 LOC (the wake-path tests in
  `restore_handler.rs:r12_i1_tests` were anticipated to live in
  `nomad_ch.rs` per the cold-boot pattern, but the R12-I1 fixer
  chose to keep them with the wake-path builder — a cohesion-
  appropriate choice but it means r11's 5072 prediction is now
  spread across two oversized files instead of one). r11 had said
  *"do R10-A4 first, do T-8 second, and T-8 becomes `rm
  crates/sandbox/src/backend/nomad_ch/jobspec_rawexec.rs` + delete
  3 callers."* That ordering recommendation is now mandatory:
  - The cold-boot RawExec arm at `nomad_ch.rs:2402-2408` references
    `cfg.nomad_ch.wrapper_path`.
  - The wake-path RawExec arm at `restore_handler.rs:1372-1377`
    references `cfg.wrapper_path`.
  - The `ZSBX_*` env block construction lives in 2 places now
    (`nomad_ch.rs:2334-2374` + `restore_handler.rs:1334-1349`).
  - The bash-wrapper line-number regression tests live in 2 places
    (`nomad_ch.rs:3874-4016` + the wake-path now has its own at
    `restore_handler.rs:2451-2680`).
  - T-8 deletion in this shape: 8 zones across 2 files vs. r11's
    6 zones across 1 file.
- **Why important**: r11 framed this as an ordering preference
  ("doing R10-A4 first is the better lever"); after R12-A1's
  duplication-across-modules shape, doing T-8 in the current
  layout means touching:
  1. RawExec arm in `nomad_ch.rs`.
  2. RawExec arm in `restore_handler.rs`.
  3. Env block in `nomad_ch.rs`.
  4. Env block in `restore_handler.rs`.
  5. Wrapper-line-pin tests in `nomad_ch.rs`.
  6. Wrapper-line-pin tests in `restore_handler.rs`.
  7. `cfg.nomad_ch.wrapper_path` field removal in `config.rs`.
  8. The wrapper script delete.

  Without R10-A4 + R12-A1's collapse, T-8 is an 8-zone surgical
  edit across 3 modules at 5371 + 2680 + 1051 = 9102 LOC. With
  the split-and-collapse, T-8 is "delete `jobspec/rawexec.rs` (a
  single file ~150 LOC)" + "delete the wrapper script."
- **Action**: Same as r11 R11-A2, with the new urgency level.
  The T-8b-smoke retry at `b85c1edd` (FAIL → fixed in `b4500576`
  + `9f1dfc99`) still hasn't validated the ch_plugin path end-to-
  end on cluster; the next cluster cycle is the natural
  validation gate for "is T-8 deletable yet?". Land R10-A4 +
  R12-A1's collapse in the same window. Estimated cost: ~250 LOC
  of mechanical movement (R10-A4) + the R12-A1 jobspec collapse
  (~330 LOC of code dedup; ~280 LOC of new shared helper).

### [R12-A5] Sweep recovery layer remains in `db.rs` + `sweep.rs` + inline in `restore_handler.rs`; R11-A4 / R10-A1 untouched; `db.rs` crossed 3300 LOC (MINOR, architecture-r12)

- **Files**:
  - `crates/sandbox/src/db.rs:2569` — `claim_orphan_transient_for_recovery`
    (the §6.1 / §9.2 recovery CAS, still in `db.rs`).
  - `crates/sandbox/src/db.rs:2407` (approx) —
    `transient_state_lease_expired_sandboxes` (partner query).
  - `crates/sandbox/src/sweep.rs:108` — `recovery_target` (status
    map; still in `sweep.rs`).
  - `crates/sandbox/src/restore_handler.rs:332` — `read_snapshot_row`
    (still inline outside `db.rs`).
  - New: `crates/sandbox/src/db.rs:1161` —
    `enforce_host_id_file_mode` (R11-S2; the host_id reader that
    underpins `claim_orphan_transient_for_recovery`'s self-host
    fence). This adds a 6th item to the "recovery-adjacent code
    scattered across 3 files" surface that R10-A1 / R11-A4 already
    flagged.
- **Symptom**: r10-A1 / r11-A4 recommended a `recovery.rs` module
  carrying `transient_state_lease_expired_sandboxes` +
  `claim_orphan_transient_for_recovery` + lifted
  `read_snapshot_row` + lifted `recovery_target`. r11 → r12 added
  `enforce_host_id_file_mode` (the read-side of the self-host
  fence) — this **belongs in the same `recovery.rs` module by
  cohesion** (it's the host-identity enforcement that makes the
  recovery CAS' self-host fence meaningful). The longer this
  sits, the more recovery-adjacent code accretes onto `db.rs`.
  db.rs is now 3298 LOC; the per-cycle growth rate is +95
  LOC/cycle (r10 → r11 → r12: 3003 → 3108 → 3298).
- **Why minor**: same as r11; nothing new about the recovery layer's
  shape — only the size of the file holding it has grown enough
  to make the extraction marginally cheaper to defer. The single
  fresh data point this cycle is that R11-S2's host_id reader is
  logically part of the same recovery-layer concern.
- **Action**: When R11-A4's extraction lands, **include
  `enforce_host_id_file_mode` and the host_id file path
  derivation** as part of the moved code. The host_id machinery
  is the recovery-CAS's fence input; it shouldn't live in the
  general-purpose `db.rs` when the CAS doesn't either.

  If R11-A1's `secret_io.rs` extraction lands first, the
  `enforce_host_id_file_mode` body becomes a 3-line wrapper around
  `read_root_owned_secret_file(env_name=..., path, expected_mode=0o600,
  expected_len=None)` — at which point moving the wrapper to
  `recovery.rs` is mechanical.

## R12-I1 architectural assessment

**The fixer picked approach (b) — parallel-branch — per their commit
message at `b3bf741c`.** Verified:

- The wake-path builder `build_restore_nomad_job_json` at
  `restore_handler.rs:1276-1457` is now a sibling of
  `build_nomad_job_json_with` at `nomad_ch.rs:2311-2502`. Both take a
  `TaskDriverMode` arg, both contain `let (driver_name, config) =
  match mode { RawExec => …, ChPlugin => … }`, both emit the same
  ChPlugin TaskConfig shape modulo restore-specific fields.
- 181 LOC of feature code added at `restore_handler.rs:1276-1457`
  (mostly mirroring `nomad_ch.rs:2311-2502`), plus 157 LOC of tests
  at `restore_handler.rs:2451-2680` (mirroring the cold-boot tests at
  `nomad_ch.rs:4063-4327`).

**Did it deepen or shallow the duplication?**

Net: **deepened, on every metric**.

| Axis | Pre-R12-I1 | Post-R12-I1 | Δ |
|---|---|---|---|
| Number of jobspec-builder functions | 2 (`build_nomad_job_json` + `_with` are wrappers around the same impl, so this counts as 1 logical builder) | **2 logical builders** (`build_nomad_job_json_with` + `build_restore_nomad_job_json`) | +1 |
| Modules holding `TaskDriverMode` match arms | 1 (`nomad_ch.rs`) | **2** (`nomad_ch.rs` + `restore_handler.rs`) | +1 |
| Modules with an env-mutex for `SANDBOX_TASK_DRIVER` | 1 (`T7_ENV_LOCK`) | **2** (`T7_ENV_LOCK` + `R12_I1_ENV_LOCK`, **unsynchronised against each other**) | +1 + race |
| Modules holding `ZSBX_*` env-block construction | 1 | **2** | +1 |
| Modules with `nomad-vm-wrapper.sh`-line-pin tests | 1 | **2** | +1 (these test fixtures now duplicate across `nomad_ch.rs:3874-4016` and `restore_handler.rs:r12_i1_tests`'s assertions on `task["Env"]["ZSBX_RESTORE_FROM"]` etc.) |
| Lines that must change at T-8 cleanup | ~270 | **~470** | +200 |

**Should approach (a) merge be revisited?**

Approach (a) as the commit's author defined it — "fold the wake-path
into the cold-boot builder" — is **correctly rejected**. The wake-
path genuinely has a different Env contract (no PUBKEY_HEX, no
SANDBOX_ID, has RESTORE_FROM) and different Meta (`zeroship.kind:
"restore"`). The author's case for (b) is correct under those two
options.

But there's a third option — **approach (c)** — that neither the
fixer nor r11 considered explicitly: a shared lower-level helper
carrying a `JobspecRequest` value object with all the per-call
inputs (mode, vm_index, env_extra, pubkey_hex: Option<&str>,
restore_from: Option<&Path>, meta_kind: &'static str,
memory_mb: u32, cpus_boot: u32). Both call sites construct a
`JobspecRequest` and hand it to `build_nomad_job_json_for(req)`.

- The `match mode` block lives once.
- The Env block lives once (parameterised on `restore_from`,
  `pubkey_hex`, `meta_kind`).
- The Resources block lives once.
- The env mutex lives once (in `jobspec.rs`, the natural home).
- Test fixtures live once.

Estimated diff: +280 LOC for the shared helper, −330 LOC across the
two existing builders + their tests, net ~−50 LOC, removes the
cross-module env-lock race entirely.

**Recommendation**: r11-A2's "split before T-8" is now mandatory
(R12-A4). Pair it with **R12-A1's approach (c) collapse** in the
same PR — splitting `nomad_ch.rs` without unifying the two builders
would lock in the current dual-module duplication shape. The split's
natural home for the shared helper is `crates/sandbox/src/backend/
nomad_ch/jobspec.rs`, which already houses `TaskDriverMode` per the
R10-A4 sketch.

## Phase B cutover trajectory

**The cluster-side trajectory has stalled at T-8b-smoke**, not
because of code quality but because of an environmental + version-
pin failure pattern. The committed sequence:

1. **T-7** (`5fe36805`): SANDBOX_TASK_DRIVER feature flag in cold-
   boot jobspec builder. Lands the `TaskDriverMode` machinery in
   `nomad_ch.rs`.
2. **T-8a-controller** (`ae946cba`): worker-startup.sh installs the
   `ch_plugin` driver when `INSTALL_CH_PLUGIN_DRIVER=1` env is
   present.
3. **T-8b-smoke** (`b85c1edd`, 2026-05-23 night): 1-server +
   1-worker cluster. **FAIL at CREATE** — two independent issues:
   (a) Nomad refused to load the plugin without an explicit
   `plugin "nomad-driver-ch" { config {} }` stanza (worker-side
   provisioning gap), (b) deployed controller was v6 (stale by 13
   versions), missing both B24 (`ZSBX_SANDBOX_ID` env) and T-7
   (the flag) — controller emitted `raw_exec` jobs and the
   wrapper's kill-switch terminated every alloc in ~30ms. The Go
   driver was never reached.
4. **T-8b-prereqs-config** (`b4500576`): scripts-side fixes for
   blockers (a) and (b's worker-config half) — Nomad plugin stanza
   + a driver-health gate that fails worker provision if the
   `ch` driver isn't healthy within 30s.
5. **R12-I1** (`b3bf741c`): the architecture-impacting fix
   reviewed here — wake-path respects the same env flag. **This
   is in-source, NOT scripts.** It closes the 4th T-8b-smoke FAIL
   blocker enumerated in the cluster review (wake-path raw_exec
   hard-code).
6. **T-8b-build-and-upload** (`9f1dfc99`): rebuilt controller v18
   → v19 with all post-T-8b-smoke fixes (B24 + T-7 + R12-I1 +
   R11-S2 + R9-S4d + the R10/R11 fix set); driver v1 → v2
   (T-0..T-7 + T-8a + G4 tap rollback).
7. **T-8b-smoke-retry**: **not yet run** at HEAD `46e0fa2a`.

**Trajectory assessment**:

- **Diverging on script quality**: the Phase B scripts have absorbed
  a steady stream of corrections (B24, T-7, T-8a-controller,
  T-8b-prereqs-config, T-8b-build-and-upload). Each fix is
  precise and well-documented. **Script churn is converging on a
  clean post-cutover shape.**
- **Diverging on controller-vs-cluster coupling**: every cluster
  cycle so far has surfaced a controller-pin staleness issue
  (CONTROLLER_OBJECT defaults are bumped manually per cycle). The
  fixer pattern is to bump the default to the current version
  (`9f1dfc99` bumped v6 → v19). This pattern resolves itself
  asymptotically — at some point the default reaches a stable
  release and the bump cadence drops to zero — but **today the
  controller-pin and the script-pin live in the same file**
  (`provision-gcp-cluster.sh`) and must be advanced in lock-step
  every cluster cycle. A `CONTROLLER_OBJECT_LATEST` symlink in
  GCS (or a `.latest` marker file) would let the script default
  to "latest stable" without manual bumps. Suggested follow-up
  but out of architecture scope for this round.
- **Converging on the Rust controller shape**: the in-source fixes
  this cycle (R12-I1, R11-S2, R9-S4d) are all *additive correctness
  patches*, not *additive complexity*. Each is a one-line invariant
  check or a per-call-site flag plumbing. No new abstractions, no
  new traits, no new struct fields. **The controller-side
  architecture has not moved this cycle**, neither toward cruft
  nor toward cleanliness.
- **Accumulating cruft at the duplication seams**: the dominant
  Phase B accumulation is the **two-channel jobspec contract**
  (ZSBX_* env block under both modes, typed Config under
  ChPlugin only) and the **two-module match-on-mode shape**
  (R12-A1). T-8 deletion will clear half of this (the ZSBX_*
  env block + wrapper_path field + bash-wrapper script). But the
  match-on-mode shape is here to stay unless R12-A1's approach
  (c) lands first.

**Net**: the Phase B trajectory is **converging on a clean post-
cutover shape on the script + driver-binary lattice, but
accumulating cruft at the controller-side jobspec seam**. T-8b-
smoke-retry will validate the cluster lattice; the R10-A4 +
R12-A1 split-and-collapse will retire the controller-side cruft.
**Land R10-A4 + R12-A1 in the same window as T-8b-stress
(architecturally, BEFORE T-8b-cutover removes the wrapper)** —
otherwise post-T-8 leaves two oversized files holding a single-
mode dispatcher dead-code branch in each.

## Carry-forward (still open from earlier rounds)

- **[R4-A2 / R5-A2]** LeasedVmSlot RAII guard — **9th cycle**, no
  movement. R10-C1's `unregister_restored` interim committed at
  `be246395`. R12-I1's wake-path changes did NOT touch the
  vm_index lifecycle; the slot still leaks under the same drop-mid-
  await window. **Architecturally, this is now incident-class** —
  the failure surface widens every cycle without the RAII.
- **[R3-A1 / R5-A1 / R10-A3]** `Backend` enum 5-Err-returner split
  — count at HEAD = **5** (no change). Promoted CRITICAL in r10;
  remains CRITICAL.
- **[R3-A2 / R10-A2]** `RestoreBackend` trait facade — sits at
  **8 methods** (api-surface r12 confirms the trait was 8 since
  the B19 fix at `15b4f9a8`, pre-r10 — r10 and r11 both undercounted
  at 7). 3 of the 8 are one-line delegations through `Arc<NomadCH>`;
  the trait is still a facade. **`unregister_restored` is NOT on
  the trait** — it's a `pub(crate)` helper at
  `nomad_ch.rs:1706`, called only from `RealRestoreBackend::
  teardown_restore` at `restore_handler.rs:1199`. r11's "trait
  surface unchanged at 7" was a recount error; the trait was 8 in
  r10 too.
- **[R3-A3]** wrapper bash → Rust sidecar — subsumed by R11-A2 /
  R12-A4 (the wrapper is going away entirely at T-8).
- **[R3-A4]** `StopDisposition` enum — still `stop_inner(.., bool)`
  at `nomad_ch.rs:986-989`. Lands cheaply with R10-A4's split.
- **[R4-A1 / R10-A6 / R11-A3]** AppState builder accretion — 9th
  cycle, **unchanged at 7 `with_*` + `new_fixture`**
  (`grep -nE "^\s*pub fn with_\|^\s*pub fn new_fixture" lib.rs`
  returns 8 lines).
- **[R10-A4 / R11-A2 / R12-A4]** nomad_ch.rs at 5371 LOC, un-split.
  **Now a T-8 prerequisite**, not an ordering preference.
- **[R10-A1 / R11-A4 / R12-A5]** db.rs at 3298 LOC carrying the
  recovery CAS + the host_id reader. Un-extracted.
- **[R11-A1 / R11-Q2 / R12-A2]** root-owned-secret-file 5-site
  duplication. R9-S4d closed at `b4c3ef27` (added 4th site uid
  check). R11-S2 opened a 5th site at `85e4f2f9`. **Net duplication
  density grew this cycle.**
- **[T9 / T10 / R10-A7]** ControllerIdleSnapshotter duplicates
  admin_handlers' 70-LOC orchestration — unchanged at
  `sweep.rs:325-405` ↔ `admin_handlers.rs:1250-1325`.
- **[r9 C3]** AEAD fail-OPEN on GCS path — still at `lib.rs:654-666`,
  no boot-gate.

## Closed by recent commits

- **R9-S4d** (load_admin_token uid==0 check) — closed at `b4c3ef27`.
  Architecture impact: added the 4th site for R11-A1's loader
  duplication (which immediately became 5 with R11-S2).
- **R11-S2** (host_id reader mode+uid check) — closed at `85e4f2f9`.
  Architecture impact: added the 5th site for R11-A1's loader
  duplication; introduced a new mode constant (0o600).
- **R12-I1** (wake-path respects SANDBOX_TASK_DRIVER) — closed at
  `b3bf741c`. Architecture impact: deepened the cold-boot ↔
  wake-path duplication (R12-A1); approach (b)'s parallel-branch
  shape produced a second module holding `TaskDriverMode`
  machinery with an unsynchronised env-mutex.
- **R12-Q1** (Database::open_pool TODO refresh) — closed at
  `46e0fa2a`. Doc-only; cited R11-P1 + ntex factory bounds. No
  structural change.

None of these closed an architecture-level finding; the R9-S4d
+ R11-S2 pair *grew* the dominant duplication surface (R11-A1 /
R12-A2) by 2 sites + 1 axis.

---

## What's structurally new vs. r11

| Item | r11 state | r12 state | Δ |
|---|---|---|---|
| `RestoreBackend` trait methods | 7 (r11 recount error; was 8) | **8** | 0 actual; +1 reported |
| `RealRestoreBackend` Arc-NomadCH fields | 2 | **2** | 0 |
| `Backend` enum Err-returners | 5 | **5** | 0 |
| `NomadCHBackend` pub methods | 27 at HEAD (r11); +2 wt | **29 at HEAD** | +2 (now committed) |
| `db.rs` LOC | 3108 | **3298** | +190 (R11-S2 host_id reader + tests) |
| `with_*` builders | 7 | **7** | 0 |
| `pub fn new_fixture` | 2 | **2** | 0 |
| `restore_handler.rs` LOC | 2367 | **2680** | **+313** (R12-I1 wake-path TaskDriverMode + r12_i1_tests) |
| `nomad_ch.rs` LOC | 4923 HEAD; 5072 wt | **5371 HEAD** | +448 (T-7 committed + tests) |
| Root-owned-secret-file loader sites | 4 (R9-S4d pending) | **5** (R9-S4d closed; R11-S2 opened 5th) | **+1** |
| Modules with `TaskDriverMode` match arms | 1 | **2** | **+1** |
| Modules with `SANDBOX_TASK_DRIVER` env-mutex | 1 | **2 (unsynchronised)** | **+1 race** |
| Logical jobspec-builder functions | 1 | **2** | **+1** |
| Files > 5000 LOC | 0 | **1** (`nomad_ch.rs` at 5371) | +1 |
| Files > 2500 LOC | 1 (`db.rs` at 3108) | **3** (`db.rs` at 3298, `nomad_ch.rs` at 5371, `restore_handler.rs` at 2680) | +2 |

Every line of growth this cycle reinforces an existing flagship
finding: T-7 + R12-I1 deepens R10-A4 / R11-A2 by adding a second
module; R11-S2 deepens R11-A1 by adding a 5th caller; R9-S4d
closed one carry-forward by reinforcing another. **Zero structural
movement toward closing the 6 flagship carry-forwards** (R4-A2,
R10-A3, R10-A4, R10-A1, R11-A1, R4-A1). The cycle added 3 NEW
arch findings (R12-A1, R12-A2, R12-A3) and elevated 1 (R11-A2 →
R12-A4 / T-8 prerequisite).

## Recommended order of attack (updated; 6 PRs)

Updated from r11 with one elevation and one new PR:

1. **R11-A1 / R12-A2** `secret_io::read_root_owned_secret_file`
   extraction — now mandatory after the 5th site. ~80 LOC new
   module + `expected_mode: u32` parameter axis, ~150 LOC removed
   across 5 sites, fully mechanical. Closes R11-A1 + R11-Q2 +
   R12-A2; makes a future 6th caller a 3-line wrapper.
2. **R10-A1 / R11-A4 / R12-A5** db.rs split → `recovery.rs`
   (~280 LOC moved + `enforce_host_id_file_mode` follows). Closes
   the §6.1 / §9.2 recovery seam; drops db.rs to ~2900 LOC.
3. **R10-A7** snapshot orchestrator extraction (~150 LOC moved,
   closes T9 + T10).
4. **R10-A4 + R12-A1 + R12-A4 together** — nomad_ch.rs split AND
   the R12-A1 jobspec-builder collapse, **in the same PR**.
   Splitting without collapsing locks in the dual-module
   duplication shape; collapsing without splitting puts the
   shared helper in an oversized file. Estimated ~500 LOC of
   mechanical movement (the split) + ~280 LOC new shared helper
   (the collapse) − ~330 LOC removed across the two existing
   builders + their tests. **MUST land before T-8b-cutover** —
   if T-8 removes the wrapper, the post-T-8 cleanup is a single-
   file delete (`jobspec/rawexec.rs`) instead of an 8-zone
   surgical edit across 3 modules. Also closes R3-A4
   (StopDisposition lands in `stop.rs`), R12-A3 (TaskDriverMode
   becomes the field carried into the shared helper rather than
   the env arg), and resolves the cross-module env-lock race
   (R12-A1's lock unifies into `jobspec.rs`).
5. **R10-A3 + R10-A2 + R4-A2 (LeasedVmSlot) together** — the
   structural fix. `SnapshotCapableBackend` trait + collapse
   `RestoreBackend` into it + `LeasedVmSlot` RAII becomes a
   method-level concept on the trait. ~600 LOC. Closes 4
   carry-forwards.
6. **R4-A1 / R11-A3 / R10-A6** AppStateBuilder — closes 9-cycle
   carry-forward + `pub fn new_fixture` on both `AppState` and
   `SandboxConfig`.

Total: 6 PRs. Order is unchanged from r11 with one swap (R10-A4
+ R12-A1 + R12-A4 now combine, was a single-item PR). PR #4 is now
**load-bearing for T-8b-cutover**, not just T-8.

Closes 7 carry-forwards + 5 r12-new findings. Net file-count
change: +~10 modules, but each existing 2000+ LOC file drops
below 1500; `restore_handler.rs` drops to ~2200 LOC after
R12-A1's collapse moves ~480 LOC of duplicate-builder + tests out.
