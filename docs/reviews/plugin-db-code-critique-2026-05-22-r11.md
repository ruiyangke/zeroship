# plugin-db code-quality critique — round 11 (2026-05-22)

**Scope**: `crates/plugin-db/` at HEAD `71a457a1`. Prior rounds r1–r10.
r10 scored **95/100** ("freeze the lens"; recommended move to
architecture/concurrency rounds). r11 runs because cycle 12:17 landed
a 5-site pattern-conversion that needs auditing for consistency.

**Cycle-12:17 commits in scope**:
- `51c342e8` — `release_advisory_lock` returns `Result<(), DbError>` (closes [I6])
- `fcf7ce3c` — `tracing::warn!` on audit-status update failure, 5 sites (F1 warn-half)
- `71a457a1` — doc-drift cleanup on `auth/mod.rs` + `lib.rs` (docs-audit r7)

**Method**: read every diff line; grep'd for surviving `let _ = ...await`
patterns; verified `cargo build -p zeroship-plugin-db --lib` (15 warnings,
unchanged) and `cargo test -p zeroship-plugin-db --lib` (352 pass).

---

## TL;DR

The three commits land cleanly. No new CRITICAL, no new MAJOR. The
warn-half conversion is correct (no shadowing, no accidental `?` leak,
no test/warning-clean regression) but introduces two MINOR
**field-shape / message-style inconsistencies** within the very five
sites the commit message claims it unified. The Result-returning
release-advisory-lock cousin (`51c342e8`) is clean.

