# Sandbox snapshot-restore architecture review — 2026-05-25 r31

**Reviewer**: architecture-r31 (post-cycle-50 paperwork, post-v25 perf validation, post-cadence quick-win merge)
**HEAD**: `7c44cc78` (worktree `.worktrees/sandbox-snapshot-restore`, READ-ONLY).
**Predecessor**: r30 at `729f22dd` — `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r30.md`.
**Scope**: `crates/sandbox/**` only.

---

## Summary

Four commits since r30:

- `a6e517b2` — cadence quick-win merged from feat/nomad-driver-ch (5 sleep-cadence tightenings: 250→100ms alloc-poll, 150→50ms livez-poll, both async and blocking variants).
- `594f6d89`, `f82de17a`, `7c44cc78` — reviewer paperwork + v25 perf validation report. No code under `crates/sandbox/src/**` touched.

Net source delta vs r30: ±10 LOC across `nomad_ch.rs` + `restore_handler.rs` (sleep durations + doc comments). File sizes unchanged at significant figures: `restore_handler.rs` 5326 LOC, `nomad_ch.rs` 8283 LOC, `lib.rs` 3184 LOC. Both r30 IMPORTANT carries remain open and unmodified.

What v25 perf validation surfaced: wake p50 50.7s → 49.1s (−1.6s, vs hypothesized −5–15s). The 17fc24b8 latency investigation already concluded ~85% of wake is inside CH's `--restore` path; v25 confirms this empirically. **The controller has no phase-level instrumentation of the restore boundary today** — see r31-A3 below.

Net: 4 findings (0 CRITICAL, 2 IMPORTANT carries, 2 MINOR — one new, one re-stated). Arch sprawl from the cherry-pick: none. Module-boundary coupling unchanged.

---

## CRITICAL

None.

---

## IMPORTANT

### [r31-A1 CARRY of r30-A1] ZSBX_* env block still emitted in both jobspec builders; no deletion work landed

Status at `7c44cc78`: identical to r30. Both emission sites unchanged:

- `crates/sandbox/src/backend/nomad_ch.rs:2790-2817` — 12 keys on cold-boot, +`ZSBX_RESTORE_FROM` on the restore branch (`:2819-2821`).
- `crates/sandbox/src/restore_handler.rs:2549-2561` — 9 keys on restore.

Rustdocs at `nomad_ch.rs:2782-2784` and `restore_handler.rs:2540-2542` still self-describe as "Largely redundant with the typed Config block below … but kept for debugging." The typed `Config` block at `nomad_ch.rs:2876-2901` (`sandbox_id`, `subnet_base_octet`, `vm_index`, `workspace_img`, `user_home_img`, `memory_mb`, `cpus`, `pubkey_hex`, `user_id`, `stage_disk_images`, `restore_from`) is the actual driver input; the env block is decoration the driver discards.

No new evidence against r30-A1's recommendation. The risk is unchanged: any future change to `subnet_second_octet` semantics or vm-index encoding requires two coordinated edits per builder, with no compile-time pin. Pre-launch, per `feedback_no_backward_compat.md`: delete the env block and log structurally via `tracing::info!`. Fix shape: ~70 LOC delete + ~10 LOC `tracing::info!` calls.

**Severity**: IMPORTANT. Re-stating because no progress; if the next cycle's backlog drain doesn't land it, downgrade to MINOR at r32 (the shape is stable, the doc-comment self-warns, immediate harm is "wrong log line" only).

### [r31-A2 CARRY of r30-A2] `nomad_stop_permits` on `AppState` still allocated unconditionally for Docker/K8s

Status at `7c44cc78`: identical. `crates/sandbox/src/lib.rs:258-259` keeps the non-`Option` field; `from_config:785-790` allocates a sized `NomadStopPermits` unconditionally; `:791-798` installs on the inner backend only when `nomad_ch_handle()` returns `Some`. Fixture path at `:550-552` does the same: build, hold, never install.

For Docker/K8s deployments — which won't have `nomad_ch_handle()` — the permits are sized from `config.nomad_ch.nomad_stop_concurrency` (Nomad-scoped config field), exposed via the `AppState::nomad_stop_permits()` accessor (`:306-310`), and never bound to a stop path. The rustdoc at `:245-251` defends "process-global resource budget", but the budget bounds calls to Nomad's `POST /shutdown` and nothing else.

