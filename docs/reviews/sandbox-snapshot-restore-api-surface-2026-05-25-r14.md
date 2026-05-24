# Sandbox/snapshot-restore — api-surface r14 review

Date: 2026-05-25 (UTC)
HEAD at audit: `af4678ac` (branch tip; user-pinned `0053e8b6` predates the
ExecBody-demote commit that landed mid-cycle — both measured here).
Round 14 of N.
Prior: r13 at `e887b8ee` (`docs/reviews/sandbox-snapshot-restore-api-surface-2026-05-25-r13.md`).

Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`. Read-only.

## Summary

- **2 new findings** (0 critical, 0 important, 2 minor). Both are
  in-crate-only `pub` surfaces with zero external callers that have
  never been called out before:
  - **R14-API1** — `RealRestoreBackend::with_nomad_handle` +
    `with_shared_allocator` builder methods are `pub` but consumed
    only by `crate::AppState::from_config` (in-crate) + same-file
    tests. Cluster fix.
  - **R14-API2** — `RestoreHandlerError` docstring at
    `restore_handler.rs:58` documents a `Retry-After` header on the
    `503 vm_index_unavailable` response that the response builder
    at `admin_handlers.rs:1157-1163` does **not** actually emit.
    Wire-contract docstring drift.
- **3 closures r13→r14 on the api-surface lens**:
  - **R10-API3 (full)** — closed at `f50c95da`. 3 `persist.rs`
    `pub fn`s demoted to `pub(crate)`. `seal_filename_for` (the 1
    external-caller fn) correctly stays `pub`. 4-round carry resolved.
  - **R13-API1** — closed at `af4678ac` (sandbox crate `ExecBody`
    demoted; required inlining the body parse so the `pub(crate)`
    type stops surfacing in `pub fn` signatures). 1-round carry
    resolved.
  - **R10-API2 (ExecBody half)** — closed at `af4678ac` (sandbox-agent
    crate `ExecBody` demoted; one-token edit). The `not_found` half
    stays open intentionally per the commit message — `not_found` is
    consumed by `crates/sandbox-agent/src/main.rs:229` across the
    bin/lib split; the docstring at `handlers.rs:255-263` already
    explains. **R10-API2 closure rationale = "ExecBody fixed; not_found
    is bin/lib-justified."**
- **Net new `pub` r13→r14** (`e887b8ee..af4678ac` for sandbox + sandbox-agent
  src): zero. The 2 demotions are visibility-neutral (`pub` and
  `pub(crate)` both match the canonical regex `pub[[:space:](]`).
- **Net new `pub(crate)` r13→r14**: **+3** — all from `c5b9cb9d`'s
  R13-Q1 unification (`#[cfg(test)] pub(crate) mod test_env_lock` +
  `pub(crate) static TASK_DRIVER_ENV_LOCK` + `pub(crate) fn
  with_task_driver_env`). All three are inside a `#[cfg(test)]` module
  — they compile out of production builds entirely. **No production
  surface added by R13-Q1.**
- **`RestoreBackend` trait method count**: still **8**. No drift.
- **`Result<_, String>`** sandbox **161**, sandbox-agent **14** at
  `af4678ac` — **flat r13→r14** under the canonical find-based regex.
- **`pub`-token count**: r13: 841 → r14: **844** (+3 from R13-Q1's
  cfg-test module). Top-level `pub`: r13: 329 → r14: **330** (+1 from
  the cfg-test module declaration).
- **C-3 fix (`c890c015`)**: api-surface clean. Zero new pub items.
  `std::thread::Builder::spawn` swap is a body-of-`Tiered::put` change;
  no signature touched.
- **R13-Q1 audit (`c5b9cb9d`)**: api-surface clean **for production**.
  All new `pub(crate)` items are gated on `#[cfg(test)]`; the only
  out-of-module test consumer (`restore_handler::r12_i1_tests`) is
  itself `#[cfg(test)]`. **Test-only `pub(crate)` is the correct
  visibility shape for a cross-module test helper.**
