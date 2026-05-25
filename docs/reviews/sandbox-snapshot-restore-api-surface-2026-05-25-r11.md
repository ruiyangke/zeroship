# Sandbox/snapshot-restore — api-surface r11 review

Date: 2026-05-25 (UTC)
HEAD at audit: `d2cfcb34`
Round 11 of N.
Prior: r10 at `9678a840` (`docs/reviews/sandbox-snapshot-restore-api-surface-2026-05-25-r10.md`).

Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`. Read-only.

## Summary

2 new findings (0 critical, 0 important, 2 minor). 1 closure to record
(R10-Q1 / handlers.rs raw `{e}` leak — closes a 7-round carry via
`228569d3`). R10-API3 partially **invalidated** by evidence: of the 5
`persist::*` `pub fn`s flagged, 2 (`unseal_one`, `seal_filename_for`)
are consumed by `crates/sandbox/tests/sandbox_preview_share_e2e.rs`
across the integration-test crate boundary — `pub` is required, not a
defect. The remaining 3 (`seal`, `unseal_dir`, `seal_filename_for_str`)
have no external callers and can be safely demoted.

`pub `-token count (includes `pub(crate)`): 289 total, identical to r10
(`git diff 9678a840 d2cfcb34 -- crates/sandbox{,-agent}/src/ | grep '^+pub '`
returns empty). **Net new `pub` surface r10→r11: zero.** The only commits
in range either modify visibility (`228569d3` bumps `err_safe` from
private to `pub(crate)` — surface narrowing relative to a hypothetical
full `pub`) or add no new surface (`e4e5db60` adds an owner-uid check
inside the existing `AeadKey::from_path` body, no signature change;
`040812f2` is doc-only; `d2cfcb34` is doc-only).

`Result<_, String>` (strict regex `Result<[^,>]+,\s*String\s*>`): 161 in
sandbox (was 178 in r10's looser count; comparable strict-form delta is
-N), 14 in sandbox-agent (was 16). Trend: flat-to-down in the agent,
small further reduction in sandbox. The agent crate is at floor.

## New `pub` items since r10 (audit)

Hash range `9678a840..d2cfcb34`. Diff probe:

```
$ git diff 9678a840 d2cfcb34 -- crates/sandbox/src/ crates/sandbox-agent/src/ | grep '^+pub '
(empty)
```

| Commit | Item | Path | Visibility | Justified? |
|---|---|---|---|---|
| `e4e5db60` | (none — owner-uid check added inside existing `AeadKey::from_path` body) | `crates/sandbox/src/persist.rs:333` | unchanged `pub fn` | n/a (signature unchanged; 2 new tests added inside `#[cfg(test)] mod tests`) |
| `228569d3` | `err_safe` visibility bump | `crates/sandbox/src/admin_handlers.rs:228` | `fn` → `pub(crate) fn` | YES — `handlers.rs:671/822/838` now imports `crate::admin_handlers::err_safe;` (line 16) and calls it from 3 sites. Narrowest possible visibility for cross-module use; not `pub`. |
| `040812f2`, `5f030871`, `d2cfcb34` | (none) | docs only | n/a | n/a |

**Net new `pub` items: 0. Net new `pub(crate)` items: 1 (`err_safe`).
Both intentional and narrowly scoped.** Clean delta from r10.

## R-carryover status

- **R10-Q1** (handlers.rs:670/821/837 raw `{e}` leak; 7-round carry):
  **CLOSED at `228569d3`**. Verified at `handlers.rs:671/822/838` — all
  three sites now read `err_safe(500, "backend_*_failed", "backend …
  failed", e)`. `err_safe` is `pub(crate)` at `admin_handlers.rs:228`
  (the chosen visibility for cross-module use within the sandbox
  crate). Three new wire-shape regression tests pinned at
  `handlers.rs:1286-1360` feed sentinel-bearing raw errors through
  `err_safe` and assert the wire body contains the fixed public message
  but NOT the sentinels. `grep 'err\(500.*\{e\}' crates/sandbox/src/*.rs`
  returns empty per the commit message — verified.

