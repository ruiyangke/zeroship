# Sandbox/snapshot-restore — api-surface r13 review

Date: 2026-05-25 (UTC)
HEAD at audit: `e887b8ee`
Round 13 of N.
Prior: r12 at `ae946cba` (`docs/reviews/sandbox-snapshot-restore-api-surface-2026-05-25-r12.md`).

Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`. Read-only.

## Summary

- **1 new finding** (0 critical, 0 important, 1 minor). The sandbox crate
  has its own `ExecBody` over-pub — a sibling of the R10-API2 finding
  for the agent crate. Pre-existing code, not a regression.
- **Closures r12→r13**: none on the api-surface lens. R10-Q5 closed
  this round but was a code-quality finding (deleted `_ref_imports`
  helper from `proxy.rs`); audited here only to confirm no other dead
  fns of the same shape leaked over the lens boundary — and none did.
- **Net new `pub` r12→r13 (`d2cfcb34..e887b8ee` for full scope; or
  `ae946cba..e887b8ee` for the r12-audit→r13 delta): zero.**
  `git diff ae946cba..e887b8ee -- crates/sandbox{,-agent}/src | grep
  '^[+-][[:space:]]*pub'` returns empty.
- **Net new `pub(crate)` r12→r13: zero.** R12-I1's wake-path TaskDriverMode
  threading is a private fn (`build_restore_nomad_job_json` at
  `restore_handler.rs:1276` — no `pub` keyword) gaining one parameter,
  plus a new test-module-private static + helper fn. No surface added.
- **`RestoreBackend` trait method count**: **still 8**. R12-I1 did NOT
  add a trait method (the mode is threaded through a private builder
  fn, not through the trait surface). Trait stays at `reserve_vm_index`,
  `release_vm_index`, `restore_alloc_dir`, `submit_restore_job`,
  `wait_for_livez`, `teardown_restore`, `register_restored`,
  `derive_agent_url`. No drift this round.
- **`Result<_, String>`** sandbox **161**, sandbox-agent **14** at
  `e887b8ee`. Sandbox identical to `ae946cba` (re-measured against
  the same commit — the r12 "144" figure couldn't be reproduced with
  the published regex; using r12's regex `Result<[^,>]+,\s*String\s*>`
  against `ae946cba` yields 161, identical to `e887b8ee`). **Trend is
  flat r12→r13 against any consistent measurement.**
- **`pub`-token count** (indented `pub(crate)` included): **841** at
  both `ae946cba` and `e887b8ee` — flat exactly. Top-level `pub` only:
  **329**.
- **ErrorEnvelope drift**: unchanged from r10 — agent's
  `error_envelope.rs:33-42` carries the documented "do not lift into
  zeroship-core yet" rationale. No regression, no convergence either.

## R12-I1 pub items audit

R12-I1 (`b3bf741c`, `sandbox/restore: wake-path respects SANDBOX_TASK_DRIVER
feature flag`) — the wake-path `build_restore_nomad_job_json` now takes a
`mode: TaskDriverMode` parameter that mirrors the cold-boot builder's
match-on-mode shape.

`git show b3bf741c -- crates/sandbox/src/restore_handler.rs | grep '^+pub '`
→ **empty.**

`git show b3bf741c -- crates/sandbox/src/restore_handler.rs | grep -E '^\+' |
grep -E 'pub\(crate\)'` → only doc-comment text references; **zero new
`pub(crate)` items**.

What R12-I1 actually added, by visibility:

| Symbol | Path | Visibility | Justified? |
|---|---|---|---|
| (param add) `mode: TaskDriverMode` on `build_restore_nomad_job_json` | `restore_handler.rs:1276-1286` | fn stays **private** | YES — fn was already private; adding a param doesn't widen visibility. The `TaskDriverMode` type itself is `pub(crate)` in `nomad_ch.rs` (added in T-7 — audited at r12) and is reachable here via `crate::backend::nomad_ch::TaskDriverMode`. Correct. |
| `R12_I1_ENV_LOCK` static | `restore_handler.rs:2478` | **module-private** (inside `mod tests`) | YES — test serialisation lock for env mutation, mirrors `T7_ENV_LOCK` in `nomad_ch::tests`. Not `pub(crate)` deliberately — the prior round's audit (r12) noted that cross-crate sharing of T7_ENV_LOCK would expose pub(crate) test internals; r13's mirror copy stays scoped to the new test module. |
| `with_task_driver_env` fn | `restore_handler.rs:2485` | **module-private** | YES — test helper, only consumed by the 5 new tests in the same `mod`. |
| 5 new `#[test] fn`s | `restore_handler.rs:2508/2553/2580/2620/2654` | module-private | YES — standard test visibility. |

