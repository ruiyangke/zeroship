# Sandbox/snapshot-restore — api-surface r33 review

Date: 2026-05-25 (UTC). HEAD at audit: `fe8c9216`.
Worktree: `/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`.
Read-only. Round 33 of pilot-cron loop. Prior: r32 (`8d82ecde`).

Commits since r32 (api-surface impact):

- `2faaf39b` — R32-P1: parallelise the two cold-boot `mkfs.ext4`
  subprocesses (`workspace.img` + `home.img`) via
  `std::thread::scope`. Adds two scoped-thread `spawn` sites
  inside the existing `spawn_blocking` closure at
  `nomad_ch.rs:1141-1175`. Also rewrites prose comments in
  `nomad_ch.rs`, `mod.rs`, `config.rs`, `restore_handler.rs`
  scrubbing stale "wrapper" attribution → "ch driver".
  **No pub deltas.**
- `fe8c9216` — `docs/reviews/` only (cycle 53 paperwork: arch
  r32 + code-quality r33 + security r32).

## Summary

**Net pub-count delta r32 → r33: 0.** `git diff 8d82ecde..fe8c9216
-- crates/sandbox crates/sandbox-agent | grep -cE '^[+-]\s*pub'` = 0.
No `+pub` lines, no `-pub` lines. The `std::thread::scope` block
introduced in R32-P1 expands inside the existing
`spawn_blocking(move || …)` closure body in
`pub(crate) async fn create_sandbox` — the two scoped `.spawn()`
calls return `ScopedJoinHandle` (a library type, never re-exported)
and the join handles are consumed locally. **Zero new items in
the symbol table.**

**Zero new findings, zero closures. Backlog steady at 11.**

## CRITICAL

None.

## IMPORTANT

### [R26-API1] (carry, 8 rounds) `nomad_driver_ch_destroy_task_unreaped_total` no operator surface

Out-of-tree `nomad-driver-ch`. Unchanged. Owner: out-of-tree
driver / observability ADR.

### [R20-API1] (carry, 10 rounds) schema-marker rewriter sites unchanged

4 path-derivation rewriter sites in `restore_handler.rs` /
`nomad_ch.rs`. Held pending driver-side validator. Owner:
security (driver-side validator landing).

## MINOR

### [R30-API1] (carry, 4 rounds) `AppState::nomad_stop_permits` returns `&Arc<NomadStopPermits>`

`lib.rs:306-310`:

```
pub fn nomad_stop_permits(
    &self,
) -> &Arc<crate::backend::nomad_ch::NomadStopPermits> {
    &self.nomad_stop_permits
}
```

(Correction to r32: this returns `&Arc<…>` directly, not
`Option<&Arc<…>>` — the field is always populated since
`ade8fb46`. The 9-item leak count stands.) Field at `:258`
already `pub(crate)`. Accessor still `pub`. Type
`NomadStopPermits` + 7 methods at `nomad_ch.rs:558,609,632,
638,654,715,729` still `pub`. r33 landed no narrowing.
**9 items.** Biggest leak in the crate — external test code
that calls `state.nomad_stop_permits().acquire().await` could
hold a permit indefinitely (DoS shape against `stop_inner`).
Fix: 9-line mechanical `pub`→`pub(crate)`. Owner: code-quality
r34.

### [R30-API2] (carry, 4 rounds) `metrics::inc/dec_nomad_stop_permits_in_use` `pub fn`

`metrics.rs:496,505`. Verified at audit. Callers in-file only.
Bundle with R27-API3 17-item sweep. Owner: code-quality r34.

### [R30-API3] (carry, half-closed) operator-facing docs still describe pre-T-8 architecture

`docs/runbooks/sandbox-nomad-ch.md` audience / prerequisites /
config table / triage all reference deleted `raw_exec` +
wrapper. **AGENTS.md task router** at line 27 still lists
`crates/sandbox/scripts/nomad-vm-wrapper.sh` — that file was
deleted at `cdcd670d`. Verified at r33 audit (`grep -n
nomad-vm-wrapper.sh AGENTS.md` → match at :27). Fix: ~30-line
prose update + 1-line AGENTS.md edit. r32-P1's comment-scrub
pass landed inside `crates/sandbox/src/` but did **not** touch
the operator-facing docs nor AGENTS.md. Owner: code-quality r34.

