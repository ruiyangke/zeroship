# Sandbox/snapshot-restore — api-surface r15 review

Date: 2026-05-25 (UTC)
HEAD at audit: `b8fae7b7`.
Round 15 of N.
Prior: r14 at `af4678ac` (`docs/reviews/sandbox-snapshot-restore-api-surface-2026-05-25-r14.md`).

Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`. Read-only.

## Summary

- **0 new findings**. All four r14→r15 commits the prompt asked to
  audit (`493d6c1e`, `91ce9be5`, `9afd0986`, `79b4d258`) add zero
  new `pub` items per `git show … | grep "^+pub"`. The largest
  visibility delta in the window came from a single pre-prompt
  commit (`b2892368`, the C-4 retry-loop fix that landed during
  the r14 cycle) and is fully justified by an already-pub trait
  surface — analysed below as a **non-finding** so it's on the
  record.
- **2 carries closed since r14**:
  - **R14-API1 (CLOSED)** at `00161cea` — `RealRestoreBackend::
    with_shared_allocator` + `with_nomad_handle` demoted to
    `pub(crate)`. Two-token edit; struct stays `pub` for the
    e2e-test constructor. Carry of 1 round.
  - **R11-API1 (expanded, CLOSED)** at `370fdbba` — 3
    `#[doc(hidden)] pub fn` orphans deleted outright from
    `metrics.rs` (`takeover_unreachable_value`,
    `takeover_corrupt_value`, `sandbox_corrupt_id_value`). Carry
    of 4 rounds, expansion in r14 closed in same round as the
    base.