**Score: 95/100 (no delta vs r10's 95).** The conversion is net-positive
on observability and net-zero on idioms; the inconsistencies are real
but cosmetic, well under the 0.5-pt threshold for moving the score.
r10's freeze-the-lens recommendation **stands**. r11 exists only to
discharge the audit duty on the new 5-site pattern; no further code-
critique rounds should run until something more substantive than a
warn-emit conversion lands.

The two explicit questions:

1. **Did `fcf7ce3c`'s pattern-conversion introduce any new code
   smell?** Two MINOR inconsistencies (field naming `error` vs
   `audit_error` vs `upd_err`; message body has the
   `"row stays in 'running' until reset"` consequence-phrase in 2-of-5
   sites and omits it in 3-of-5). Both are sub-finding-grade. See
   **MINOR-R11-1** below.

2. **Are there other `let _ = ...await` sites that should have been
   included in the same sweep?** **No.** The remaining 9 hits in the
   crate are categorically different (test-only scaffolding,
   best-effort DROP INDEX cleanup, best-effort ROLLBACK on rollback-
   helper, `OrchestratorLockGuard::release` which internally already
   warns and returns `Ok(_)` by construction). See
   **MINOR-R11-2** for the one borderline case worth tracking.

---

## CRITICAL findings

None.

---

## IMPORTANT findings

None.

---

## MINOR findings

### MINOR-R11-1 — five-site sweep is consistent at the structural level but inconsistent at the field / message level

The conversion shape is uniform across all five sites:

```rust
if let Err(<binding>) = crate::audit::update_audit_status(...).await {
    // <comment>
    tracing::warn!(app_id, audit_id = id, ..., error = ?<binding>, "...");
}
```

No shadowed variables, no awkward block nesting, no `?` propagation,
the underlying DDL error path is unchanged in every site (verified
against the surrounding `result` / `refuse(...)` / `return Err(...)`
flow). Build stays at 15 warnings, all 352 lib tests pass.

The cosmetic inconsistencies:

**(a) Err-binding name (3 distinct names for the same role)**:

| Site | File:line | Binding | Tracing field name |
|---|---|---|---|
| Applied terminal | `apply.rs:163` | `e` | `error = ?e` |
| Failed terminal | `apply.rs:190` | `upd_err` | `audit_error = ?upd_err` |
| invalid_index | `postgres.rs:487` | `e` | `error = ?e` |
| data_violation | `postgres.rs:542` | `upd_err` | `error = ?upd_err` |
| index_build | `postgres.rs:594` | `upd_err` | `error = ?upd_err` |

Three permutations: `e`/`error`, `upd_err`/`error`, `upd_err`/`audit_error`.
The Failed-branch in `apply.rs` is the only one that disambiguates the
DDL error from the audit-write error in the field name (`ddl_error`
+ `audit_error`), which is the **most informative** shape but is not
replicated to `postgres.rs:542,594` where there is also an outer DDL
error in scope (`e` is shadowed by the if-let's `upd_err`, so
`e` is no longer visible — but the SQLSTATE is still recorded as
`sqlstate = code_str` in the data_violation case).

**(b) Consequence phrase only in 2-of-5 messages**:

```rust
// apply.rs (both sites):
"update_audit_status(Applied) failed; row stays in 'running' until reset"
"update_audit_status(Failed) failed; row stays in 'running' until reset"

// postgres.rs (all three sites — no consequence phrase):
"update_audit_status(Failed/invalid_index) failed"
"update_audit_status(Failed/data_violation) failed"
"update_audit_status(Failed/index_build) failed"
```

The consequence phrase is the operator-actionable bit ("you have a
stuck row; reset to clear"). The `postgres.rs` messages name the
sub-cause better (`invalid_index` / `data_violation` / `index_build`)
but drop the operator hint. Ideal would be both — sub-cause in the
verb-tag, consequence phrase trailing — across all five.

**(c) `app_id` format spec drift**:

`apply.rs` uses `app_id = %app_id` (`app_id: String`, Display-format
forced); `postgres.rs` uses the shorthand `app_id` (`app_id: &str`,
auto-recorded as Display). Same wire shape on the tracing layer, but
visually inconsistent in source. The shorthand is the idiomatic
choice; `%app_id` is only needed when there's a naming clash with
the function-local binding — there is none here in `apply.rs`, so
the explicit `%` is gratuitous.

**Disposition**: MINOR. The on-the-wire structured field set is good
enough that a log-pipeline grep for `update_audit_status` + `app_id`
still finds all five. A future cleanup could:

- Standardize on `upd_err` as the local binding and `audit_error` as
  the tracing field name (matches `apply.rs:Failed` which is the most
  information-dense site).
- Add the consequence phrase to all five messages.
- Drop the `%` format spec in `apply.rs`.

Not worth its own commit; fold into the next plugin-db touch.

---

### MINOR-R11-2 — `migrations.rs:476` ROLLBACK helper is the one remaining cousin worth tracking

```rust
async fn rollback_and_return(backend: &PostgresBackend, client: LockClient) {
    let _ = backend.client_exec(&client, "ROLLBACK", &[]).await;
    return_lock_client(client);
}
```

This is the closest semantic cousin to the swept sites that was NOT
included in `fcf7ce3c`. The shape:

- Best-effort cleanup on an error path (matches the F1 warn-half profile).
- Silently swallowing a wire-level error from PG.
- Operator would benefit from knowing "we tried to ROLLBACK and it
  errored" (could indicate the connection is already dead, which is
  fine; could indicate something the next caller will hit, which is
  not).

The reason it's MINOR-not-IMPORTANT and the reason it was correctly
**excluded** from the F1 warn-half sweep: this is the rollback path
on a different transaction-lifecycle dimension (commit-batch ROLLBACK
on lock-row contention), not on the audit-row terminal-state
dimension. F1 is specifically about audit rows stuck in `running`. A
follow-up "rollback-helper warn-half" item is a candidate for a future
cycle but is out-of-scope for F1.

**Disposition**: MINOR carry, separate item from F1.

---

### The other 8 `let _ = ...await` sites — all correctly out of scope

For completeness (and so future audits don't re-litigate):

| Site | Why correctly skipped |
|---|---|
| `lib.rs:256,279,286,315` | All four are `#[cfg(any(test, feature = "test-helpers"))]` test-helper scaffolding (`clear_migration_lock_for_tests`, `install_tx_marker_for_tests`, `uninstall_tx_marker_for_tests`). Not production paths; no audit-row semantics. |
| `backend/postgres.rs:509,564,617` | DROP INDEX cleanup after a failed CREATE INDEX. Best-effort: if DROP fails the index stays in INVALID state, the surrounding code's `refuse(...)` already names the underlying CREATE failure. Different semantic dimension from audit-row terminal-state. Candidate for a separate "DROP INDEX cleanup warn-half" item if operator demand surfaces. |
| `orchestrator/register_model/apply.rs:251`, `bootstrap.rs:137`, `mod.rs:219` | All three call `OrchestratorLockGuard::release().await`. By construction `release()` catches the inner `pg_advisory_unlock` SQL error and emits its own `tracing::warn!` (`lock_guard.rs:172-178`), returning `Ok(_)` unconditionally. The outer `let _ = ...await` discards `Ok(Option<PooledClient>)` — there is no error variant to ever surface. Cousin to MAJOR-R9-3 (release-tristate), but the swept sites in F1 don't reach this code. |

---

## Regression-baseline checks (carried from r10)

| r10 check | r11 status |
|---|---|
| Default-build warning count | 15 (unchanged) |
| `cargo test -p zeroship-plugin-db --lib` | 352 pass (unchanged) |
| `Result<_, String>` production sites | unchanged: `lib.rs:351` (R9-1), `v8_classes/migration.rs:{455,743}`, `orchestrator/register_model/validate.rs:59` |
| `auth/*` cfg gating intact | yes — `lib.rs` arm still `#[cfg(feature = "hardening")]` on `mod auth`; r11's `71a457a1` only touches docstrings. |

`71a457a1` is doc-only: `auth/mod.rs` rewrites the "Backwards
compatibility" preamble section, and `lib.rs` adds the three-arm
`hardening`-ladder explanation to the existing cfg-fork rationale.
Both correctly reflect the post-`2fa9472e` state. The `required-features`
update on `[[test]] integration` (now `["test-helpers", "hardening"]`)
is consistent with the build invariant. Zero code change; zero risk
on this commit.

`51c342e8` is also clean: trait signature is now `Result<(), DbError>`,
the Postgres impl correctly lifts `compio_postgres::Error` via
`DbError::from_pg`, and the two production callers in `migrations.rs`
emit a structured `tracing::warn!` on `Err` (with `app_id` + `name`
+ `error`). The warn message in `migrations.rs:289-294,663-668`
correctly notes "lock auto-releases on session end", which matches
the prior best-effort semantics — observability-only change. Note that
`51c342e8`'s warn message style (`error = %e`, Display) is yet another
permutation vs `fcf7ce3c`'s `error = ?e` (Debug). Sub-finding; tracked
under MINOR-R11-1's "field-shape inconsistency" umbrella.

---

## Score breakdown

| Dimension | r10 | r11 | Δ |
|---|---|---|---|
| Correctness | 96 | 96 | 0 |
| Performance | 96 | 96 | 0 |
| Security | 94 | 94 | 0 |
| API Design | 95 | 95 | 0 |
| Rust Idioms | 96 | 96 | 0 |
| **Weighted total** | **95** | **95** | **0** |

The conversion lifts observability (Correctness +0.3 informally) and
loses a smidge on consistency (API Design −0.3 informally); the two
cancel exactly. No genuine movement.

---

## Recommendation

**Freeze code-critique here.** r10 already called this. r11 exists
only because cycle 12:17 landed a multi-site sweep and the audit
discipline demanded a "did this sweep introduce smell?" / "did the
sweep miss anything?" pass. Both questions have been answered
("two MINOR inconsistencies; nothing missed in scope"). The cost of
running r12+ on subsequent cycles will exceed the marginal yield —
the per-cycle finding density is now firmly in the "fold into the
next plugin-db touch" tier, not the "needs its own commit" tier.

The next plugin-db code-critique round should run only on:
- a structural refactor (e.g., backend trait reshape, audit module rewrite),
- a new subsystem joining the crate,
- a CRITICAL bug surfacing from production,
- or an explicit operator-facing pattern change (e.g., the F1 sweeper-half).

Cosmetic field-name / message-style cleanups should ride on whatever
production-driven commit hits those files next.

---

## Open items at r11 (delta vs r10)

| Item | Source | Status |
|---|---|---|
| MAJOR-R9-1 `init_pool_async` `Result<_, String>` | r9 | Carry, unchanged at `lib.rs:351` |
| MAJOR-R9-3 `OrchestratorLockGuard::release` tristate | r9 | Carry, unchanged at `orchestrator/lock_guard.rs:144` |
| MIN-R9-2 `runtime_state` consolidator 5-of-6 callsites bypassed | r9 | Carry, unchanged |
| MIN-R9-4 `config_hinted` 1-of-9 adoption | r9 | Carry, unchanged |
| MIN-R8-5 `into_held` dead helper | r8 | Carry, unchanged |
| MIN-R8-7..10 cosmetic | r8 | Carry, unchanged |
| **MINOR-R11-1 5-site warn-shape inconsistency** | **r11 NEW** | New, fold-into-next-touch |
| **MINOR-R11-2 `migrations.rs:476` rollback-helper warn-half cousin** | **r11 NEW** | New, separate from F1 |

Net open: **2 MAJOR** + **7 MINOR** (was 2 + 5). The two new MINORs
are both sub-finding-grade; tracking them is a courtesy, not a
ship-blocker.