### [R29-API1] (carry) `release_vm_index_after` + `spawn_delayed_release_in_worker` `pub fn`

`nomad_ch.rs:401,443`. Visibility untouched at r33. Owner:
code-quality r34.

### [R29-API2] (carry) `spawn_delayed_release_in_worker` `pub` + `#[allow(dead_code)]` + zero production callers

`nomad_ch.rs:443-462`. Pick one of three coherent endings.
Owner: code-quality r34 / architecture.

### [R28-API3] (carry) `test-support` feature lacks crate-level rustdoc warning

`Cargo.toml:71-77` + `lib.rs:1-9`. 5 active `#[doc(hidden)]`
markers gated correctly; crate-root rustdoc still unmarked.
Owner: code-quality r34.

### [R27-API1] (carry) `Backend::builder` rustdoc lacks cross-reference to `BackendBuilder`

`backend/mod.rs:260-264`. Owner: code-quality r34.

### [R27-API3] (carry, 17-item sweep steady) `metrics::*_value` + `inc/dec_*` `pub` with zero external consumers

17 functions enumerated in r30. Target list unchanged. Owner:
code-quality r34.

### [R27-API4] (carry) `WakeErrorCode` rustdoc table 8 of 10 variants

`db.rs:1725-1736` table; `wire_code()` match at `:1749-1762`
covers 10. Missing from table: `StagingPathMissing`,
`AgentVersionMissing`. Owner: code-quality r34.

### Other carries (no movement)

R19-API2, R22-API2, R22-API3, R23-API2, R23-API3, R24-API2,
R24-API3, R24-MIG1, R24-SWEEP1, R25-API3, R25-API4, R25-API5,
R26-API5.

## §10.0 envelope state post-r33

**Inventory delta from r32**: none. r33's only in-scope commit
(`2faaf39b`) does not touch wire shape — it parallelises a
subprocess-spawn block + scrubs prose comments. The 32 §10.0
codes are unchanged. `WakeErrorCode` remains 10 variants.

**Fallible handler scan**: 12 `pub async fn` admin handlers at
`admin_handlers.rs:405-2055` (`list_all_sandboxes`,
`get_sandbox_detail`, `list_user_sandboxes`, `list_user_shares`,
`list_hosts`, `export_user`, `delete_user`, `snapshot_sandbox`,
`wake_sandbox`, `poll_wake`, `cold_boot_sandbox`,
`metrics_endpoint`). All route through `crate::error_envelope::
{ErrorEnvelope, error_response}` (imported at `:71`, verified
at audit). The `error_envelope` module's surface is entirely
`pub(crate)`. **Envelope consistency: clean.**

## R33-API-VERIFY1 — `std::thread::scope` audit (R32-P1)

Per brief: confirm the new parallelisation block adds no public
surface.

1. **Site**: `nomad_ch.rs:1141-1175` inside
   `spawn_blocking(move || { … })` at line `:1133`. The
   enclosing closure runs inside `pub(crate) async fn
   create_sandbox` body.
2. **New constructs**:
   - `std::thread::scope(|s| { … })` — closure expression, no
     item.
   - `let workspace_h = s.spawn(|| { … })` — local binding to
     `ScopedJoinHandle<'_, Result<…, String>>`. Not an item.
   - `let home_h = s.spawn(|| { … })` — same shape.
   - `let (workspace_res, home_res) = …` — locals.
3. **Error mapping**: `unwrap_or_else(|p| Err(format!("workspace
   .img mkfs panic: {p:?}")))` — converts the `Box<dyn Any +
   Send>` panic payload into a `String`-typed `Err`, matching
   the existing `format!("workspace.img: {e}")` shape so
   call-site error handling stays uniform. **Wire-shape
   neutral.**