**Verdict**: R12-I1 is **api-surface-clean**. Zero new `pub` items, zero
new `pub(crate)` items. The behavioural change rides on a parameter
addition to an already-private fn — exactly the right visibility shape
for an in-crate jobspec-builder switch. Mirrors the T-7 cold-boot
pattern audited and accepted at r12.

## Findings (NEW since r12)

### [R13-API1] Sandbox crate's `ExecBody` is over-pub — sibling of R10-API2's agent case (MINOR, api-surface-r13)

- **File**: `crates/sandbox/src/handlers.rs:797`
- **Symptom**:
  ```rust
  #[derive(Debug, Deserialize)]
  pub struct ExecBody {
      pub cmd: String,
      pub cwd: Option<String>,
      pub timeout_ms: Option<u64>,
  }
  ```
  Declared `pub struct` with `pub` fields. The only consumer is the
  sibling fn `exec` at `handlers.rs:803-808` via `web::types::Json<ExecBody>`
  in its signature.
- **Why a NEW finding (sibling of R10-API2)**: R10-API2 is filed
  against `sandbox-agent/src/handlers.rs:568`'s `ExecBody` — same
  struct name, same shape, same anti-pattern, different crate. The
  agent's `ExecBody` has been carried for 3 rounds as actionable
  (recommendation: `pub` → `pub(crate)`). The sandbox crate's version
  shares the bug **and** the same recommendation. The two should be
  fixed together for symmetry, and the r10/r11/r12 carry-forwards
  never named the sandbox-crate site.
- **Evidence of orphanage**:
  - `grep -rn 'zeroship_sandbox::handlers::ExecBody\|sandbox::handlers::ExecBody' crates/ tests/`
    → **zero** matches.
  - `grep -rn 'use.*handlers::ExecBody\|use.*handlers::\*' crates/`
    → **zero** matches (no glob imports either).
  - The only consumer is the local `exec` fn in the same file.