Same antipattern shape as r29-A3 (backend taxonomy leaking into central state). Cost today is bytes; risk is the precedent — the next per-backend cap will follow this pattern. r30 recommendation (move to `Backend::NomadCh`'s inner Arc, drop the `AppState` field) stands; ~30 LOC.

**Severity**: IMPORTANT, unchanged.

---

## MINOR

### [r31-A3 NEW] No controller-side ingestion of driver phase counters; v25 validation surfaced restore-internal attribution gap

The v25 perf review (`docs/reviews/sandbox-snapshot-restore-perf-validation-v25-2026-05-25-r1.md:80-86`) concludes the wake bottleneck is "inside CH restore, not disk I/O" — 42s of the 49s wake p50 is in CH-internal restore state reconstruction. The driver exports `nomad_driver_ch_start_task_*`, `nomad_driver_ch_destroy_task_*`, and the absent-from-prom `nomad_driver_ch_prewarm_memory_ranges_bytes_total`, but **the controller does not ingest or correlate any driver-side phase counters**: no matches for `nomad_driver_ch_*` or `restore_phase` / `wake_phase` in `crates/sandbox/src/**`.

Today the controller times the full wake call (`do_restore_inner` start to `/livez=200`), bucketed in `metrics.rs`. It cannot attribute "42s inside CH" vs "7s in Nomad scheduling + alloc-running poll + livez probe" without parsing driver Prom or reading driver task-state-store entries via Nomad API. The v25 report had to derive the split out-of-band.

**Architectural follow-up shape**: either (a) add a Nomad-driver task-state hook that the controller polls between `wait_for_alloc_running` and `wait_for_livez` to read the driver's per-phase timings (driver-side change; controller side is one extra GET); or (b) accept the gap and rely on `nomad-driver-ch` Prom scraping for restore-internal latency, leaving the controller's wake-bucket as wall-clock only. Option (b) is cheaper and matches the "gateway is dumb" sibling invariant (controller doesn't peek inside driver internals).

**Severity**: MINOR — observability, not correctness. The architectural decision (introduce a driver-controller phase-handoff RPC, or don't) is the substance; today's lack-of-it isn't a bug.

**Recommendation**: defer until a second perf cycle measures cadence-quick-win impact. If after a6e517b2's −300ms hypothesis lands, the CH-internal share grows above ~88%, formalize option (b) — pin the driver Prom names + scrape them — rather than (a). One ADR + ~50 LOC of scrape glue.

### [r31-A4 CARRY of r30-A3] Stale "wrapper" rustdoc residue in `restore_handler.rs`

Unchanged from r30: 10 stale references at `restore_handler.rs:21, 1266, 1470, 1503, 1973, 2028, 2507, 2515, 2903, 3936`, plus `backend/mod.rs:487` and `config.rs:335, 346, 403, 431`. Sweep at `4d10ba45` was scope-bounded to `nomad_ch.rs` + selected sites. ~30 LOC search-and-rewrite to close.

**Severity**: MINOR, unchanged.

---

## Verified closed since r30

| r30 finding | Status at r31 |
|---|---|
| r30-A1 (IMPORTANT) ZSBX_* env block | **OPEN** — restated as r31-A1. No code change. |
| r30-A2 (IMPORTANT) `nomad_stop_permits` on `AppState` | **OPEN** — restated as r31-A2. No code change. |
| r30-A3 (MINOR) stale "wrapper" rustdoc | **OPEN** — restated as r31-A4. |
| r30-A4 (MINOR) `restore_handler.rs` module split | **OPEN, deferred per r30 recommendation** — production LOC unchanged at ~2772; below the ~6k threshold r30 set for triggering the split. No re-flag needed. |
| r29-A4 (carried) typed staged-images contract | **OPEN, no change.** |
| r29-A3 (carried) per-backend setter taxonomy | **OPEN, no change.** |
| r29-A5/A6 (MINOR carries) | **OPEN, no change.** |

---

## Net assessment

The window since r30 was reviewer paperwork + a 5-line cadence cherry-pick + a perf validation report. No architectural shape moved. Both r30 IMPORTANT findings carry forward verbatim; r31's one new MINOR (r31-A3) is the architectural follow-up question the v25 validation surfaced.

**Arch sprawl from `a6e517b2`**: none. The cherry-pick is pure constant updates (sleep durations + doc comments). No new types, no new functions, no surface evolution. `wait_for_agent_livez`'s `probe_elapsed`-aware cadence is structurally preserved per the commit message; the diff confirms this.

**Module-split status (r30-A4)**: `restore_handler.rs` LOC unchanged at 5326 (~2772 production, ~2554 inline tests). `nomad_ch.rs` unchanged at 8283 LOC. Neither has crossed the threshold where the split's payoff justifies the churn. Defer.

**Biggest open risk**: still r30-A1 / r31-A1. Two builders writing the same VM-launch inputs in two formats, manually kept in sync, with self-warning rustdoc that no one has actioned for two cycles. The shape is exactly r29's "fix the instance, leave the trap" antipattern — the controllers' own rustdoc names the smell. Land the deletion in the next backlog-drain cycle.

**Three decisions for the next reviewer (r32)**:

1. **If r31-A1 + r31-A2 still untouched at r32, downgrade r31-A1 to MINOR** (the shape is stable; restating IMPORTANT three rounds running is reviewer noise). Keep r31-A2 IMPORTANT until the per-backend-cap precedent question is addressed.
2. **Defer r31-A3 architectural ADR** until a second perf cycle (cadence quick-win measurement) lands. If the controller-vs-driver split is still gap-shaped at that point, escalate to IMPORTANT.
3. **Do not re-flag r30-A4** module-split until `restore_handler.rs` production LOC crosses ~3.5k or a code-quality reviewer cross-flags.