- **ErrorEnvelope drift**: unchanged from r10. Agent's
  `error_envelope.rs:33-42` still carries the "do not lift into
  zeroship-core yet" rationale. No convergence either way.

## R13-Q1 audit — `test_env_lock` visibility

`c5b9cb9d` promoted `T7_ENV_LOCK` (nomad_ch.rs::tests-local) out of
`#[cfg(test)] mod tests` into a sibling `#[cfg(test)] pub(crate) mod
test_env_lock` at file scope, then deleted the duplicate
`R12_I1_ENV_LOCK` in `restore_handler.rs::r12_i1_tests`.

| Symbol | Path | Visibility | Cfg gate | Justified? |
|---|---|---|---|---|
| `mod test_env_lock` | `crates/sandbox/src/backend/nomad_ch.rs:3440` | `pub(crate)` | `#[cfg(test)]` | YES — has to be `pub(crate)` so `restore_handler::r12_i1_tests` can `use` it via `crate::backend::nomad_ch::test_env_lock::with_task_driver_env` at `restore_handler.rs:2604`. Cannot be smaller. |
| `static TASK_DRIVER_ENV_LOCK` | `crates/sandbox/src/backend/nomad_ch.rs:3445` | `pub(crate)` | inside `#[cfg(test)] mod test_env_lock` | YES — referenced by `with_task_driver_env` (same module) but the docstring (and the design rationale at `architecture-r12 R12-A1`) reserves it as the canonical lock for any future cross-module env-touching tests. `pub(crate)` lets a future test mod take the lock directly without routing through the helper. |
| `fn with_task_driver_env` | `crates/sandbox/src/backend/nomad_ch.rs:3455` | `pub(crate)` | inside `#[cfg(test)] mod test_env_lock` | YES — the cross-module helper. Used by `restore_handler.rs:2604` import. |

Verification that no production code accidentally sees these items:

- `git show c5b9cb9d -- crates/sandbox/src/backend/nomad_ch.rs | grep
  "^+pub"` → 3 hits, all 3 inside the `#[cfg(test)]` mod body. None
  leak.
- Cross-crate / cross-binary survey: `grep -rn 'test_env_lock\|
  TASK_DRIVER_ENV_LOCK\|with_task_driver_env' crates/` → 9 hits across
  2 files, all in `crates/sandbox/src/{backend/nomad_ch.rs,
  restore_handler.rs}`, all in test-only code (the imports are inside
  `#[cfg(test)] mod {tests,r12_i1_tests}` blocks at lines 3475 and
  2595 respectively). Zero production callers.
- Outside the sandbox crate: zero references to `test_env_lock` /
  `TASK_DRIVER_ENV_LOCK` / `with_task_driver_env`. **Confirmed: the
  unification is test-binary-internal.**

**Verdict**: R13-Q1 is api-surface-clean. The `pub(crate)` widening is
correct — it's the minimum visibility that lets `r12_i1_tests` reach
the helper across modules. Approach (A) (in-place promotion +
co-location with `task_driver_mode_from_env`) is the right shape
relative to (B) (fresh `src/tests/env_lock.rs`) for the reason the
commit message gives: the lock is conceptually owned by the env-var
reader, not by either test mod.

## C-3 fix audit — `c890c015`

`Tiered::put`'s L2 upload switched from `compio::runtime::spawn_blocking
(...).detach()` to `std::thread::Builder::spawn`. Verification:

- `git show c890c015 -- crates/sandbox/src/snapshot_store_gcs.rs |
  grep "^+pub"` → **empty**.
- `git show c890c015 -- crates/sandbox/src/snapshot_store_gcs.rs |
  grep "^+[[:space:]]*pub"` → **empty**.
- The added `c3_put_callable_from_non_compio_thread` test is module-
  private (`#[test]` inside `#[cfg(test)] mod tests`); no `pub`
  attached.

**Verdict**: C-3 is api-surface-clean. Zero new surface; the fix is
purely a body-of-`put`-impl swap.

