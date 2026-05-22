# plugin-db — Test Coverage Review (round 11)

- **Date:** 2026-05-22 (cycle 10:47)
- **Scope:** `crates/plugin-db/` (src + tests + benches)
- **Lens:** Test coverage
- **Anchors:** r7 (83) · r8 (84) · r9 (84) · r10 (84)
- **Method:** Read-only. Re-ran `cargo test --lib` (default + `hardening`),
  re-built bench harness, diffed every src/* file touched since the r10
  anchor, audited the `hardening` gate boundary, audited the new tracing
  side-effects in `set_mig_lock` / `return_mig_client`.

---

## TL;DR

**HEAD is `403b3891`, not `05484878`** (the prompt's stated HEAD is the
docs-only commit; one more code commit landed after). Three code commits
in this cycle:

1. `2fa9472e` — `hardening` feature gate around the `auth/*` subtree.
2. `5d9acab8` — `tracing::error!` / `tracing::warn!` on the
   `set_mig_lock` shadow-replace and `return_mig_client` empty-slot
   branches.
3. `403b3891` — `validate_field_name` rejects non-ASCII identifiers
   (closes the r10/r2 GAP-1 / I12 carry-over). Includes **two new
   tests** at `query.rs:4285` and `:4298`.

**Lib test counts (HEAD = 403b3891):**

| Feature flag                  | Tests | Δ vs r10 |
| ----------------------------- | ----- | -------- |
| (default)                     | **349** | n/a — gating reshuffle |
| `--features hardening`        | **373** | +2 vs r10 (371) |
| Hardening delta (`auth/*`)    | +24     | exact: 16 (session) + 4 (keys) + 4 (bootstrap) |

The hardening gate arithmetic is exact and structurally sound: there are
zero `crate::auth` references outside the `auth/*` subtree itself (grep
hits only doc comments inside `auth/session.rs`). The `[[test]]
integration` target lists `required-features = ["test-helpers",
"hardening"]`, which correctly reaches the 35+ integration tests that
import `zeroship_plugin_db::auth::*` (`tests/integration.rs:3379-3755+`).
**Gating is correct.**

Net: **+1 vs r10 (84 → 85).** The bump is GAP-1 closure plus the
hardening gate being cleanly drawn. The score does not jump further
because (a) the tracing side-effects in 5d9acab8 ship without a
log-capture test, and (b) the other r10 carry-overs (multibyte
validate_collection boundary, queue_or_emit direct unit test, lenient
strictness integration, four bare files, audited-CIC branches) are all
unchanged.

---

## 1. Tool results

```
git rev-parse HEAD                                     → 403b3891

cargo test -p zeroship-plugin-db --lib                 → 349 passed (0.14s)
cargo test -p zeroship-plugin-db --lib
   --features hardening                                → 373 passed (0.14s)
cargo bench -p zeroship-plugin-db
   --bench bench_query_build --no-run                  → bench harness builds
                                                         clean (0.24s incr)
```

All green, no flakes, sub-second wall time.

---

## 2. Hardening gate audit (commit 2fa9472e)

**Verified correct.**

### 2.1 The gate

`crates/plugin-db/src/lib.rs:68-71`:

```rust
#[cfg(all(feature = "hardening", not(feature = "test-helpers")))]
pub(crate) mod auth;
#[cfg(all(feature = "hardening", feature = "test-helpers"))]
pub mod auth;
```

Both arms require `hardening`. There is no third `cfg(not(hardening))`
arm — so without the feature the `auth` module is not declared at all.
Confirmed via `cargo test --lib`: 349 tests, none of which are
`auth::*`.

### 2.2 Subtree isolation

`grep -rn 'crate::auth\|use crate::auth\|super::auth\|self::auth' src/`
returns only two hits, both inside `auth/session.rs` and both inside
`///` doc-comments (`auth/session.rs:166`, `:191`). **No production
code outside the subtree references `crate::auth`** — so gating it out
in default builds creates no dead links.

### 2.3 Arithmetic

```
src/auth/session.rs   16 #[test] / #[compio::test]
src/auth/keys.rs       4
src/auth/bootstrap.rs  4
src/auth/mod.rs        0
                      --
                      24  (matches the 373 - 349 delta exactly)
```

### 2.4 Integration suite

`tests/integration.rs` has 35+ references to `zeroship_plugin_db::auth::*`
(`ensure_admin_schema`, `mint_session_token`, `init_session`,
`keys::current_key_id`, `keys::rotate_session_keys`, `SessionInit`).
`[[test]] integration` requires `hardening`, so these compile.

**Failure mode I checked for: was anything mis-gated?** None observed.
The commit only added `hardening` to the existing `cfg(test-helpers)`
fork and tightened the integration target's `required-features`. No
auth tests leak through default `--lib`; no auth tests are
unreachable.

---

## 3. mig_lock tracing audit (commit 5d9acab8) — NEW GAP

The commit adds two side-effects that the existing slot tests
(`context.rs:813-908`) deliberately *don't* assert on:

### 3.1 `set_mig_lock` shadow-replace error log

`context.rs:373-384`:

```rust
pub(crate) fn set_mig_lock(&mut self, lock: MigrationLock) -> Option<MigrationLock> {
    if let Some(prev) = self.mig_lock.as_ref() {
        tracing::error!(
            prev_name = %prev.name, prev_audit_id = prev.audit_id,
            new_name  = %lock.name, new_audit_id  = lock.audit_id,
            "set_mig_lock called while another lock is active …",
        );
    }
    self.mig_lock.replace(lock)
}
```

The existing test `set_mig_lock_replaces_existing_returns_previous`
(`context.rs:838-854`) exercises the `prev.is_some()` branch and
asserts the `replace` return value — but does NOT assert the
`tracing::error!` fired. **A regression that silently removed the log
would not be caught.**

This is the I23 deferred item the commit was opened against; the
commit closes the **code** half of I23 but leaves the **test** half
open.

### 3.2 `return_mig_client` empty-slot warn log

`context.rs:405-412`:

```rust
pub fn return_mig_client(&mut self, client: Client) {
    match self.mig_lock.as_mut() {
        Some(lock) => lock.client = Some(client),
        None => tracing::warn!(
            "return_mig_client: mig_lock slot empty — client dropped …",
        ),
    }
}
```

The existing test `return_mig_client_after_cancel_is_silent_noop`
(`context.rs:896-908`) explicitly **skips** passing a real `Client`
("we can't construct a real Client here"); it only re-asserts
`!has_mig_lock()` after a no-op `clear_mig_lock`. The branch that
would now fire `tracing::warn!` is **not entered** by any test —
neither before nor after the commit. **Same untested-side-effect gap
as 3.1, plus the underlying branch is still uncovered.**

### 3.3 Severity

These are observability-only paths; a regression that breaks the log
emission causes a degraded debug experience, not data loss. Listing as
**a new gap** but ranking it low. A test-coverage closure would require:

- adding `tracing-subscriber` to `[dev-dependencies]` (currently only at
  workspace root for other crates), and
- using `tracing::subscriber::with_default` + a custom capture layer
  to assert the event fired with the expected fields.

The mechanical cost is small. **Logged here as the only new gap this
cycle.**

---

## 4. r10 carry-overs — status check

| ID  | Description                                                | r10  | r11  |
| --- | ---------------------------------------------------------- | ---- | ---- |
| I12 | `validate_field_name` rejects non-ASCII (GAP-1)            | open | **CLOSED** by 403b3891 |
| I13 | `queue_or_emit` direct unit test (GAP-2)                   | open | open |
| I14 | `lenient` strictness integration test (GAP-3)              | open | open |
| —   | Multibyte 63-byte boundary on `validate_collection`        | open | open |
| —   | `create_index_with_recovery_audited` 5+ branches           | open | open |
| —   | Four bare files (`crud.rs`, `v8_bridge.rs`, two v8_classes)| open | open |

### 4.1 I12 — CLOSED

`query.rs:127-132` now rejects any char outside `[A-Za-z0-9_]`. Two
new tests at lines 4285 and 4298:

- `validate_field_name_rejects_non_ascii` — exercises `café`, `naïve`,
  `日本`, `user—id`, `field name`.
- `validate_field_name_accepts_ascii_allowlist` — pins the positive
  side (`id`, `user_id`, `createdAt`, `v2`, `_private`).

This is the first r10 carry-over to close in three rounds.

### 4.2 I13 — `queue_or_emit` still indirectly tested

`exec.rs:257` `queue_or_emit` has two branches (`in_tx=true` → push to
pending_emits, `in_tx=false` → `wal_consumer::emit_local`). Integration
tests at `tests/integration.rs:3077-3174` use the `*_for_tests` helpers
which **bypass** `queue_or_emit` (they push directly into
`pending_emits` via `push_pending_emit_for_tests`). The function itself
is reached only via real CRUD callbacks behind `install_tx_marker_for_tests`,
where the in_tx branch is implicitly observed. **Unchanged from r10.**

### 4.3 I14 — `lenient` strictness still untested

`grep -rn 'lenient' tests/integration.rs` → zero hits. The
`validate.rs:90-93` lenient branch ("fall through, but apply will skip
… destructive ops") has no end-to-end coverage. **Unchanged from r10.**

### 4.4 Multibyte 63-byte boundary

`validate_collection_rejects_name_exceeding_63_bytes` at
`query.rs:4221` is still ASCII-only:

```rust
let name = "a".repeat(64);
…
assert!(validate_collection(&"a".repeat(63)).is_ok(), …);
```

No multibyte cases (3-byte ×21 = 63 bytes pass; 3-byte ×22 = 66 fail;
4-byte ×16 = 64 fail; mixed-width straddle). Note that 403b3891 made
this slightly less critical — `validate_field_name` now rejects all
non-ASCII outright, so the multibyte boundary case applies only to
collection names (the `__zeroship`/`pg_` reserved-prefix check still
runs first). **Unchanged from r10.**

### 4.5 `create_index_with_recovery_audited` and four bare files

No movement. Same defensive responses as r6-r10. The plateau on these
is **STRONG → STRONG-WANING** — the GAP-1 closure shows the team will
pick off the cheap ones eventually.

---

## 5. New gaps introduced this cycle

Exactly one, scoped narrowly:

- **NEW-R11-1** — `set_mig_lock` shadow-replace `tracing::error!` and
  `return_mig_client` empty-slot `tracing::warn!` (both added by
  5d9acab8) have no log-capture test asserting the events fire with
  the expected fields. Side-effect-only paths; observability-only
  blast radius. See §3 above.

Severity: LOW (observability, not data-path).

---

## 6. Per-file `#[test]` / `#[compio::test]` count (HEAD = 403b3891)

```
tests   file
 149    src/query.rs              (+2 since r10)
  34    src/context.rs
  29    src/broker.rs
  26    src/wal_consumer.rs
  20    src/read_set.rs
  16    src/auth/session.rs       gated by `hardening`
  14    src/replication.rs
  13    src/error.rs
  11    src/diff.rs
  10    src/v8_classes/replication.rs
   8    src/v8_classes/migration.rs
   6    src/orchestrator/lock_guard.rs
   4    src/replication_ops.rs
   4    src/orchestrator/auto_tx.rs
   4    src/auth/keys.rs          gated by `hardening`
   4    src/auth/bootstrap.rs     gated by `hardening`
   4    src/audit.rs
   3    src/v8_classes/db.rs
   3    src/orchestrator/register_model/apply.rs
   3    src/migrations.rs
   3    src/exec.rs
   2    src/v8_classes/subscription.rs
   2    src/backend/postgres.rs
   1    src/backend/mod.rs
   0    src/v8_classes/transaction.rs        ← bare
   0    src/v8_classes/mod.rs
   0    src/v8_classes/migrations.rs
   0    src/v8_classes/collection.rs         ← bare
   0    src/v8_bridge.rs                     ← bare
   0    src/orchestrator/transaction.rs
   0    src/orchestrator/register_model/{validate,plan,mod,bootstrap}.rs
   0    src/orchestrator/mod.rs
   0    src/lib.rs
   0    src/crud.rs                          ← bare
   0    src/auth/mod.rs
```

Per-file totals: 149 (query) + 24 (auth) + 200 (everywhere else) =
373, matches `--features hardening`. Default = 373 - 24 = 349, also
matches.

---

## 7. Score (1-100)

```
Round  Score  Delta  Notes
-----  -----  -----  ------------------------------------------------------
r7     83     —      Baseline.
r8     84     +1     Wave of new tests.
r9     84      0     One behavioural test.
r10    84      0     Zero new tests; plateau STRONG.
r11    85     +1     GAP-1/I12 CLOSED via 403b3891 (+2 query tests).
                     New cycle commits: hardening gate (correct, +24
                     gated), tracing-side-effects (untested → new minor
                     gap). r10 carry-overs minus GAP-1 still open.
```

**Score: 85** — first move off the 84 plateau in three rounds.

### Why not higher

- Two new untested code paths in 5d9acab8 (the tracing side-effects).
- Five r10 carry-overs still open (queue_or_emit, lenient, multibyte
  collection boundary, audited-CIC, four bare files).
- The non-ASCII closure was a single-test fix; that alone doesn't lift
  the score much.

### Why not lower

- Hardening gate is precise: arithmetic exact, no leakage, no
  unreachable tests, integration suite correctly opted-in via
  `required-features`. This is exactly how a feature gate should look.
- 373 tests, all green, 0.14s.
- The cycle shipped two pieces of *code* with *tests for one of them*
  — a ratio that was 0/1 in r9 and 0/3 in r10. The "code without
  tests" rate dropped this cycle.
- Bench harness still builds clean from cold.

---

## 8. Files reviewed

- `/home/ruiyang/Projects/appbase/crates/plugin-db/Cargo.toml`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/lib.rs` (gate)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs:362-412, 800-908`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/query.rs:97-138, 4248-4310`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/exec.rs:240-306`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/auth/{session,keys,bootstrap,mod}.rs` (count only)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/tests/integration.rs:3077-3174, 3379-3755`
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-deferred.md` (I12/I13/I14/I23 cross-ref)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-test-coverage-2026-05-22-r10.md`
