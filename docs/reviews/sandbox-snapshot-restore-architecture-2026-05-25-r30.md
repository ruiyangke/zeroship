# Sandbox snapshot-restore architecture review — 2026-05-25 r30

**Reviewer**: architecture-r30 (post-T-7+T-8 cutover + post-R30-A1 semaphore + post-R31-P1)
**HEAD**: `729f22dd` (worktree `.worktrees/sandbox-snapshot-restore`, READ-ONLY).
**Predecessor**: r29 at `a3cfca10` — `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r29.md`.
**Scope**: `crates/sandbox/**` only.

---

## Summary

Six focus commits since r29 (ade8fb46, cdcd670d, b75728ce, 4d10ba45, 29a2dc95, 729f22dd) plus three mid-window class-fixes (b8310356, 62b083e1, 81b6e689). **Both r29 CRITICALs CLOSED.** The cutover (cdcd670d) deletes `TaskDriverMode`, `wrapper_path`, the env-validator, and 781 LOC of `nomad-vm-wrapper.sh` — the "two transports" axis is gone.

What remains:

- A **dual-mode hangover at the wire**: the `ZSBX_*` env block (12 keys) is still emitted in `build_nomad_job_json_with` and `build_restore_nomad_job_json`. Both rustdocs self-describe it as "the Go driver ignores Env; kept for debugging parity." Two writes; one is now decoration.
- `restore_handler.rs` is 5326 LOC (r29 baseline "3000+"). Production stays ~2.7k LOC; tests inline ~2.5k. Growth is in-domain (post-cutover jobspec tests + clock-resync hardening), not arch sprawl.
- `NomadStopPermits` placement on `AppState` is sized + held even for Docker/K8s builds — a Nomad-specific budget leaked into central state. Same antipattern shape as r29-A3.
- 4d10ba45's stale-rustdoc sweep was scope-bounded; ~16 "wrapper" references remain (mostly `restore_handler.rs`).

Net: 4 findings (0 CRITICAL, 2 IMPORTANT, 2 MINOR). Module-boundary coupling between `backend/nomad_ch.rs`, `restore_handler.rs`, `lib.rs` is **falling** post-cutover.

---

## CRITICAL

None. r29-A1 closed at `b8310356`; r29-A2 closed at `62b083e1`.

---

## IMPORTANT

### [r30-A1] ZSBX_* env block is a dual-mode hangover; "debugging parity" rationale doesn't survive the cutover

`nomad_ch.rs:2790-2817` and `restore_handler.rs:2549-2569` each emit a 9-12-key `ZSBX_*` env block. Post-cutover, the typed `Config` block (`nomad_ch.rs:2876-2901`) is the only field the Go driver reads. The env block exists for:

1. "Largely redundant with the typed Config block, but kept for debugging" (`nomad_ch.rs:2782-2784`)
2. "B24 / R8-DEPLOY1 regression pin: `ZSBX_SANDBOX_ID` must be present" (`:2812-2816`)
3. "Debugging parity with the cold-boot builder" (`restore_handler.rs:2549-2551`)

Reason 2 pins to the deleted wrapper's env validator (`nomad-vm-wrapper.sh:153`). The driver embeds `sandbox_id` directly via `Config.sandbox_id` (`:2882`) into the guest cmdline. The env copy is no longer load-bearing.

Reason 1/3 ("debugging parity") is the smell: the controller now writes the same VM-launch inputs in two formats — typed JSON for the driver, untyped env strings for observability — and they must stay in sync **manually**. A future change to `subnet_second_octet` semantics needs two edits in each builder; no compile-time pin enforces agreement. Same lesson as r29-A1/A2: invisible at lint/type-check, observable only at cluster runtime.

**Three fix options**:

1. **(Best) Delete the ZSBX_* env block entirely.** The driver ignores it. For debugging, log the fields at job-submit time via `tracing::info!(…)` — easier to grep than Nomad-task env. One source of truth. ~70 LOC delete across both builders + ~10 LOC structured-log additions.
2. **(Next) Keep only `ZSBX_SANDBOX_ID`.** The named regression pin survives; the rest goes. ~50 LOC delete + ~5 LOC test fixup.
3. **(Minimum) Exhaustive `struct ZsbxTaskEnv` with one emission site.** Both builders construct from the same inputs they pass to Config; emission is one function. ~80 LOC.