## R10-API3 closure audit — `f50c95da`

Confirmed at HEAD:

| Symbol | `persist.rs` line | Visibility | Out-of-crate callers |
|---|---|---|---|
| `seal_filename_for` | 272 | **`pub`** (justified) | 2 in `sandbox_preview_share_e2e.rs` (external integration test) + 1 in `backend/nomad_ch.rs` + 1 docstring ref in `admin_handlers.rs` |
| `seal_filename_for_str` | 281 | **`pub(crate)`** (demoted) | 0 |
| `seal` | 384 | **`pub(crate)`** (demoted) | 0 |
| `unseal_dir` | 521 | **`pub(crate)`** (demoted) | 0 |

`seal_filename_for` stays `pub` because the external e2e test
constructs sealed-snapshot artifacts and consults the canonical
filename helper rather than re-deriving it. The other 3 are correctly
demoted. **R10-API3 fully closed at `f50c95da` — drop from
carry-forward.**

## R13-API1 + R10-API2 closure audit — `af4678ac`

`crates/sandbox-agent/src/handlers.rs:568` — `pub struct ExecBody` →
`pub(crate) struct ExecBody`. One-token edit; the handler already parsed
via `serde_json::from_slice(&body)` so no signature widening cascade.

`crates/sandbox/src/handlers.rs:797` — `pub struct ExecBody` →
`pub(crate) struct ExecBody`, **plus** the `exec` handler signature
switched from `body: web::types::Json<ExecBody>` to `body: Bytes`,
with an inline `serde_json::from_slice` + 400 envelope on parse
failure. The signature change matters: as long as `exec` was
`pub fn exec(..., body: Json<ExecBody>) -> HttpResponse`, demoting
`ExecBody` to `pub(crate)` would have produced a `private type in
public interface` error. Inlining the parse closes the cascade. **The
shape sandbox-agent's `exec_cmd` already used (`body: Bytes` + manual
parse) is now used in both crates.**

`not_found` in sandbox-agent stays `pub` — consumed by
`crates/sandbox-agent/src/main.rs:229` across the bin/lib split. The
docstring at `handlers.rs:255-263` (added in an earlier round)
documents this. R10-API2's `not_found` half is closed-by-rationale,
not by code change.

**Verdict**: R13-API1 fully closed at `af4678ac`; R10-API2 fully closed
at `af4678ac` (ExecBody) + carry-rationale (not_found).

## Findings (NEW since r13)

### [R14-API1] `RealRestoreBackend::with_nomad_handle` + `with_shared_allocator` builder methods are `pub` with zero out-of-crate callers (MINOR, api-surface-r14)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:1022` (`with_shared_allocator`)
  - `crates/sandbox/src/restore_handler.rs:1039` (`with_nomad_handle`)
- **Symptom**: both are `pub fn`s on `RealRestoreBackend`. The
  enclosing struct (`pub struct RealRestoreBackend` at
  `restore_handler.rs:996`) must stay `pub` because
  `crates/sandbox/tests/sandbox_admin_e2e.rs:755` constructs it via
  `zeroship_sandbox::restore_handler::RealRestoreBackend::new(...)`.
  But the e2e tests do **not** call either builder method — only
  `RealRestoreBackend::new` plus the public `AppState::with_*`
  fluent API for wiring.
- **Evidence of orphanage at clean HEAD `af4678ac` (with the
  uncommitted C-4 fastpath stash excluded)**:
  - `grep -rn 'with_nomad_handle' --include="*.rs" crates/ tests/`:
    - definition at `restore_handler.rs:1039`
    - in-crate caller at `lib.rs:730` (`AppState::from_config`)
    - same-file tests at `restore_handler.rs:2287, 2371`
    - 2 docstring refs in `backend/mod.rs`
    - **zero out-of-crate callers, zero out-of-file production
      callers.**
  - `grep -rn 'with_shared_allocator' --include="*.rs" crates/ tests/`:
    - definition at `restore_handler.rs:1022`
    - in-crate caller at `lib.rs:706` (`AppState::from_config`)
    - same-file tests at `restore_handler.rs:1926, 1965, 2286, 2370`
    - 2 docstring refs in `backend/mod.rs`
    - 1 docstring ref in same file at line 1917
    - **zero out-of-crate callers, zero out-of-file production
      callers.**