4. **No new function items, no new types, no new pub use,
   no new const.** Confirmed via `git diff … | grep -E
   '^[+-]\s*pub'` = 0.

**Verdict**: R32-P1 adds zero public surface. Parallelisation
lives entirely inside the existing function body. Clean.

## R33-API-VERIFY2 — pub surface delta scan

Per brief: confirm pub-surface delta is 0 since r32.

1. **`git diff 8d82ecde..fe8c9216 -- crates/sandbox crates/
   sandbox-agent | grep -cE '^[+-]\s*pub'` = 0.** Zero pub
   lines added or removed.
2. **Strict pub-item count** at r33 across `crates/sandbox/
   src/` + `crates/sandbox-agent/src/`: 316. (r32 review
   reported "300" for `crates/sandbox/src/` only at a
   stricter pattern; including the agent crate brings it to
   316. Both metrics monotonic.)
3. **Indent-allowed pub count** at r33 in `crates/sandbox/
   src/` alone: 475. Unchanged from r32.
4. **`#[doc(hidden)]` items**: 5 active markers gated under
   `cfg(any(test, feature = "test-support"))`. Unchanged. No
   intent-leak.

**Verdict**: zero pub-surface drift this round. No new `pub
fn` candidates for `pub(crate)` narrowing because no `pub fn`
was introduced.

## R33-API-VERIFY3 — R30-API1 carry status

Per brief: any movement on the 3-round-running carry?

`lib.rs:306` still reads `pub fn nomad_stop_permits(&self) ->
&Arc<crate::backend::nomad_ch::NomadStopPermits>`. **Not
closed.** No narrowing PR landed during the 2-commit window
between `8d82ecde` and `fe8c9216`. r32-P1 was a perf-only diff;
`fe8c9216` was paperwork. The carry has been open since r30
and is now **4 rounds old**. Code-quality r33 received the
hand-off at r32 but did not act on it; re-hand to code-quality
r34.

**Status: open, carry into r34, 4 rounds old.**

## R33-API-VERIFY4 — error-envelope consistency

Per brief: §10.0 wire envelope still consistent across all 12
admin handlers?

All 12 `pub async fn` handlers import `error_envelope::
{error_response, ErrorEnvelope}` at `admin_handlers.rs:71`
(verified at audit). r33 commits did not touch
`admin_handlers.rs` at all (in-scope diff inventory is
`mod.rs` + `nomad_ch.rs` + `config.rs` + `restore_handler.rs`
prose comments + the one `nomad_ch.rs` parallelisation block).
Envelope rustdoc on `poll_wake` (`:1967-1969`) unchanged.
`wire_code()` callsites at `:1995`, `:2497`, `:2529` produce
codes from the typed enum. The 12 handlers all funnel through
`error_response()` for fault paths and emit `ErrorEnvelope`
directly only when a typed handler-error is converted in
place (`:1221` `SnapshotHandlerError::StateMismatch`, `:1231`
`SnapshotHandlerError::NotFound`).

**Verdict**: envelope consistency **clean**. Open carry is
R24-API3 (async `which` field) only.

## Cross-lens consensus

- **R32-P1 parallelisation (`2faaf39b`)**: net-positive perf
  (~1-2 s CREATE-path savings on first-sandbox-per-user); zero
  api-surface delta. `std::thread::scope` is the right idiom
  here — both child threads borrow from the enclosing
  `spawn_blocking` closure and join before the scope exits,
  so no `Arc` / `Send` dance is needed. The error-mapping
  pattern (`unwrap_or_else(|p| Err(format!("…panic: {p:?}")))`)
  is consistent with the surrounding `format!("workspace.img:
  {e}")` shape so call-site error handling stays uniform.
- **Prose scrub**: `2faaf39b` also rewrites ~30 inline
  comments / docstrings across `mod.rs`, `nomad_ch.rs`,
  `config.rs`, `restore_handler.rs` replacing stale "the
  wrapper" attribution with "the ch driver". Rustdoc-correctness
  improvement; no api-surface signal. R30-API3's
  **operator-facing** docs (`docs/runbooks/sandbox-nomad-ch
  .md` + AGENTS.md line 27) are still unscrubbed — the prose
  pass deliberately stopped at the crate boundary.