**Severity**: IMPORTANT — duplication is documented; immediate failure mode is "wrong log line", not "wrong guest". But the pattern is the kind r29 promoted from instance to class.

**Recommendation**: option 1, pre-emptive. The B24 regression pin is satisfied by the typed Config field (`:2882`); the env-side pin is decoration. Per `feedback_no_backward_compat.md` (pre-launch), delete + log structurally.

### [r30-A2] `nomad_stop_permits` on `AppState` is sized + held even for Docker/K8s

`lib.rs:258-259` declares `pub(crate) nomad_stop_permits: Arc<NomadStopPermits>` (non-`Option`). `from_config` at `:785-790` constructs it unconditionally from `config.nomad_ch.nomad_stop_concurrency`; install on the inner backend (`:791-798`) is conditional on `Backend::NomadCh`. Fixture path (`:550-552`) does the same: build + hold, never install. For Docker/K8s builds, the semaphore is allocated, exposed at `AppState.nomad_stop_permits()` (`:306-310`), sized from a Nomad config field, and never used.

The rustdoc at `:245-251` argues "process-global resource budget, not a backend-implementation detail." But the budget IS scoped: the downstream it bounds (`POST /shutdown` against the local Nomad agent) only exists in Nomad builds. Same `Configurable` antipattern shape r29-A3 named ("backend taxonomy leaks into a purportedly backend-orthogonal central surface"), now on `AppState`.

**Symptoms today**: low — ~tens of bytes per process. The shape risk is what it telegraphs: future per-backend caps will follow the same pattern, accreting per-backend dead fields on `AppState`.

**Three fix options**:

1. **(Best) Move the permits onto `Backend::NomadCh`'s inner `Arc<NomadCHBackend>`.** The `OnceLock` (`:215`) is already the load-bearing read site for `stop_inner`. Skip the `AppState`-level field. `metrics_export` reads via `state.backend.nomad_ch_handle().map(|b| b.nomad_stop_permits())`. ~30 LOC.
2. **(Next) Make the field `Option<Arc<NomadStopPermits>>`.** `None` for non-Nomad builds; metrics returns 0/0. ~20 LOC. Same antipattern shape, less acute.
3. **(Minimum) Document the dead-field on Docker/K8s in the field rustdoc.** Cheapest; doesn't fix.

**Severity**: IMPORTANT — same shape r29-A3 named. The fix surface is small today (1 field, 1 install, 2 metric reads). At 3-4 such caps it becomes invasive.

**Recommendation**: option 1, before the next per-backend cap. Pair with r29-A3 (per-backend sub-builders) for one combined PR; they share the recommendation shape.

---

## MINOR

### [r30-A3] Stale "wrapper" rustdoc in `restore_handler.rs` survives the 4d10ba45 sweep

`4d10ba45` cleaned 9 stale `nomad-vm-wrapper.sh` references in `nomad_ch.rs` + `config.rs` + `restore_handler.rs`. Residue at HEAD:

```
crates/sandbox/src/restore_handler.rs:21,1266,1470,1503,1973,2028,2507,2515,2903,3936
crates/sandbox/src/backend/mod.rs:487       wrapper's `[ ! -f $ZSBX_WORKSPACE_IMG ]` gate
crates/sandbox/src/config.rs:335,346,403,431  wrapper computes tap=... / IP arithmetic / etc.
```

The driver does these things now (the deleted wrapper does not). `restore_handler.rs:3826` ("error wrapper text") is a different idiom — fine.

**Severity**: MINOR — comments only; no behavioural divergence. The risk is a future reader at `:2507-2515` assuming the wrapper still env-validates and wasting an investigation cycle. ~5 minutes per future reader × ~10 readers ≈ 1 hour cost.