- **Action**: `pub struct ExecBody` → `pub(crate) struct ExecBody`,
  fields stay `pub` (intra-struct visibility). One-token edit.
  Recommended: bundle with R10-API2 as a single 2-site PR ("`ExecBody`
  demote both crates").
- **Severity**: MINOR. Wire-shape is unaffected; this is purely a
  visibility tightening that signals "this is an internal request
  body, not a public API." Same severity as R10-API2.
- **Cluster**: pair with R10-API2 in the deferred backlog under a
  single "`ExecBody` demote (sandbox + sandbox-agent)" entry.

## Carry-forward (still open at HEAD `e887b8ee`)

Reference lines re-verified against HEAD.

- **R10-API1** — `_test_build_auth_from_sealed` orphan `pub` at
  `restore.rs:613` (4th-round carry). Verbatim. `grep -rn
  '_test_build_auth_from_sealed' crates/ tests/` returns the definition
  site only. Cluster with R11-API1.
- **R10-API2** — `ExecBody` + `not_found` over-pub in sandbox-agent
  (4th-round carry). Verbatim at `handlers.rs:264` (`not_found`,
  bin/lib-split-justified — close that half by docstring) and
  `handlers.rs:568` (`ExecBody`, demote to `pub(crate)`). **Now
  paired with R13-API1** as the sandbox-crate sibling.
- **R10-API3** — `persist.rs` 3 demotable `pub fn`s (`seal`,
  `unseal_dir`, `seal_filename_for_str`); evidence stable across
  r10/r11/r12/r13. **4th-round carry.** Re-verified at HEAD: only
  `seal_filename_for` has external callers (2 sites in
  `sandbox_preview_share_e2e.rs` + 1 site in `backend/nomad_ch.rs` +
  1 docstring reference in `admin_handlers.rs`). The other 3 are
  zero-caller `pub`. 3-token edit.
- **R10-API4** — readyz non-§10.0 wire shape in sandbox-AGENT
  (4th-round carry). Verbatim at `handlers.rs:498-510`. Two non-§10.0
  503 paths (draining + reaper-down). Clustered with R12-API1
  (sibling site in sandbox crate).
- **R10-API5 / sig.rs:120** — stale hyphenated UUID example.
  **5th-round carry.** Verbatim:
  `/// Sandbox UUID (string form, e.g. \`019486f5-…\`).`
- **R10-API6 / db.rs** — stale "hyphenated form" test comment.
  **5th-round carry.** Line reference moved again: now at
  **`db.rs:2917`** (was 2839 at r10/r11, 2859 at r12). The
  `validate_ha_env_vars` test block growth pushed the comment further
  down this round. Comment itself unchanged.
- **R11-API1** — two `#[doc(hidden)] pub fn` orphans in `metrics.rs`
  (`takeover_corrupt_value`, `sandbox_corrupt_id_value`). **3rd-round
  carry.** Zero callers confirmed at HEAD.
- **R12-API1** — sandbox crate's `readyz` non-§10.0 wire shape at
  `handlers.rs:132-139`. **2nd-round carry.** Verbatim.
- **R12-API2** — R10-API6 line reference doc-bookkeeping nudge. Line
  moved again this round (2859 → 2917) — see R10-API6 above. **2nd-round
  carry, doubly stale.**
- **R13-API1** (NEW) — sandbox crate's `ExecBody` over-pub.

## Closed by recent commits (api-surface scope)

None this round. R10-Q5 closed at `0cc7af52` but was a code-quality
finding; verified here only to confirm no other dead-fn ghosts of the
same shape remain in sandbox-agent (none found).

## Recommended fix order

If a single api-surface PR is queued this cycle, the priority list is
**unchanged from r12** — none of the recommended items landed this
round. Refresh:

1. **R10-API3 (3 demotions)** — 3-token edit, evidence-stable for **4
   consecutive rounds**, zero risk. The rate at which this is *not*
   landing is itself the signal worth flagging up the chain.
2. **R10-API1 + R11-API1 (orphan test-only `pub fn`s)** — bundle as
   one commit, demote to `pub(crate)` + `#[cfg(test)]` or delete.
   Choose-one micro-PR. 3-symbol surface.
3. **R10-API5 + R10-API6** — two doc-fix edits, ~60 chars total.
   Cheapest in the backlog. **5th-round carries.** Free to land
   alongside any other PR.
4. **R10-API2 + R13-API1 (`ExecBody` demote, 2 crates)** — 2-token
   edit. Bundle for symmetry.
5. **R10-API4 + R12-API1 (readyz §10.0 envelope drift, 2 sites)** —
   carve-out + comments OR convert both to envelope. Decide carve-out
   policy first; the implementation is mechanical.

## Two most-critical citations

1. **`crates/sandbox-agent/src/sig.rs:120`** — `Sandbox UUID (string
   form, e.g. \`019486f5-…\`)`. Hyphenated example contradicts
   `restore_handler.rs`'s `.simple()` wire contract; a reader
   following the docstring will 401 every resync. **5th-round carry.**
   30-char edit. Persistent-doc bug with the lowest fix cost in the
   entire api-surface backlog. Continues to be the *only* critical
   citation each round.

2. **`crates/sandbox/src/persist.rs` 3-demotion edit** — `seal`,
   `unseal_dir`, `seal_filename_for_str` confirmed for the **fourth
   consecutive round** to have zero external callers. The bypass risk
   (`Persistence::seal_all`'s audit + `spawn_blocking` discipline) has
   been documented since r10; the evidence has been stable since r10.
   **Failing to land a 3-token demotion across 4 review rounds is
   itself the signal** — this is the cheapest open api-surface defect,
   and the rate at which it's *not* being closed is the only
   non-trivial datum left in this backlog.

## Trend

- **`Result<_, String>`** (strict regex `Result<[^,>]+,\s*String\s*>`,
  measured via `find … -name '*.rs' -print0 | xargs -0 grep -cE … | awk
  '{sum+=$NF}'` for stable per-file counts):
  - sandbox **161** at `ae946cba`, **161** at `e887b8ee` — **flat
    r12→r13**. Note: r12 reported 144, but that figure cannot be
    reproduced against `ae946cba` with the documented regex; the
    re-measurement yields 161. Suspect a different glob scope or
    grep-mode in r12. Going forward, the find-based count is the
    canonical method.
  - sandbox-agent **14** at both — flat, at floor.
- **`pub`-token count** (`^[[:space:]]*pub[[:space:](]`, includes
  indented `pub(crate)` items):
  - r12: 841
  - **r13: 841** — exactly flat.
  - Top-level `pub` only: **329** at r13.
  All r12→r13 commits are either documentation, cluster scripts,
  config/pin bumps, or in-fn body changes — none touch the `pub`
  surface.
- **`RestoreBackend` trait method count**:
  - r12: 8
  - **r13: 8** — flat. R12-I1 threaded `TaskDriverMode` through a
    private fn, not through the trait. Correct architectural choice
    (the trait stays storage-agnostic; jobspec-mode is a backend
    implementation detail behind `submit_restore_job`).
- **Net new wire endpoints** (r12→r13): **0**. All recent commits are
  docs (r12 reviewer artifacts), cluster smoke scripts/results
  (T-8b-prereqs-config, T-8b-smoke, T-8b-smoke-retry, T-8b-smoke-retry-r3),
  driver/controller pin bumps, dead-fn delete (R10-Q5), uid-check
  hardening (R11-S2), TODO refresh (R12-Q1), capability presence pins
  (R11-T3), and R12-I1's wake-path mode threading. No new HTTP routes,
  no new request shapes, no new error envelope fields.