- **R10-API1** (`_test_build_auth_from_sealed` orphan `pub`):
  **STILL OPEN.** Verbatim at `restore.rs:613`. `grep -rn
  '_test_build_auth_from_sealed' crates/ tests/` returns the definition
  site only; zero callers. Compare with sibling test-only accessors
  surfaced this round (R11-API1 below).

- **R10-API2** (`ExecBody` + `not_found` over-pub):
  **STILL OPEN.** Verbatim at `handlers.rs:264` (`pub fn not_found`)
  and `handlers.rs:568` (`pub struct ExecBody`). No new evidence
  changes the r10 recommendation.

- **R10-API3** (`persist.rs` 5 module-level `pub fn`s):
  **PARTIALLY INVALIDATED.** Cross-checked against
  `crates/sandbox/tests/sandbox_preview_share_e2e.rs`:
  - **`unseal_one`** — CONSUMED externally at `e2e.rs:1034,1079,1114`
    (`use zeroship_sandbox::persist::{unseal_one, AeadKey}`). MUST stay
    `pub`. Not a defect.
  - **`seal_filename_for`** — CONSUMED externally at `e2e.rs:1058,1112`
    (`zeroship_sandbox::persist::seal_filename_for(id)`). MUST stay
    `pub`. Not a defect.
  - **`AeadKey::from_bytes` / `AeadKey`** — CONSUMED at `e2e.rs:151,1064,1114`
    via `zeroship_sandbox::persist::AeadKey::from_bytes`. MUST stay
    `pub`.
  - **`seal`** — NO external caller (`grep persist::seal\b` outside
    `persist.rs` yields only `admin_handlers.rs:1036`'s docstring
    reference and `nomad_ch.rs:4593` which calls `seal_filename_for`,
    not `seal`). Safe to demote to `pub(crate) fn`. 1-token edit.
  - **`unseal_dir`** — NO external caller. Safe to demote to
    `pub(crate) fn`.
  - **`seal_filename_for_str`** — NO external caller. Safe to demote
    to `pub(crate) fn`.
  - **Net**: 3 of 5 are demotable (the `seal` write path + bulk
    `unseal_dir` + the `_str` overload), 2 must stay `pub` because the
    integration test binds against them. **The r10 "demote all 5"
    recommendation is wrong; the right ask is "demote 3 of 5".** Same
    bypass-risk argument applies to the 3 (they bypass the
    `Persistence` handle's audit + spawn_blocking discipline). 3-token
    edit.

- **R10-API4** (readyz `{"status":"draining"}` non-§10.0 envelope):
  **STILL OPEN.** Verbatim at `handlers.rs:498-510`. No new context
  changes the r10 carve-out-+-comment recommendation.

- **R10-API5 / sig.rs:120** (stale `019486f5-…` hyphenated UUID
  example in `ResyncBody` doc): **STILL OPEN, 3rd-round carry.**
  Verbatim at `sig.rs:120`:
  `/// Sandbox UUID (string form, e.g. \`019486f5-…\`). Agent rejects`
  Controller signs `.simple()` (no hyphens) at `restore_handler.rs:1601`
  via `sandbox_id.simple().to_string()`. The docstring's hyphenated
  example contradicts the wire contract; an implementer following the
  doc 401s every resync. 30-char edit.

- **R10-API6 / db.rs:2839** (stale "hyphenated form" test comment):
  **STILL OPEN, 3rd-round carry.** Verbatim at `db.rs:2839`:
  `// The file itself contains the UUID (hyphenated form).`
  Test is about `host_id` (which is intentionally hyphenated, unlike
  `sandbox_id` which uses `.simple()`). Same r9/r10 recommendation:
  append one clarifying sentence about host_id ≠ sandbox_id.

- **R7-API2** (`clock.resync-v1` capability advertised but unread by
  controller): **STILL OPEN, 5th-round carry.** `version.rs:54`
  advertises `clock.resync-v1` with a docstring at line 49-53 that
  *claims* "The controller feature-detects via this string so older
  agents (no resync endpoint) gracefully fall back to the pre-fix
  path." That claim is **false-by-code**:
  `grep -rn 'has_capability\|capabilities' crates/sandbox/src/` returns
  zero hits; `clock_resync_post_restore` at `restore_handler.rs:1582-1666`
  issues the resync UNCONDITIONALLY (no capability gate, no precondition
  check against the `/version` response). The advertised capability is
  decoration that documents a fall-back path that does not exist in code.
  Either gate the call site or remove the advertisement + the now-stale
  comment claim. **The docstring claiming feature-detect support
  while the code does no feature-detect is the more serious sub-finding
  this round** — code-vs-doc drift compounds the wire-surface defect.

## NEW findings (post-r10)

### [R11-API1] Two `#[doc(hidden)] pub fn` test-only accessors with zero callers in `metrics.rs` (MINOR, api-surface-r11)

- **Files**:
  - `crates/sandbox/src/metrics.rs:223` — `pub fn takeover_corrupt_value() -> u64`
  - `crates/sandbox/src/metrics.rs:229` — `pub fn sandbox_corrupt_id_value() -> u64`
- **Symptom**: Both are `#[doc(hidden)] pub fn` accessors with the
  docstring "Test-only accessor for the … counter."
  `grep -rn 'takeover_corrupt_value\|sandbox_corrupt_id_value' crates/`
  returns the definition sites only — **zero call sites** in the
  workspace (no production callers, no in-crate tests, no integration
  tests in `crates/sandbox/tests/*`, no doctests). Same flavor as
  R10-API1's `_test_build_auth_from_sealed`: scaffold added
  speculatively, never wired. Live in release builds despite the
  `#[doc(hidden)]`.
- **Action**: Either delete (preferred — three orphan accessors of the
  same shape across `restore.rs` + `metrics.rs` is a small but
  cumulative drift) or downgrade to `pub(crate)` + `#[cfg(test)]` if a
  future test is genuinely planned. Note that the metrics module
  already exposes `inc_takeover_*` and `inc_sandbox_corrupt_id` setters
  used in production — the accessors would only fire under a future
  test scenario that hasn't materialised. Cluster these with R10-API1
  in the deferred backlog under "orphan test-only `pub fn`s".

### [R11-API2] R7-API2 docstring now actively misleads (MINOR, api-surface-r11)

- **File**: `crates/sandbox-agent/src/version.rs:49-54`
- **Symptom**: The capability list docstring asserts
  ```rust
  // controller feature-detects via this string so older agents
  // (no resync endpoint) gracefully fall back to the pre-fix path.
  ```
  but the controller (`restore_handler.rs:1582`'s
  `clock_resync_post_restore`) issues the resync unconditionally with
  no `/version` precondition check. **No callsite reads the
  `capabilities` array** from the agent's version response anywhere in
  `crates/sandbox/src/`. The docstring is now self-falsifying — it
  documents behaviour that does not exist. R7-API2 has been carried
  for 5 rounds, but the docstring claim raises the severity from "dead
  surface" to "actively misleading" (a future contributor reading the
  doc will assume the gate exists, then debug a phantom).