**Fix shape**: ~30 LOC of search-and-rewrite; each occurrence becomes "the ch driver's <op>" with a `nomad-driver-ch/ch/task_config.go` forward-link (the rewrite target 4d10ba45 adopted).

### [r30-A4] `restore_handler.rs` is 5326 LOC; the production-vs-test split argues for extracting tests, not for splitting modules

Production: ~2772 LOC. Inline tests: ~2554 LOC across three modules (`unit_tests` 549, `real_backend_tests` 1474, `r12_i1_tests` 527). Growth since r29 (`do_restore_inner` ~400 → ~830) is driven by cutover + C-7-LT/clock-resync hardening — in-domain, not sprawl.

Natural split (cosmetic, not load-bearing):

- `restore_handler::flow` — `restore_sandbox`, `do_restore_inner`
- `restore_handler::backend` — `RealRestoreBackend`, `RestoreBackend` trait, `StubRestoreBackend`
- `restore_handler::jobspec` — `build_restore_nomad_job_json` + blocking I/O helpers
- `restore_handler::clock_resync` — `clock_resync_post_restore_typed`, `verify_agent_version_post_restore`

**Severity**: MINOR — coherent largeness, not arch sprawl.

**Recommendation**: defer until a third reviewer (code-quality / api-surface) cross-flags. Below ~6k LOC the split's payoff is too small. ~150 LOC of pure moves when triggered.

---

## Verified closed since r29

| r29 finding | Status at r30 |
|---|---|
| r29-A1 (CRITICAL) STARTUP-HEREDOC-LEAK class | **CLOSED** at `b8310356` — 11-site audit + 9 hardened with `<<'EOF'`. |
| r29-A2 (CRITICAL) `spawn(...).detach()` inside `detach_isolated` body class | **CLOSED** at `62b083e1` — `spawn_delayed_release` deleted; typed `Task` + inline-await helpers; R29-C1 admin-snap-teardown call site fixed in same PR. |
| r29-A4 (IMPORTANT) typed staged-images contract | **OPEN; redirected.** Cutover simplified the gating; size-on-the-wire gap unchanged. Carry forward as r30-A4-CARRY (not re-stated). |
| r29-A3 (IMPORTANT) BackendBuilder backend-specific-setter taxonomy | **OPEN.** No new setter this round. r30-A2 is the same antipattern recurring on `AppState`. |
| r29-A5 (MINOR) exhaustive `SandboxStatus` match | **OPEN.** No change. |
| r29-A6 (MINOR) `Arc<SandboxConfig>` pre-launch shape | **OPEN.** No change. |
| TaskDriverMode + raw_exec wrapper path | **CLOSED** at `cdcd670d` — enum/field/validator/script/tests deleted. "Two transports" axis gone. |

---

## Net assessment

Post-cutover the architectural shape is cleaner. The single most load-bearing dual-mode axis (`TaskDriverMode`) is gone, and the two r29 CRITICALs closed via class-level fixes (not instance patches). r30's findings are all **shape-residue**:

- One wire residue (r30-A1 ZSBX_* — self-described as decoration),
- One central-state residue (r30-A2 — same antipattern as r29-A3),
- Two pure cleanup (r30-A3 stale rustdoc, r30-A4 module-split judgement).

Module-boundary coupling is **falling**: `nomad_ch.rs` net -270 LOC (cutover diff is mostly deletion); `restore_handler.rs` production grew for in-domain reasons; `lib.rs` +97 LOC concentrated in NomadStopPermits wiring.

**Biggest open risk**: r30-A1. The driver ignores Env; the controller writes 12 keys that must stay in sync with the typed Config; the sync is manual. r29-A1/A2 already taught "fix the instance, leave the trap" is a recurring loss pattern. Delete the env block.

**Three decisions for the next reviewer**:

1. **Land r30-A1 + r30-A2 in one structural-fix PR.** ~70 LOC combined.
2. **Carry r29-A4 forward as r30-A4-CARRY** without re-stating in r31.
3. **Defer r30-A4 module split** until a third reviewer cross-flags.