- **Why a NEW finding**: the sibling `Backend::nomad_ch_handle()` at
  `backend/mod.rs:482` IS already `pub(crate)` with a docstring at
  lines 475-481 that explicitly explains: *"`pub(crate)` because
  handing out a concrete `Arc<NomadCHBackend>` bypasses the 'enum
  dispatch is the only contract' promise — out-of-crate callers could
  reach past the `Backend` enum and call backend-specific methods
  directly, stranding the trait surface. The only legitimate caller is
  `crate::restore_handler::RealRestoreBackend::with_nomad_handle`
  inside `crate::AppState::from_config`."*
  The same argument applies to the **consumer side** of that chain:
  `with_nomad_handle` accepts the `pub(crate)` handle and stores it
  on a `pub`-exposed type. The builder method itself doesn't need to
  be `pub` because the only legitimate construction path is the
  in-crate `AppState::from_config` wiring. An out-of-crate consumer
  cannot usefully call `with_nomad_handle` because they can't obtain
  an `Arc<NomadCHBackend>` legitimately (the `pub(crate)` handle gate
  blocks that). Likewise `with_shared_allocator` accepts a
  `crate::backend::nomad_ch::VmIndexAllocator` which is itself in-crate.
- **Action**: demote both to `pub(crate)`. Two-token edit. The struct
  stays `pub` (external test construction continues to work via
  `RealRestoreBackend::new`); only the wiring builders shrink.
- **Severity**: MINOR. Same severity as R10-API3 (3-token persist.rs
  demotion). No wire-shape impact, no behavioural change, purely a
  visibility tightening that matches the already-documented intent of
  the `nomad_ch_handle` sibling.
- **Cluster**: pair the two `RealRestoreBackend::with_*` demotions as
  one micro-PR. Consider folding R10-API1 + R11-API1 in too for a
  "visibility tightening" cluster (5 demotions total across the api-
  surface backlog).

### [R14-API2] `RestoreHandlerError` doc-comment promises a `Retry-After` header on `503 vm_index_unavailable` that the response builder does not emit (MINOR, api-surface-r14)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:58` (the docstring)
  - `crates/sandbox/src/admin_handlers.rs:1157-1163` (the response
    builder)
- **Symptom**: the `RestoreHandlerError` enum's class-level docstring
  at `restore_handler.rs:54-61` says:
  ```text
  /// Why the restore couldn't proceed. Maps to § 10.0 wire envelope:
  ///
  /// - `StateMismatch`        → 409 `state_mismatch`
  /// - `FeatureDisabled`      → 501 `feature_disabled`
  /// - `VmIndexUnavailable`   → 503 `vm_index_unavailable` (Retry-After)
  /// - `SnapshotCorrupt`      → 500 `snapshot_corrupt` (CAS to suspect)
  /// - `Backend` / `Database` → 500 (controller-internal)
  /// - `NotFound`             → 404
  ```
  The `(Retry-After)` parenthetical promises an HTTP `Retry-After`
  header — a standard backoff hint for 503 responses (RFC 9110 § 10.2.3).
  But the actual response builder at `admin_handlers.rs:1157-1163`
  emits only the §10.0 envelope JSON; no header is set:
  ```rust
  RestoreHandlerError::VmIndexUnavailable { requested } => ErrorEnvelope::new(
      StatusCode::SERVICE_UNAVAILABLE,
      "vm_index_unavailable",
      "no vm_index available to host the restored sandbox",
  )
  .with_extra(serde_json::json!({"requested": requested}))
  .into_response(),
  ```
  Whole-file `grep -rn 'Retry-After' crates/sandbox/src` returns
  **only the docstring** at `restore_handler.rs:58`. No emission site
  anywhere in the sandbox crate.