- **Code-quality r33 (`fe8c9216`)**: paperwork only, did not
  drain backlog. The 9-item R30-API1 narrowing remains the
  highest-leverage mechanical fix in the queue.
- **Security r32 / Performance r32 / Concurrency r32**: no
  api-surface intersect this round.

## Lens hand-off

- **Architecture r34**: R26-API1, R24-API3 unchanged.
- **Test-coverage r34**: all r30/r31/r32 handoffs unchanged.
- **Security r34**: R20-API1 (10-round, quadruply-motivated).
- **Code-quality r34**: bundle R30-API1 (9 items) + R30-API2
  (2 items) + R27-API3 (17 items) + R29-API1/2 + R30-API3
  (docs + AGENTS.md router) + R28-API3 + R27-API1 + R27-API4
  + R26-API5 + R22-API3 + R23-API2 + R23-API3 + R25-API5 into
  one mechanical sweep PR (~50 lines code + ~30 lines prose).
  R33 specific add: extend R30-API3 to include the AGENTS.md
  line 27 single-edit (delete `· crates/sandbox/scripts/
  nomad-vm-wrapper.sh` from the Nomad+CH router row).

## Backlog

24 entries total open. Pre-r30 carries (no movement r31-r33):
R19-API2, R22-API2, R22-API3, R23-API2, R23-API3, R24-API2,
R24-API3, R24-MIG1, R24-SWEEP1, R25-API3, R25-API4, R25-API5,
R26-API5, R27-API1, R27-API3 (17 items), R27-API4, R28-API3,
R29-API1, R29-API2 — all MINOR. R20-API1 + R26-API1 IMPORTANT
(multi-round). r30 carries: R30-API1 (9 items), R30-API2,
R30-API3 (half-closed) — all MINOR.

Net: r32 open = 11 → r33 open = 11 (0 closures, 0 new; net 0).

## Trend

- **r17-r22**: §10.0 envelope + typed-error landed.
- **r23-r27**: pre-flight + observability surface;
  BackendBuilder consolidated.
- **r28-r29**: minimum-disclosure forward-pressure; backlog
  drops to 9.
- **r30**: T-7/T-8 cutover SHRINKS pub surface; r30-A1
  semaphore ADDS pub surface (with overshoot); backlog rises
  to 11.
- **r31-r32**: steady state. Cadence-tightening + trace
  instrumentation landed without leaking surface. R31-M1
  closed the in-source half of R30-API3.
- **r33 (this round)**: **steady state continues**. Two
  commits in scope (1 perf parallelisation, 1 paperwork).
  **Zero pub-surface delta** (316 → 316 by the crate-pair
  filter; 475 → 475 by the sandbox-only filter). No new
  findings. No closures. R30-API1 enters its **4th round**
  unclosed.

**Defining theme of r33: parallelise without surface drift.**
R32-P1 added an in-closure `std::thread::scope` block — exactly
the kind of perf optimisation that historically tempts authors
to extract a `pub fn parallel_mkfs(…)` helper. r33 resisted:
the scope expands inside the existing `spawn_blocking` closure,
no new items. The prose-scrub half of the commit removes
~30 stale "wrapper" references from in-crate docstrings (a
quiet R31-M1-shaped closure of `crates/sandbox/src/` debt,
though it does not formally close any open finding because
the operator-facing docs + AGENTS.md router remain untouched).
The carry backlog is steady at 11; all 9 active MINOR items
are mechanical narrowings that will close in a single
~50-line code-quality sweep PR whenever scheduled. The 2
IMPORTANT items remain out-of-tree (R26-API1) and
multi-round-blocked (R20-API1).

**Net pub-count delta: 0.**

**Biggest leak (4 rounds running)**: `AppState::
nomad_stop_permits()` at `lib.rs:306` returning `&Arc<
NomadStopPermits>`. Fix: one-line `pub` → `pub(crate)`.
R30-API1 carries forward to r34.