- **Carry-forward still open at HEAD `b8fae7b7`** (3 items, down
  from 4 at the start of r14):
  - **R10-API1** — `_test_build_auth_from_sealed` orphan `pub`
    at `restore.rs:613`. **6th-round carry.** Re-verified at HEAD;
    `grep -rn '_test_build_auth_from_sealed' --include="*.rs"` in
    the worktree returns the definition site only.
  - **R10-API4** — readyz §10.0 envelope drift in sandbox-AGENT
    at `crates/sandbox-agent/src/handlers.rs:498-510`. **6th-round
    carry.** Two non-§10.0 503 paths still verbatim:
    `{"status":"draining"}` (line 501) and
    `{"status":"reaper-down"}` (line 508). Clustered with R12-API1
    (sandbox crate's readyz, `handlers.rs:132-139`).
  - **R12-API1** — sandbox crate's `readyz` non-§10.0 wire shape.
    **4th-round carry.** Clustered with R10-API4.
  - **R14-API2** — Retry-After header docstring drift on 503
    `vm_index_unavailable`. **2nd-round carry.** Re-verified at
    HEAD: `grep -rn 'Retry-After' crates/sandbox/src` still returns
    **only the docstring** at `restore_handler.rs:58`. No emission
    site anywhere in either sandbox or sandbox-agent crate. Pasted
    response builder at `admin_handlers.rs:1157-1163` confirms — no
    header. C-4/C-7 retry-budget work landed but did not touch the
    server-side header advisory.
- **Net new `pub` r14→r15** (`af4678ac..b8fae7b7` for
  `crates/{sandbox,sandbox-agent}/src`): added 9 pub-token lines,
  removed 5; net +4 (844 → **848**).
  - +9 added: `pub struct VmIndexRetryPolicy` + 2 fields (3 from
    `b2892368` C-4 fix) + `pub(crate) async fn
    reserve_vm_index_with_retry` (1 from `b2892368`) + 3 new
    `StubRestoreBackend` test-stub fields (`vm_index_retry_policy`,
    `reserve_succeeds_on_attempt`, `reserve_attempts` — 3 from
    `b2892368`) + 2 lines re-emitted by R14-API1's `pub` →
    `pub(crate)` demote (the regex matches both kinds equally — no
    net surface, but +2 in the grep diff).
  - -5 removed: 3 lines from R11-API1's metrics.rs deletion + 2
    lines from R14-API1's pre-demote `pub fn` definitions.
  - **Top-level `pub`** (`^pub[[:space:](]`): 330 → **329** (-1).
    Net top-level shrinkage despite the +9/-5 split because the
    metrics deletion was at column 0 and the C-4 additions are
    mostly inside `impl` or struct bodies.
- **`Result<_, String>`**: sandbox **161**, sandbox-agent **14** at
  `b8fae7b7` — flat r14→r15 (now **4 consecutive rounds** at
  161/14).
- **`RestoreBackend` trait method count**: still **8**. Flat. (C-4
  added the `vm_index_retry_policy()` method during the r13→r14
  window; r15 didn't add or remove any.)
- **`StubRestoreBackend` shape**: now 13 pub fields (was 10 at
  r13). Increase explained below — `#[doc(hidden)] pub struct`
  precedent for an integration-test scaffold consumed by
  out-of-crate `tests/sandbox_pg_e2e.rs`. Not a new finding (see
  §"Non-finding: VmIndexRetryPolicy & StubRestoreBackend C-4
  fields").
- **No new wire endpoints** r14→r15. Zero new HTTP routes, zero
  new request shapes, zero new error envelope fields. The C-7 fix
  (`493d6c1e`) tunes a numeric constant inside `Default for
  VmIndexRetryPolicy`; the C-6 fix (`91ce9be5`) is a function-body
  swap inside a private `admin_handlers.rs` helper. Both
  api-surface-clean.

## Audit of the four r14→r15 commits the prompt called out

### 1. `493d6c1e` — C-7 fix (retry budget 60×2s → 25×2s + per-attempt log)

```
$ git show 493d6c1e -- crates/sandbox/src/restore_handler.rs \
    | grep -E "^\+[[:space:]]*pub" | head
```
→ **empty**.

Verification: the commit changes `Default for VmIndexRetryPolicy`'s
`max_attempts: 60` → `25` (lines 169-182 at HEAD) and inserts an
`tracing::info!` before the existing `reserve_vm_index` call inside
`reserve_vm_index_with_retry` (lines 380-388). It also replaces the
C-4 #4 "default policy budget ≥ 90 s" guard test with a C-7 inverse
guard (`budget + 5 s headroom ≤ 60 s client deadline`) — both are
`#[compio::test]`s inside `#[cfg(test)] mod tests`, no `pub`
attached.

**Verdict: api-surface-clean.** Zero new pub items. The
`VmIndexRetryPolicy` struct (already `pub` from `b2892368`) is
unchanged in shape; only the default field values change.

### 2. `91ce9be5` — C-6 fix (detach `teardown_source_for_snapshot` on dedicated OS thread)

```
$ git show 91ce9be5 -- crates/sandbox/src/admin_handlers.rs \
    | grep -E "^\+[[:space:]]*pub" | head
```
→ **empty**.

The fix replaces the body of `do_snapshot`'s detach pattern at
`admin_handlers.rs:1311-1324`. Before: `compio::runtime::spawn
(async move { teardown_source_for_snapshot(…).await }).detach()`.
After: `std::thread::Builder::new().name("snap-teardown-…")
.spawn(move || { compio::runtime::Runtime::new()
.block_on(async move { … }) })`. Same shape as C-3's
`Tiered::put` OS-thread escape (`c890c015`). Same `pub(crate)`
visibility on `teardown_source_for_snapshot` — no shift.

**Verdict: api-surface-clean.** Pattern is the same body-of-fn
change C-3 used: zero touch on signatures or types.

### 3. `9afd0986` — R14-Q3 + R14-P2 (`snap-l2-upload` thread name simplification)

```
$ git show 9afd0986 -- crates/sandbox/src/snapshot_store_gcs.rs \
    | grep -E "^\+[[:space:]]*pub" | head
```
→ **empty**.

Verification: this changes the `std::thread::Builder::new().name
(...)` argument inside `Tiered::put`'s L2 upload path (the same
function C-3 fixed at `c890c015`). Pure string-building
simplification — no signature, no type, no visibility touch.

**Verdict: api-surface-clean.**

### 4. `79b4d258` — R14-Q2 (`cfg(test)`-gate `seal_filename_for_str`)

```
$ git show 79b4d258 -- crates/sandbox/src/persist.rs \
    | grep -E "^\+[[:space:]]*pub" | head
```
→ **empty**.

The fn was already `pub(crate)` from `f50c95da` (R10-API3
closure). r14-Q2 adds `#[cfg(test)]` above the existing
`pub(crate) fn seal_filename_for_str(...)` definition + a 4-line
doc-comment explaining the gate. Visibility token unchanged.
**Net effect on production builds**: -1 pub-token (the fn now
compiles out entirely outside test builds), but the canonical
trend regex counts source-text pub tokens, so the measurement is
unchanged — which is correct: the source shape is what
api-surface watches.

**Verdict: api-surface-clean.** The cfg-gate is a build-shape
tightening, not a visibility change.

## Non-finding: `VmIndexRetryPolicy` & `StubRestoreBackend` C-4 fields

The +6 net new pub-token-emitting lines from `b2892368` (C-4 fix,
which landed during the r13→r14 window and was first visible in
the api-surface trend at r15) deserve an explicit analysis. They
look like a surface expansion but are all justified by existing
contracts.

| Symbol | Location | Visibility | Justified? |
|---|---|---|---|
| `pub struct VmIndexRetryPolicy` | `restore_handler.rs:157` | `pub` | YES — return type of `RestoreBackend::vm_index_retry_policy()` (line 342), a method on a `pub` trait consumed by external `tests/sandbox_pg_e2e.rs` via `dyn RestoreBackend`. `pub(crate)` would cause "private type in public interface". |
| `pub max_attempts: u32` | `restore_handler.rs:161` | `pub` | YES — config-style struct where backends construct via struct literal: `VmIndexRetryPolicy { max_attempts: N, interval: D }` (4 occurrences in tests at lines 1307, 1348, 1382). `pub` fields are the idiomatic shape for parameterised-config types; `with_*` builders would add 2 fn defs for no behavioural gain. |
| `pub interval: Duration` | `restore_handler.rs:166` | `pub` | YES — same rationale as `max_attempts`. |
| `pub(crate) async fn reserve_vm_index_with_retry` | `restore_handler.rs:362` | `pub(crate)` | YES — correctly visibility-restricted at birth. Only called from `restore_handler.rs` itself (lines 678 + 3 test sites). Could be plain `fn`-scoped, but `pub(crate)` lets the same-file `#[cfg(test)]` `tests` mod call it without an extra `super::` chain. |
| `pub vm_index_retry_policy: VmIndexRetryPolicy` | `restore_handler.rs:1107` | `pub` | YES — field of `StubRestoreBackend` (`#[doc(hidden)] pub struct` consumed by `tests/sandbox_pg_e2e.rs` at 5 sites). Existing precedent: all 10 prior fields on `StubRestoreBackend` are `pub` for direct mutation in tests. |
| `pub reserve_succeeds_on_attempt: Option<u32>` | `restore_handler.rs:1112` | `pub` | YES — same precedent. Used at line 1310 in same-file tests: `stub.reserve_succeeds_on_attempt = Some(3)`. **No external consumer yet** — but the field shape (test scaffolding) explicitly invites future external tests. |
| `pub reserve_attempts: std::sync::atomic::AtomicU32` | `restore_handler.rs:1113` | `pub` | YES — same precedent. Tests at lines 1334, 1372, 1409 read it via `stub.reserve_attempts.load(...)`. |

**Out-of-file consumers of the 3 new `StubRestoreBackend` fields**:
`grep -rn 'reserve_succeeds_on_attempt\|reserve_attempts' --include="*.rs"
/home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore` returns
only the 3 definition sites + 6 same-file test refs. Zero external
consumers at HEAD. **This is the only spot in the +6 cluster where
a tighter visibility would be defensible** — but it would require
splitting `StubRestoreBackend` into a public 10-field part + a
private 3-field part, which is wildly out of proportion to the
hypothetical benefit. The `#[doc(hidden)]` attribute is the right
discouragement; the struct as a whole is already documented as
"test scaffolding". **No finding filed.**

## Sibling-C-6 sites — pub item check (item 6 from the prompt)

The C-6 fix at `91ce9be5` audited three sibling sites in its commit
message. Pub-surface implications:

| Site | Enclosing fn | Visibility of enclosing | Notes |
|---|---|---|---|
| `crates/sandbox/src/backend/nomad_ch.rs:2002` | `<NomadCreateGuard as Drop>::drop` body | n/a (trait impl) | Drop body. Per the C-6 commit message: lower-risk because no concurrent wake races the same `vm_index`. The fix, if applied later, is body-internal — no pub surface touched. |
| `crates/sandbox/src/sweep.rs:563` | `spawn_idle_eviction_sweep` (line 552) | `pub` | Consumed only by `lib.rs:842` (in-crate). Could be `pub(crate)`, but this is independent of C-6 — the fn was `pub` long before C-6 and stays `pub` regardless of body change. Filing as a finding here would duplicate the long-standing R10-/R12-class "in-crate-only pub" pattern that the api-surface lens already tracks holistically; not separately worth a new ID. |
| `crates/sandbox/src/registry.rs:829` | `start_idle_gc` (line 828) | `pub` | Consumed by `main.rs:105` (bin/lib-split — same crate, different compilation unit). **Must be `pub`** for the bin to call into the lib. Same justification as `not_found` in sandbox-agent (R10-API2 not_found half). Not a finding. |

**Conclusion on item 6**: applying the C-6 OS-thread pattern to any
of the three sibling sites is a body-of-fn change with zero
pub-surface impact. The enclosing pub-vs-pub(crate) question on
`spawn_idle_eviction_sweep` is decoupled and not worth filing.

## Carry-forward (still open at HEAD `b8fae7b7`)

Reference lines re-verified against HEAD.

- **R10-API1** — `_test_build_auth_from_sealed` orphan `pub` at
  `restore.rs:613`. **6th-round carry.** `grep -rn
  '_test_build_auth_from_sealed' --include="*.rs"
  /home/ruiyang/Projects/appbase/.worktrees/sandbox-snapshot-restore`
  returns the definition site only. Cheapest path: either prefix
  with `#[cfg(test)]` (in-file test consumer) or delete. **R10-API1
  is now the longest-running unclosed api-surface carry.**
- **R10-API4** — readyz non-§10.0 wire shape in **sandbox-agent**
  at `crates/sandbox-agent/src/handlers.rs:498-510`. **6th-round
  carry** (matches R10-API1 carry length). Verbatim:
  ```rust
  pub async fn readyz(state: State) -> HttpResponse {
      if state.is_draining() {
          return HttpResponse::ServiceUnavailable()
              .json(&json!({"status": "draining"}));
      }
      if !crate::reap::is_healthy() {
          return HttpResponse::ServiceUnavailable()
              .json(&json!({"status": "reaper-down"}));
      }
      HttpResponse::Ok().json(&json!({"status": "ready"}))
  }
  ```
  Two non-§10.0 503 envelopes (`{"status":"draining"}`,
  `{"status":"reaper-down"}`). Clustered with R12-API1.
- **R12-API1** — sandbox crate's `readyz` non-§10.0 wire shape at
  `crates/sandbox/src/handlers.rs:132-139`. **4th-round carry.**
  Two paths: `{"status":"ready"}` and
  `{"status":"backend-unhealthy"}`. Clustered with R10-API4 for a
  single carve-out decision.
- **R14-API2** — Retry-After header docstring drift on 503
  `vm_index_unavailable`. **2nd-round carry.** Docstring at
  `restore_handler.rs:58` still promises `(Retry-After)`; response
  builder at `admin_handlers.rs:1157-1163` still emits no header.
  C-4/C-7 retry-budget work landed but **did not address the
  server-side header advisory** — clients of the new clean-503
  failure mode (introduced by C-7's 50 s budget) will read the
  docstring and look for a `Retry-After` header that isn't there.
  **Severity reconsideration**: with C-7 landing, this 503 is now a
  real production-observable failure mode (not a theoretical
  edge); recommend bumping severity from MINOR to MINOR+ unless
  the long-term async-response (C-7-LT) plan obsoletes the
  `Retry-After` design.

## Closed by recent commits (api-surface scope)

| Finding | Commit | Lens-relevant change |
|---|---|---|
| R14-API1 (RealRestoreBackend builders) | `00161cea` | 2× `pub fn` → `pub(crate) fn` (`with_shared_allocator` + `with_nomad_handle`); struct stays `pub` for e2e-test constructor |
| R11-API1 (expanded: 3 fns) | `370fdbba` | 3× `#[doc(hidden)] pub fn` deletions in `metrics.rs` (`takeover_unreachable_value`, `takeover_corrupt_value`, `sandbox_corrupt_id_value`) |

**2 carries closed this round** — fewer than r14's 6, matching the
trend prediction ("r14→r15 to revert toward the baseline (0–1
closures per round)"; we got 2, slightly above forecast because
the R14-API1 closure was a same-cycle micro-PR teed up by r14
itself).

## Recommended fix order

1. **R14-API2 (Retry-After docstring drift)** — now an
   active-production-path concern since C-7's 50 s budget gives
   clients a real `503 vm_index_unavailable` to retry from. Choose
   option 1 (add `Retry-After: <calculated>` header + assertion in
   `a4_map_restore_error_vm_index_unavailable_envelope` test) or
   option 2 (strip the docstring's `(Retry-After)` parenthetical
   to match reality). **Option 1 is strictly better** if the C-7
   client-disconnect failure mode lands in stress/smoke runs;
   option 2 is a fallback if the C-7-LT async-poll work is
   imminent and would obsolete the synchronous-retry shape.
2. **R10-API1 (`_test_build_auth_from_sealed` orphan pub)** —
   **6-round carry**, mechanical 1-token edit (`pub fn` →
   `#[cfg(test)] fn` if same-file test, else delete). Longest
   unclosed item in the backlog; cheapest fix in the backlog.
3. **R10-API4 + R12-API1 (readyz §10.0 envelope drift cluster)** —
   carve-out policy decision first (2× sandbox-agent paths + 2×
   sandbox-crate paths). Either define `readyz` as
   "shape-stable-pre-§10.0" with a comment at each non-envelope
   site, or convert all 4 to envelope-shaped 503s. Mechanical
   edit once the policy is fixed.

The api-surface backlog is now **3 open findings (R10-API1,
R10-API4, R12-API1, R14-API2)** = **4 items**. Down from 6 at end
of r14. Closure velocity remains positive.

## Two most-critical citations

1. **`crates/sandbox/src/restore_handler.rs:58`** — *"`VmIndexUnavailable`
   → 503 `vm_index_unavailable` (Retry-After)"*. Docstring promises a
   header that does not exist in the response. **C-7's landing
   (`493d6c1e`, ~6 hours before r15) bumps this from theoretical
   to live**: the new 50 s budget is engineered to surface clean
   503s rather than silent stalls, so clients will start consuming
   this error in the field. They will read the docstring and look
   for a `Retry-After` header. The response builder at
   `admin_handlers.rs:1157-1163` still emits only the JSON
   envelope. **This is the only finding in the backlog that
   touches a live wire-shape contract; the other three are
   visibility tightening or doc-fix tier.**
2. **`crates/sandbox/src/restore.rs:613`** — `pub fn
   _test_build_auth_from_sealed(...)` — 6-round carry. Now the
   longest-running unclosed api-surface item after r14 closed
   R10-API3 (4-round) and R11-API1 (4-round, expanded). 1-token
   mechanical edit blocks. Treat the longevity itself as the
   signal: the item is small enough to be invisible in any single
   round's planning, so it needs an explicit "always include in
   the visibility-tightening cluster" tag.

## Trend

- **`Result<_, String>`** (canonical regex `Result<[^,>]+,\s*String\s*>`,
  measured via `find … -name '*.rs' -print0 | xargs -0 grep -cE … |
  awk '{sum+=$NF}'`):
  - sandbox **161** at `af4678ac`, **161** at `b8fae7b7` — **flat
    r14→r15** (**4 consecutive rounds at 161**).
  - sandbox-agent **14** at both — **flat, at floor (6 consecutive
    rounds at 14)**.
- **`pub`-token count** (`^[[:space:]]*pub[[:space:](]`):
  - r14: 844
  - **r15: 848** — +4. Source: net +6 from `b2892368` C-4 (during
    r14 cycle, first visible in r15 measurement) minus 3 from
    R11-API1 closure (`370fdbba`) minus 0 from R14-API1 demote
    (visibility-neutral under the canonical regex). **Of the +6
    new C-4 pub items, 0 are unjustified by an already-pub trait
    surface or `#[doc(hidden)] pub struct` precedent** — see
    §"Non-finding".
  - Top-level `pub` only (`^pub[[:space:](]`): **329** at r15 (-1
    from 330 at r14). Top-level shrank because metrics deletions
    were at column 0 while C-4 additions are mostly inside
    `impl`/`struct` bodies.
- **`RestoreBackend` trait method count**:
  - r14: 8
  - **r15: 8** — flat. (Method 8, `vm_index_retry_policy()`, was
    added during r13→r14 by `b2892368`; r14 measured it at 8.
    r15 confirms no further movement.)
- **Net new wire endpoints** (r14→r15): **0**. All r14→r15 commits
  are: docs (cluster-r6/r7/r8 review artifacts + deferred
  refresh), C-3-mirror admin-detach OS-thread fix (`91ce9be5`),
  C-7 numeric-constant + log-line tune (`493d6c1e`), R14-Q3/R14-P2
  thread-name simplification (`9afd0986`), R14-Q2 cfg-gate
  (`79b4d258`), R14-API1 demotion (`00161cea`), R11-API1 expanded
  deletion (`370fdbba`), R14 reviewer pilot artifacts, controller
  pin bumps v22→v23. **No new HTTP routes, no new request shapes,
  no new error envelope fields.**
- **Closure velocity** (api-surface findings closed per round):
  - r10→r11: 0
  - r11→r12: 0
  - r12→r13: 1
  - r13→r14: 6
  - **r14→r15: 2** — slightly above the predicted 0–1 baseline.
    R14-API1's same-cycle closure was teed up by r14 itself; the
    R11-API1 expansion fell out of the same micro-PR cluster.
    Expect r15→r16 to drop to 0–1 as the backlog now requires
    judgment calls (R14-API2 docstring-vs-header policy; R10-API4
    + R12-API1 readyz carve-out policy) rather than mechanical
    edits.
- **Backlog open-item count** (api-surface lens):
  - r12: 7 (after R10-API3 partial closure)
  - r13: 8 (after R13-API1 + R13-API2 NEW)
  - r14: 6 (after r14's 6 closures)
  - **r15: 4** (after r15's 2 closures, 0 NEW). **Lowest
    open-item count since the lens opened at r10.**