- **Action**: Pair with the R7-API2 resolution. Two coupled options
  carry over from r10:
  - **Option A (preferred)**: implement the feature-detect at
    `restore_handler.rs:1582` by branching on
    `version_resp.capabilities.contains("clock.resync-v1")` before
    calling the resync. The docstring then becomes accurate.
  - **Option B**: remove `"clock.resync-v1"` from the advertised
    capability list AND strip the misleading comment at
    `version.rs:49-53`. The agent serves `/_clock_resync` regardless;
    the controller already calls unconditionally. Net behaviour
    unchanged, but the docstring stops lying.
  - The status quo (capability advertised + comment claiming
    feature-detect + no feature-detect in code) is the worst of all
    three states. **5-round carry** is long enough — pick A or B this
    round.

## Closed by recent commits

- **handlers.rs:670/821/837 raw `{e}` leak** (R10-Q1, 7-round carry) —
  CLOSED at `228569d3`. `err` → `err_safe` substitution at three sites;
  `err_safe` bumped to `pub(crate)`. 3 new wire-shape regression tests.
  Operators recover raw via `tracing::error!`. Verified at
  `handlers.rs:671/822/838`.

## Carry-forward (still open at HEAD `d2cfcb34`)

- **R10-API1** — `_test_build_auth_from_sealed` orphan `pub` at
  `restore.rs:613` (2nd-round carry, now clustered with R11-API1).
