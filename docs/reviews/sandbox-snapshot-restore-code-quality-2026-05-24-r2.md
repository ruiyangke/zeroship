# Code-quality review — 2026-05-24 round 2

**Reviewer**: pilot-cron round-r2 (read-only)
**Worktree HEAD**: `09dfd902`
**Baseline**: r1 at `8ad3cf3f` (`docs/reviews/sandbox-snapshot-restore-code-quality-2026-05-23-r1.md`)
**Lens**: code-quality — deltas since r1 + audit of the new `stop_inner`, `ControllerIdleSnapshotter`, and async `IdleSnapshotter`

## Summary

- 7 findings: **1 CRITICAL**, 3 IMPORTANT, 3 MINOR. r1's two CRITICALs (preview.rs `unwrap`s) are unchanged and re-flagged below as still open.
- The B15 fix is structurally clean (`stop_inner` + `bool` gate + a sibling test), and T6's `ControllerIdleSnapshotter` does share the real handler — no parallel pipeline. But r1's systemic patterns multiplied: `Result<_, String>` 142 → **166** sites; `Duration::from_secs(N)` literals **63 in nomad_ch alone**; `.lock()/.read()/.write().unwrap()` still **31 in registry.rs**.

## CRITICAL

1. **`crates/sandbox/src/sweep.rs:443-468`** — `per_iteration_concurrency` is a no-op. The comment at 438-442 promises chunked concurrency ("snapshot at most `per_iteration_concurrency` at a time"), but both the outer `for chunk in rows.chunks(cap)` and the inner `for r in chunk { … .await }` are strictly sequential — no `join_all`/`FuturesUnordered`. At the documented 2.1 s/snapshot, a 100-row batch (`IDLE_BATCH_LIMIT`) serializes to **~210 s**, exceeding the 300 s sweep interval and risking back-pressure during eviction storms. Either drop the chunking machinery and rename the knob to a per-tick budget, or actually parallelize the chunk (e.g. `futures::future::join_all`). The compio "no Semaphore" excuse doesn't apply — `chunks(cap).await` over a `join_all` collected from the chunk is one line.

## IMPORTANT

2. **`crates/sandbox/src/sweep.rs:390-407` ⨯ `crates/sandbox/src/admin_handlers.rs:1109-1129`** — `ResolvedSourceVmOps` is duplicated verbatim across the two snapshot entry-points. Same fields, same three trait impls, same comments. The sweep copy's own doc-comment (`sweep.rs:385-389`) admits "duplicating it avoids a pub-export churn for a 20-line type" — i.e. the author saw the duplication and shipped it. r1's "share the pipeline" prediction held for `snapshot_sandbox` but not for this adapter. Hoist into `snapshot_handler` (the trait's owner) as `pub(crate) struct ResolvedSourceVmOps`. Cost: one `pub(crate)` keyword.

3. **`crates/sandbox/src/backend/nomad_ch.rs:920-1185`** — `stop_inner` is now **266 LOC** (was 249 in r1 as `stop`). The B15 fix added a `bool` gate and inlined two branches around step 5, growing the body. r1's split-by-step recommendation is now stronger: extract `step_drain_agent`, `step_purge_nomad`, `step_wait_gone`, `step_fence_and_release`, `step_rm_host_dir`. Sibling `try_create` is **288 LOC** — the worst offender remains untouched. Neither has crossed 500 LOC, but both are above the 200-LOC ceiling r1 flagged.

4. **`crates/sandbox/src/sweep.rs:249-253`** — the new async `IdleSnapshotter::snapshot_one` returns `Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>`. The trait is `Send + Sync` but the returned future is **not** `+ Send`. Today this works because compio is single-threaded, but every other async-trait helper in the workspace returns `+ Send` futures by convention; the silent divergence is a footgun for anyone who later tries to spawn this on a multi-threaded runtime, or who pulls `IdleSnapshotter` into a non-compio test harness. Either add `+ Send` to the boxed future (matches the bound on the trait) or drop the `Send + Sync` bound on the trait itself.

## MINOR

5. **`crates/sandbox/src/sweep.rs:292`** — `pub struct ControllerIdleSnapshotter` is referenced only from `lib.rs:505`. Same for `pub fn new` at 297 and `pub struct RecordingIdleSnapshotter` (used internally + `#[doc(hidden)]`). Demote to `pub(crate)`. r1 flagged the same pattern in `AppState::admin_token` (still open per A5 in the deferred file).

6. **`crates/sandbox/src/restore.rs:379`** + **`handlers.rs:416, 840, 1103, 1190`** + **`backend/docker.rs:589`** + **`backend/k8s.rs:1293-1294`** — production paths still pattern-match on substrings (`e.contains("doesn't yet support")`, `e.contains("stale agent")`, `out.stderr.contains("No such container")`). `handlers.rs:410-414` even documents the smell in a comment ("If retry classification grows past this it should be promoted to a typed error.") yet four new `.contains` sites have been added since that comment landed. This is r1's "stringly-typed errors crossing 13 files" point — `Result<_, String>` count grew from 142 → **166** since r1.

7. **`crates/sandbox/src/snapshot_handler.rs:675`** vs **`sweep.rs:331` / `admin_handlers.rs:1177`** — `snap_stage_dir`'s parameter is named `host_state_dir`, but both callers pass `config.snapshot_l1_root`. Same path, two names — r1's "three different names per host path" finding extended. Rename the param to `snapshot_l1_root` or land r1's proposed `paths.rs`.

## r1 status — what closed, what didn't

- **CLOSED**: none. r1's 2 CRITICALs (`preview.rs:239,268,629-631`) are byte-identical at `09dfd902`. The eight IMPORTANTs (PoisonError split, magic durations, long `fn`s, `Result<_, String>` everywhere, host-path naming, `Option<Arc<dyn _>>` wiring, stringly-typed status, snapshot store `&str` keys) are all still present — some have grown (Duration::from_secs 30+ → **63 in nomad_ch.rs**; Result<_, String> 142 → **166**).
- **REGRESSED**: r1's #4 (long `fn`s in nomad_ch) — `stop_inner` is now 266 LOC vs the 249 LOC `stop` r1 measured. The B15 fix was correct but landed in the wrong shape.
- **NEW-AND-CLEAN**: The `stop_inner(bool)` + `stop_preserving_state` split is the right factoring for the bug. The pg-gated integration test at `nomad_ch.rs:4168-4238` pins the invariant. The `ControllerIdleSnapshotter` correctly reuses `snapshot_handler::snapshot_sandbox` rather than copying it (only the 17-LOC adapter struct is duplicated — see finding 2).