- **Why a NEW finding (timely)**: cluster-r5 (`T8b-smoke-r5`,
  `5399b6b7`) just surfaced this exact 503 in the field — a wake
  request arrives in the millisecond range after a snapshot response,
  races the detached source-teardown, and the v1 sticky allocator
  rejects with `vm_index_unavailable`. C-4 fix proposals
  (caller-side bounded retry) are being discussed in cluster-r5; the
  external-client-side counterpart is **exactly** the kind of backoff
  the `Retry-After` header is for. A client reading
  `RestoreHandlerError`'s docstring will believe they can drive a
  retry loop off the header value — but the header is not emitted,
  so the client will fall back to a hardcoded constant or busy-poll.
- **Evidence the §10.0 wire shape is otherwise tested**:
  `crates/sandbox/src/admin_handlers.rs:1610`
  (`a4_map_restore_error_vm_index_unavailable_envelope`) pins the
  status code, the `error` field, the `message` field, and the
  `requested` extra. **No assertion on response headers.** So the
  Retry-After absence has never had a regression guard either.
- **Action**: choose one of:
  1. **Add the header** to the response builder:
     `.with_header("Retry-After", "5")` (or compute from the
     `host_fence_timeout_secs` config + nomad-purge envelope). Extend
     the test at `admin_handlers.rs:1610` to assert the header. This
     matches the docstring and gives clients a usable backoff signal.
  2. **Strip the `(Retry-After)` parenthetical** from
     `restore_handler.rs:58`. This is the cheapest fix but loses the
     forward-looking documentation; the cluster-r5 C-4 sprint likely
     wants the header eventually anyway.
  3. **Mark `(Retry-After: planned, not v1)`** to defer until C-4
     lands. Honest about current state without removing the design
     intent.
- **Severity**: MINOR. Wire shape is otherwise §10.0-correct; the
  drift is in the documentation of a back-compat-friendly *optional*
  header that clients should not have been hard-depending on. Becomes
  IMPORTANT if the C-4 retry-loop design crystallises around a
  cluster-side response field that conflicts with this docstring's
  reservation. **Recommend option 1 (add the header + assert it) —
  paired with the C-4 sprint, this is a one-line wire-shape fix that
  enables clients to participate in the backoff.**

## R14-API3 (FOLDED) — `metrics::takeover_unreachable_value` sibling of R11-API1

`crates/sandbox/src/metrics.rs:217` — `#[doc(hidden)] pub fn
takeover_unreachable_value() -> u64` — same shape as the 2
R11-API1 functions at lines 223 and 229. Zero callers at HEAD:
`grep -rn 'takeover_unreachable_value' --include="*.rs"
/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`
returns the definition site only.

**Why folded, not filed separately**: r10/r11/r12/r13 surveyed the
metrics test accessors and named `takeover_corrupt_value` +
`sandbox_corrupt_id_value` as the orphan pair. The same survey at r14
catches a **third** sibling sharing the bug — but the right action is
to **expand the R11-API1 cluster from 2 fns to 3 fns**, not file a new
finding. Same severity, same fix shape (`#[cfg(test)]` + module-
private, or delete).

**Updated R11-API1 fix**: 3 fns to delete or `#[cfg(test)]`-gate at
`metrics.rs:217, 223, 229`.

## Carry-forward (still open at HEAD `af4678ac`)

Reference lines re-verified against HEAD.

- **R10-API1** — `_test_build_auth_from_sealed` orphan `pub` at
  `restore.rs:613` (**5th-round carry**). `grep -rn
  '_test_build_auth_from_sealed' --include="*.rs"
  /home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`
  returns the definition site only. Cluster with R11-API1 (expanded).
- **R10-API2 (closed)** — ExecBody half closed at `af4678ac`;
  `not_found` half closed by bin/lib-split docstring. **Drop from
  carry-forward.**