- **R10-API2** — `ExecBody` (`handlers.rs:568`) + `not_found`
  (`handlers.rs:264`) over-pub'd in sandbox-agent (2nd-round carry).
- **R10-API3** — `persist.rs` `pub fn`s **revised to 3 demotable**
  (`seal`, `unseal_dir`, `seal_filename_for_str`); the other 2
  flagged in r10 (`unseal_one`, `seal_filename_for`) are externally
  consumed by `sandbox_preview_share_e2e.rs` and must stay `pub`.
  3-token fix.
- **R10-API4** — readyz non-§10.0 wire shape (carve-out + comment
  recommended). 2nd-round carry.
- **R10-API5 / sig.rs:120** — stale hyphenated UUID example.
  **3rd-round carry.**
- **R10-API6 / db.rs:2839** — stale "hyphenated form" comment.
  **3rd-round carry.**
- **R7-API2** — `clock.resync-v1` advertised but no controller
  feature-detect. **5th-round carry**, now compounded by misleading
  docstring (see R11-API2).
- **R10-API7** — error-envelope helper-surface drift between
  sandbox and sandbox-agent (informational, no action this round).
- **r9 test_set_sandbox_id `pub` under `#[cfg(test)]`** (style-only,
  no fix needed per r9 ruling).

## Trend metric

- `Result<_, String>` (strict regex `Result<[^,>]+,\s*String\s*>`):
  sandbox **161** (r10 reported 178 with a looser regex; comparable
  strict-form trend is down), sandbox-agent **14** (was 16, -2).
  Agent crate is at floor. Sandbox crate still has the largest
  remaining absolute count; concentrated in `backend/nomad_ch.rs`
  (39), `backend/k8s.rs` (33), `backend/mod.rs` (18),
  `backend/docker.rs` (20).
- `pub `-token count (includes `pub(crate)`): 289 total at both
  `9678a840` and `d2cfcb34` — **flat r10→r11**.
- Net new `pub` surface r10→r11: **0** (per `git diff … | grep '^+pub '`
  returning empty).
- Net new `pub(crate)` surface r10→r11: **+1** (`err_safe` bumped from
  private to `pub(crate)` to share the sanitizer across `admin_handlers`
  + `handlers` — narrowest visibility for cross-module use; not a
  surface expansion).

## Two most-critical citations

1. **`crates/sandbox-agent/src/sig.rs:120`** — `Sandbox UUID (string
   form, e.g. \`019486f5-…\`)`. Hyphenated example contradicts
   `restore_handler.rs:1601`'s `.simple()` wire contract; a reader
   following the docstring will 401 every resync. **3rd-round carry.**
   30-char edit. The persistent-doc bug with the lowest fix cost in
   the entire api-surface backlog.

2. **`crates/sandbox-agent/src/version.rs:49-54`** (R11-API2 +
   R7-API2 compound) — capability `clock.resync-v1` advertised with
   docstring claiming controller feature-detect, BUT
   `restore_handler.rs:1582`'s `clock_resync_post_restore` issues the
   call unconditionally and `grep capabilities crates/sandbox/src/`
   has zero hits. **5th-round carry, now compounded by a self-
   falsifying docstring.** Pick option A (implement the gate) or
   option B (remove the advertisement + the misleading comment) this
   round.
