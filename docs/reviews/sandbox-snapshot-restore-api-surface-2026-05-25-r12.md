# Sandbox/snapshot-restore — api-surface r12 review

Date: 2026-05-25 (UTC)
HEAD at audit: `ae946cba`
Round 12 of N.
Prior: r11 at `d2cfcb34` (`docs/reviews/sandbox-snapshot-restore-api-surface-2026-05-25-r11.md`).

Scope: `crates/sandbox/**`, `crates/sandbox-agent/**`. Read-only.

## Summary

- **2 new findings** (0 critical, 0 important, 2 minor). Both surface a
  previously-unaudited shape in the sandbox crate (not regressions —
  pre-existing code that the r10-r11 sweep didn't touch).
- **1 closure to record**: R7-API2 / R11-API2 (the `clock.resync-v1`
  feature-detect docstring) is **CLOSED at `c8000537`** — the
  docstring was rewritten on 2026-05-23 to document the capability
  list as a per-entry diagnostic manifest with explicit "mandatory
  (not feature-detected)" wording for `clock.resync-v1`, plus a
  presence-regression guard at `version.rs:tests::mandatory_clock_resync_v1_present`.
  The "self-falsifying docstring" sub-finding from r11 is gone.
  Verified verbatim at `version.rs:38-91`. **5-round carry retired.**
- **Net new `pub` surface r11→r12: zero.** `git diff d2cfcb34 ae946cba
  -- crates/sandbox{,-agent}/src/ | grep '^+pub '` returns empty.
- **Net new `pub(crate)` surface r11→r12: +3** (`TaskDriverMode` enum
  + `task_driver_mode_from_env` fn + `build_nomad_job_json_with` fn,
  all from T-7 at `5fe36805`). All correctly visibility-scoped — they
  are in-crate-only and consumed only by the sibling
  `build_nomad_job_json` and 7 new in-module tests. No external caller
  exists or could appear.
- **`RestoreBackend` trait** sits at **8 methods** (was 7 at r10),
  +1 from `register_restored` which landed pre-r10 at the B19 fix. The
  R10-C1 + R10-C2 closing commit `be246395` did NOT add a trait method
  — `unregister_restored` is a `pub(crate)` helper on
  `NomadCHBackend`, called only from the impl on `RealRestoreBackend`
  inside `restore_handler.rs`. **No trait drift this round.**
- **Result<_, String>** trend: sandbox **144** (was 161, **-17**),
  sandbox-agent **14** (flat). Sandbox crate continues its downward
  trend; agent at floor.
- **`pub`-token count** (`^[[:space:]]*pub[[:space:](]`, includes
  indented `pub(crate)` items): **841** at `ae946cba` vs **838** at
  `d2cfcb34` — **+3, fully accounted for by T-7's `pub(crate)`
  additions.** No surface accretion.

## New `pub` / `pub(crate)` items from recent commits

Hash range `d2cfcb34..ae946cba` (the audit point — 2 docs/test
commits past `ae946cba` exist on the branch but are out of scope per
the prompt).

| Commit | Item | Path | Vis | Justified? |
|---|---|---|---|---|
| `c8000537` | (none — doc + 1 test) | `crates/sandbox-agent/src/version.rs` | n/a (docstring rewrite + 1 `#[test]`) | n/a — closes R7-API2 / R11-API2 |
| `e4e5db60` (pre-r11) | (none — owner-uid check inside existing body) | n/a | n/a | already audited at r11 |
| `5fe36805` (T-7) | `TaskDriverMode` enum | `crates/sandbox/src/backend/nomad_ch.rs:2230` | `pub(crate)` | YES — in-crate only; consumed by the sibling `build_nomad_job_json` and 7 in-module tests. Cannot be private because the test module pinning the typed Config shape lives below the function. Not `pub`. |
| `5fe36805` (T-7) | `task_driver_mode_from_env` fn | `crates/sandbox/src/backend/nomad_ch.rs:2239` | `pub(crate)` | YES — same scope. Consumed at line 2301 (the env-reading wrapper) and pinned by test `nomad_job_spec_uses_raw_exec_by_default` + `nomad_job_spec_uses_ch_when_flag_set`. |
| `5fe36805` (T-7) | `build_nomad_job_json_with` fn | `crates/sandbox/src/backend/nomad_ch.rs:2311` | `pub(crate)` | YES — typed-surface variant of the public-to-crate `build_nomad_job_json`, called from the existing wrapper at line 2290 and 7 tests at lines 3854/3952/3986/4042/4153/4221/4248/4281/4313. **Lets tests pin `TaskDriverMode + restore_from` without `std::env::set_var` races.** |
| `b4c3ef27` (R9-S4d) | (none — owner-uid check inside existing `load_admin_token` body + 2 tests) | `crates/sandbox/src/lib.rs:944-960` | unchanged `pub(crate)` | n/a — visibility unchanged, signature unchanged, the only addition is body code + tests |
| `ae946cba` (T-8a) | (none — shell script + docs only) | `crates/sandbox/scripts/gcp-worker-startup.sh` | n/a | n/a |

**Verdict**: 3 `pub(crate)` additions, all narrowly scoped to enable
T-7's typed-config jobspec path + a test surface that doesn't race
the env-reading wrapper. None should be `pub` (no out-of-crate
caller) and none could be private (the in-module test fns need
visibility). Correct visibility per zeroship's "narrowest scope that
works" rule.

## R-carryover status

- **R7-API2** (`clock.resync-v1` advertised but unread by controller):
  **CLOSED at `c8000537`**. The 5-round carry is retired. The fix is
  *not* implementing controller-side feature-detection (option A from
  r10/r11) but documenting the mandatory-not-negotiated semantic per
  capability entry (option B-prime — keep the advertisement, fix the
  docstring + add a regression guard). The new wording at
  `version.rs:38-91` is now self-consistent:
  - Module-level doc (lines 3-19) says "per-capability semantic is
    NOT uniform" + "treat CAPABILITIES as a versioned implementation
    manifest, not a universal negotiation surface".
  - Const-level doc (lines 41-58) reiterates "negotiation semantic
    — per entry, not uniform" with the rationale.
  - Per-entry comment on `clock.resync-v1` (lines 75-91) explicitly
    says "Mandatory (not feature-detected)" with a "future wiring"
    pointer.
  - Test `mandatory_clock_resync_v1_present` (lines 174-191)
    asserts the cap stays in the list; commit `eb26db31` (1 ahead of
    audit point) extended this pattern to `proxy.ws-v1` +
    `auth.ed25519-v1.1` for symmetry (R11-T3 closure).
  The r11 R11-API2 sub-finding ("self-falsifying docstring") is
  resolved by the same edit. Both **CLOSE** as a single bundle.

- **R10-API1** (`_test_build_auth_from_sealed` orphan `pub` at
  `restore.rs:613`): **STILL OPEN.** Verbatim — 3rd-round carry.
  `grep -rn '_test_build_auth_from_sealed' crates/ tests/` returns
  the definition site only.

- **R10-API2** (`ExecBody` + `not_found` over-pub in sandbox-agent):
  **STILL OPEN.** Verbatim at `handlers.rs:264`
  (`pub fn not_found`) and `handlers.rs:568` (`pub struct ExecBody`).
  3rd-round carry. R10's recommendation stands: `not_found` is
  consumed by `main.rs::default_service` (bin/lib split forces
  `pub` — actually justified, this part can be closed by
  documentation). `ExecBody` has no out-of-crate caller and could be
  `pub(crate)` — actionable.

- **R10-API3** (`persist.rs` `pub fn`s — revised in r11 to 3
  demotable): **STILL OPEN, no callers added since r11.** Verified at
  HEAD via `grep -rn 'zeroship_sandbox::persist::(seal\b|unseal_dir|seal_filename_for_str)' .` — zero matches.
  - `seal` — no external caller
  - `unseal_dir` — no external caller
  - `seal_filename_for_str` — no external caller
  - (`unseal_one` + `seal_filename_for` MUST stay `pub` — consumed by
    `sandbox_preview_share_e2e.rs`)
  3-token edit ready to land in a single commit. 3rd-round carry; the
  evidence has been stable across r10/r11/r12, which is the signal to
  ship.

- **R10-API4** (readyz `{"status":"draining"}` non-§10.0 envelope in
  sandbox-AGENT): **STILL OPEN.** Verbatim at
  `crates/sandbox-agent/src/handlers.rs:498-510` — 3rd-round carry.
  See also **R12-API1 below** (sibling case in the sandbox crate
  itself, not previously surfaced).

- **R10-API5 / sig.rs:120** (stale hyphenated UUID example in
  `ResyncBody` doc): **STILL OPEN, 4th-round carry.** Verbatim at
  `sig.rs:120`:
  `/// Sandbox UUID (string form, e.g. \`019486f5-…\`).`
  Controller signs `.simple()` at `restore_handler.rs` (~1601 in r11;
  unchanged this round). The persistent-doc bug with the lowest fix
  cost in the entire api-surface backlog. 30-char edit.

- **R10-API6 / db.rs:2839** (stale "hyphenated form" test comment):
  **STILL OPEN, 4th-round carry.** The line moved to **`db.rs:2859`**
  this round (file grew by 20 lines from intervening commits;
  comment is verbatim "The file itself contains the UUID (hyphenated
  form).") The test still concerns `host_id`, which is intentionally
  hyphenated unlike `sandbox_id`. **Reference line bumped — update
  the deferred backlog entry.**

- **R11-API1** (two `#[doc(hidden)] pub fn` test accessors in
  `metrics.rs`): **STILL OPEN.** Both verbatim at
  `metrics.rs:223,229` (`takeover_corrupt_value`, `sandbox_corrupt_id_value`).
  Zero callers anywhere — `grep -rn 'takeover_corrupt_value\|sandbox_corrupt_id_value' crates/`
  returns the definition sites only. 2nd-round carry. Cluster with
  R10-API1 in the deferred backlog under "orphan test-only `pub fn`s".

- **R10-API7** — error-envelope helper-surface drift between sandbox
  and sandbox-agent (informational, no action this round). The
  agent's `error_envelope.rs:34-42` explicitly says "we do not lift
  the envelope into `zeroship-core` yet … if a third caller appears,
  fold the two into `zeroship-core` then" — this is documented
  intentional duplication, not drift. Retire R10-API7 as
  "non-actionable, by design."

## NEW findings (post-r11)

### [R12-API1] Sandbox crate's `readyz` emits non-§10.0 wire shape on 503 — sibling of R10-API4 (MINOR, api-surface-r12)

- **File**: `crates/sandbox/src/handlers.rs:132-139`
- **Symptom**: The sandbox (controller) crate's `readyz` mirrors the
  agent's anti-pattern that R10-API4 has been tracking:
  ```rust
  pub async fn readyz(state: State) -> HttpResponse {
      if state.backend.is_healthy() {
          HttpResponse::Ok().json(&serde_json::json!({"status": "ready"}))
      } else {
          HttpResponse::ServiceUnavailable()
              .json(&serde_json::json!({"status": "backend-unhealthy"}))
      }
  }
  ```
  503 wire body is `{"status":"backend-unhealthy"}` — not the §10.0
  `{"error":"…","message":"…"}` envelope. Both arms bypass
  `error_envelope::error_response`. The sandbox crate has its own
  envelope helper at `crates/sandbox/src/error_envelope.rs` parallel
  to the agent's; the rest of the controller's HTTP surface funnels
  through it.
- **Why a NEW finding**: R10-API4 was filed against the agent's
  `readyz`. The sandbox crate's `readyz` is a separate handler in a
  separate crate, not previously audited under this banner; r10's
  recommendation (carve-out + comment) applies symmetrically but the
  carve-out hasn't been written into either site, and the sandbox
  crate's parallel envelope helper means a future "fold the two
  envelopes into `zeroship-core`" effort needs both sites pinned.
- **Evidence of orphanage**: `grep -nE 'HttpResponse::(BadRequest|NotFound|InternalServerError|Conflict|ServiceUnavailable|Forbidden|Unauthorized)' crates/sandbox/src/`
  returns exactly **one** hit — `handlers.rs:136`. Every other 4xx/5xx
  in the sandbox crate funnels through `error_envelope`. Same lone-
  exception shape as the agent's `readyz`. Two-site duplication of
  the same anti-pattern is the signal worth surfacing as a fresh
  finding.
- **Action**: One of:
  - **Strict §10.0**: emit `{"error":"backend_unhealthy","message":"backend unhealthy"}`
    via `error_envelope::error_response(StatusCode::SERVICE_UNAVAILABLE, "backend_unhealthy", "backend unhealthy")`.
    Aligns both `readyz` sites with the rest of their respective
    crates.
  - **Probe-semantics carve-out**: keep the current body, add a 3-
    line comment at both sites (sandbox + agent) noting that
    Kubernetes/CHWBL probes are the documented exception and the
    operator-debug body shape is more readable as
    `{"status":"backend-unhealthy"}`. Then encode the carve-out in
    `error_envelope.rs`'s module doc so a future fold-into-core
    effort knows to preserve it.
- **Severity**: MINOR. Probe handlers are typically consumed by
  orchestrators that only check the HTTP status, not the body, so
  this is wire-shape hygiene rather than a contract violation.
- **Cluster**: pair with R10-API4 in the deferred backlog under a
  single "readyz §10.0 envelope drift (2 sites)" entry. Either close
  both together or carve out both together.

### [R12-API2] `db.rs:2839 → db.rs:2859` carry-forward reference bumped this round (TRIVIAL, doc-bookkeeping)

- **File**: `crates/sandbox/src/db.rs:2859` (was 2839 at r11)
- **Symptom**: Not a new bug — the stale "hyphenated form" comment
  R10-API6 has been tracking moved 20 lines down from `2839` to
  `2859`. The file grew by 20 lines between `9678a840` (r10) and
  `ae946cba` (r12) for unrelated changes. The comment itself is
  unchanged: `// The file itself contains the UUID (hyphenated form).`
- **Why surface it**: The deferred backlog entry for R10-API6 and the
  r10/r11 review docs all cite line 2839, which will now point a
  reader to a `validate_dsn_scheme` test (an unrelated body). A future
  agent re-running the audit will lose 30 seconds reconciling. This
  is a doc-bookkeeping nudge, not a code finding.
- **Action**: When the R10-API6 fix lands, update the deferred
  backlog reference from `db.rs:2839` to `db.rs:2859`. Trivial.
- **Severity**: TRIVIAL — not a code defect, a stale-cite warning.

## Closed by recent commits

- **R7-API2** (`clock.resync-v1` capability advertised but unread by
  controller, 5-round carry) — **CLOSED at `c8000537`**. The fix
  documents the per-capability semantic (mandatory vs feature-detected)
  rather than implementing a feature-detect branch the deployment
  story doesn't need; a presence-regression guard ensures the cap
  can't silently disappear without simultaneously updating the
  controller's resync call site. Commit `eb26db31` (1 past audit)
  extends the same pin pattern to `proxy.ws-v1` + `auth.ed25519-v1.1`
  for symmetry. R11-API2 (self-falsifying docstring) closes as part
  of the same bundle.

## Carry-forward (still open at HEAD `ae946cba`)

- **R10-API1** — `_test_build_auth_from_sealed` orphan `pub` at
  `restore.rs:613` (3rd-round carry, clustered with R11-API1).
- **R10-API2** — `ExecBody` + `not_found` over-pub in sandbox-agent
  (3rd-round carry). `not_found` is bin/lib-split-justified per
  R10-API0's pattern; `ExecBody` is the actionable half.
- **R10-API3** — `persist.rs` 3 demotable `pub fn`s (`seal`,
  `unseal_dir`, `seal_filename_for_str`); evidence stable across
  r10/r11/r12. 3-token edit.
- **R10-API4** — readyz non-§10.0 wire shape in sandbox-AGENT
  (3rd-round carry). Now clustered with R12-API1 (sibling site in
  sandbox crate).
- **R10-API5 / sig.rs:120** — stale hyphenated UUID example.
  **4th-round carry.**
- **R10-API6 / db.rs:2859** — stale "hyphenated form" comment.
  **4th-round carry.** Line reference bumped this round (see
  R12-API2).
- **R11-API1** — two `#[doc(hidden)] pub fn` orphans in `metrics.rs`
  (`takeover_corrupt_value`, `sandbox_corrupt_id_value`). 2nd-round
  carry.
- **R12-API1** (NEW) — sandbox crate's `readyz` non-§10.0 wire shape.
- **R12-API2** (NEW, trivial) — R10-API6 line reference moved.

## Recommended fix order

If a single api-surface PR is queued this cycle:

1. **R10-API3 (3 demotions)** — 3-token edit, evidence-stable for 3
   rounds, zero risk. Land first.
2. **R10-API1 + R11-API1 (orphan test-only `pub fn`s)** — bundle as
   one commit, either delete all 3 (no callers, no doctests) or
   demote to `pub(crate)` + `#[cfg(test)]`. Choose-one micro-PR.
3. **R10-API5 + R10-API6** — two doc-fix edits, ~60 chars total.
   Free to land alongside any other PR.
4. **R10-API4 + R12-API1 (readyz §10.0 envelope drift, 2 sites)** —
   carve-out + comments OR convert both to envelope. Decide carve-out
   policy first; the implementation is mechanical.
5. **R10-API2 (`ExecBody` only)** — `not_found` is justified by
   bin/lib split (close that half with a docstring); `ExecBody`
   demotion is `pub` → `pub(crate)`.

## Two most-critical citations

1. **`crates/sandbox-agent/src/sig.rs:120`** — `Sandbox UUID (string
   form, e.g. \`019486f5-…\`)`. Hyphenated example contradicts
   `restore_handler.rs`'s `.simple()` wire contract; a reader
   following the docstring will 401 every resync. **4th-round
   carry.** 30-char edit. The persistent-doc bug with the lowest
   fix cost in the entire api-surface backlog. **Now the *only*
   critical citation this round** — its r11 stable-mate (R11-API2
   self-falsifying capability docstring) has been closed at
   `c8000537`.

2. **`crates/sandbox/src/persist.rs` 3-demotion edit** — `seal`,
   `unseal_dir`, `seal_filename_for_str` confirmed for the third
   consecutive round to have zero external callers. The bypass risk
   (`Persistence::seal_all`'s audit + `spawn_blocking` discipline)
   has been documented since r10; the evidence has been stable since
   r10. **Failing to land a 3-token demotion across 3 review rounds
   is itself the signal** — this is the cheapest open api-surface
   defect, and the rate at which it's *not* being closed is the only
   non-trivial datum left in this backlog.

## Trend

- **`Result<_, String>`** (strict regex `Result<[^,>]+,\s*String\s*>`):
  - sandbox **144** (was 161 at r11; **-17**, ~10% drop). Migration
    momentum continues — concentrated reductions in `db.rs` (was 98,
    likely down on the `validate_dsn_scheme`-adjacent refactors) and
    `restore_handler.rs` (was 19 at r11, now 19 — flat).
    Re-measure on the file-by-file basis at r13.
  - sandbox-agent **14** (flat from r11). Agent crate at floor.
- **`pub `-token count** (indented `pub(crate)` included via
  `^[[:space:]]*pub[[:space:](]`):
  - r10 baseline: 289 (top-level only) / not reported (incl.
    indented `pub(crate)`)
  - r11: 289 top-level / **838** incl. indented (per re-baseline)
  - **r12: 289 top-level / 841 incl. indented (+3)**
  All +3 are T-7's `pub(crate)` additions — visibility-correct, not
  surface accretion. Top-level `pub` count is **flat from r10
  through r12**.
- **`RestoreBackend` trait method count**:
  - r10: 7 (no `register_restored` default impl mentioned at the
    time; B19 fix landed inside r10's window)
  - r11: 8 (`register_restored` default-impl method present at
    `restore_handler.rs:162-170`, `derive_agent_url` at
    `restore_handler.rs:187` — total 8)
  - **r12: 8** (R10-C1+C2's `unregister_restored` is a
    `pub(crate)` helper on `NomadCHBackend`, NOT a trait method;
    correct visibility choice — the trait stays 8). **No trait
    drift this round.**
- **Net new wire endpoints** (T-7, T-8a): **0**. T-7 is a
  controller-side jobspec switch; T-8a is a shell-script change. No
  new HTTP routes, no new request shapes, no new error envelope
  fields. The wire surface is exactly as r11 saw it.