- **R10-API3 (closed)** — 3-fn persist.rs demotion landed at
  `f50c95da`. **Drop from carry-forward.**
- **R10-API4** — readyz non-§10.0 wire shape in sandbox-AGENT
  (**5th-round carry**). Verbatim at `crates/sandbox-agent/src/
  handlers.rs:498-510`. Two non-§10.0 503 paths:
  `{"status":"draining"}` at line 501, `{"status":"reaper-down"}` at
  line 508. Clustered with R12-API1.
- **R10-API5 (closed)** — sig.rs:120 hyphenated UUID example. Closed
  at `8ed9aa90`. Confirmed: `grep -n 'Sandbox UUID'
  crates/sandbox-agent/src/sig.rs` → `120:    /// Sandbox UUID in
  \`Uuid::simple()\` form — 32 lowercase hex`. **Drop from
  carry-forward.**
- **R10-API6 (closed)** — db.rs hyphenated test comment. Closed at
  `8ed9aa90`. Confirmed at `db.rs:2917` — comment now clarifies
  host_id stays hyphenated while sandbox_id is `.simple()`. **Drop
  from carry-forward.**
- **R11-API1 (EXPANDED)** — now **3** `#[doc(hidden)] pub fn` orphans
  in `metrics.rs` (`takeover_unreachable_value` at line 217,
  `takeover_corrupt_value` at line 223, `sandbox_corrupt_id_value` at
  line 229). All 3 zero-caller at HEAD. **4th-round carry, expanded.**
- **R12-API1** — sandbox crate's `readyz` non-§10.0 wire shape at
  `handlers.rs:132-139`. **3rd-round carry.** Verbatim. Two
  non-§10.0 paths: `{"status":"ready"}` and
  `{"status":"backend-unhealthy"}`.
- **R12-API2** — R10-API6 line reference doc-bookkeeping nudge.
  Closed alongside R10-API6 at `8ed9aa90`. **Drop from carry-forward.**
- **R13-API1 (closed)** — sandbox crate ExecBody demote landed at
  `af4678ac`. **Drop from carry-forward.**
- **R14-API1 (NEW)** — `RealRestoreBackend::with_nomad_handle` +
  `with_shared_allocator` builder over-pub.
- **R14-API2 (NEW)** — Retry-After docstring drift on 503
  `vm_index_unavailable`.

## Closed by recent commits (api-surface scope)

| Finding | Commit | Lens-relevant change |
|---|---|---|
| R10-API3 (3-fn persist demotion) | `f50c95da` | `pub fn seal`, `pub fn unseal_dir`, `pub fn seal_filename_for_str` → `pub(crate)` |
| R10-API5 (sig.rs:120 hyphen example) | `8ed9aa90` | docstring updated to `Uuid::simple()` form |
| R10-API6 (db.rs hyphen comment) | `8ed9aa90` | comment clarified — host_id stays hyphenated; sandbox_id is `.simple()` |
| R12-API2 (R10-API6 line nudge) | `8ed9aa90` | subsumed by R10-API6 closure |
| R13-API1 (sandbox ExecBody) | `af4678ac` | `pub struct` → `pub(crate)`; signature inlined the parse |
| R10-API2 (sandbox-agent ExecBody half) | `af4678ac` | `pub struct` → `pub(crate)` |
| R10-API2 (sandbox-agent not_found half) | — | closed-by-rationale (bin/lib-split docstring at `handlers.rs:255-263`) |

**6 carries closed this round** — the largest single-round closure in
the api-surface lens to date. R10-API2 + R10-API3 + R10-API5 + R10-API6
were all 4–5 round carries; the documentation-doc-fixes + ExecBody
demote each took multiple rounds of evidence-stable proof before
landing.

## Recommended fix order

1. **R14-API2 (Retry-After docstring drift)** — paired with the C-4
   cluster sprint, this is a one-line wire-shape fix that gives
   clients a backoff signal. The docstring is already documented as
   §10.0-compliant; making it true closes a wire-contract drift.
   Decide between options 1/2/3 in the finding; option 1 is best.
2. **R14-API1 + R10-API1 + R11-API1 (visibility tightening cluster)** —
   5 demotions total (2 RealRestoreBackend builders + 1 restore.rs
   test helper + 3 metrics.rs test accessors). One micro-PR.
   Evidence-stable across 1–5 review rounds; mechanical edit.
3. **R10-API4 + R12-API1 (readyz §10.0 envelope drift, 2 sites)** —
   carve-out + comments OR convert both to envelope. Decide carve-out
   policy first; the implementation is mechanical.

The api-surface backlog is now **4 open findings (R10-API1, R10-API4,
R11-API1 (expanded), R12-API1) + 2 new (R14-API1, R14-API2)** = 6
items. Down from 8 at r13.

## Two most-critical citations

1. **`crates/sandbox/src/restore_handler.rs:58`** — *"`VmIndexUnavailable`
    → 503 `vm_index_unavailable` (Retry-After)"*. Docstring promises a
   header that does not exist in the response. The 503 just surfaced
   in cluster-r5 (C-4), which means clients will start hitting this
   error in the field — and they will read the docstring before the
   handler source. This is the **only** finding in the api-surface
   backlog that touches an in-the-field wire-shape contract;
   everything else is visibility tightening or doc-fix tier.
2. **`crates/sandbox/src/metrics.rs:217, 223, 229`** — 3
   `#[doc(hidden)] pub fn` test accessors with zero callers across
   the entire workspace. Evidence-stable across **3 review rounds**
   (now 4 with the takeover_unreachable_value expansion). Cheapest
   visibility-tightening edit in the backlog: 3 fns either gated
   `#[cfg(test)]` or deleted outright. The `takeover_unreachable_value`
   sibling unflagged for 3 rounds is itself the signal — these tend
   to come in clusters that aren't surveyed exhaustively in one pass.

## Trend

- **`Result<_, String>`** (canonical regex `Result<[^,>]+,\s*String\s*>`,
  measured via `find … -name '*.rs' -print0 | xargs -0 grep -cE … |
  awk '{sum+=$NF}'`):
  - sandbox **161** at `e887b8ee`, **161** at `af4678ac` — **flat
    r13→r14** (3 consecutive rounds at 161).
  - sandbox-agent **14** at both — flat, at floor (5 consecutive
    rounds at 14).
- **`pub`-token count** (`^[[:space:]]*pub[[:space:](]`):
  - r13: 841
  - **r14: 844** — +3, all attributable to R13-Q1's `#[cfg(test)]
    pub(crate) mod test_env_lock` + 2 inner items. **Zero production
    surface added r13→r14.**
  - Top-level `pub` only: **330** at r14 (+1 from the cfg-test
    module declaration line).
- **`RestoreBackend` trait method count**:
  - r13: 8
  - **r14: 8** — flat. No round has changed this since trait was
    introduced.
- **Net new wire endpoints** (r13→r14): **0**. All r13→r14 commits
  are docs (r12/r13 reviewer artifacts, deferred refresh,
  cluster-r5 review), cluster smoke scripts/results, driver/controller
  pin bumps, persist demotion (R10-API3), R12-P1 BufReader (perf,
  no surface), R13-Q1 env-mutex unification (test-only), C-3
  L2-upload detach swap (no surface), ExecBody demotion (R13-API1 +
  R10-API2). **No new HTTP routes, no new request shapes, no new
  error envelope fields.**
- **Closure velocity** (api-surface findings closed per round):
  - r10→r11: 0
  - r11→r12: 0
  - r12→r13: 1 (sig.rs partial)
  - **r13→r14: 6** — the largest single-round closure since the
    backlog opened. The pattern matches: cluster smoke r5
    (`T-8b-smoke-r5`) triggered a pause on stress, which freed
    review-loop cycles for cheap mechanical closures. Expect r14→r15
    to revert toward the baseline (0–1 closures per round) once
    cluster activity resumes.
