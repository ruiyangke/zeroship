# plugin-db re-sweep r2 — correctness + concurrency — 2026-05-25

**Base:** main `fc72c62d` (post security-fixes). **Lens:** correctness regressions from the recent security/stability/perf fixes + concurrency/stability.
**Reviewer:** Codex (gpt-5.5, xhigh, read-only), findings then **verified line-by-line by the pilot** against the actual code before recording here.

**Counts (verified):** CRITICAL 0 · IMPORTANT 5 (2 fix-now SQLite · 3 deferred PG/known).

Clean surfaces re-confirmed: write-pipeline consolidation (insertMany/upsert/update encryption + system fields), read-pipeline ciphertext/masked leak fixes, internal-column blocking (`__fts`/`_masked` in select/sort/distinct/aggregate), query budgets, CAS / nested-`version` rejection, soft-delete filtering, SQLite tx-slot restoration, SQLite lock-map cleanup, V8 BYTEA JSON encoding.

---

## F2 — `count` does not thread `SqlDialect` — IMPORTANT (FIX NOW, SQLite-testable)
`crates/plugin-db/src/crud/mod.rs:1869` calls `query::build_count_with_soft_delete(...)` (dialect-unaware, PG-default `build_where`), whereas the sibling read paths thread the dialect: aggregate `mod.rs:1725` → `build_aggregate_with_soft_delete_with_dialect`, distinct `mod.rs:1791` → `build_distinct_with_soft_delete_with_dialect`.
- **Not** a `$N`-placeholder problem: SQLite natively accepts `$N` placeholders (see `backend/sqlite/fts.rs` mixing `$1`/`$2`/`?`), so plain WHERE counts work on SQLite today.
- **Real residue:** the dialect also controls encrypted-column bind-placeholder wrapping (`SqlDialect::encrypted_column_bind_placeholder` / `wrap_encrypted_param`). Counting with a filter **on a deterministically-encrypted column** on SQLite would emit PG-flavoured encryption wrapping → wrong SQL / failure. Aggregate & distinct were already fixed for exactly this; count was missed.
- **Fix:** add `build_count_with_soft_delete_with_dialect` and route `dispatch_count` through it with the active backend's dialect (mirror aggregate/distinct). Regression test: SQLite count filtered on a deterministic-encrypted field returns the right rows (fails on the PG-default builder).

## F3 — auto-tx is PG-only; SQLite multi-op handlers lose atomicity — IMPORTANT (FIX NOW, SQLite-testable)
`crates/plugin-db/src/transaction/auto_tx.rs:220` — `exec_auto_begin` does `backend.as_postgres().ok_or_else(|| backend_unsupported("auto-tx"))`. On SQLite the implicit auto-tx that wraps a transactional RPC-handler dispatch either errors (`backend_unsupported`) or is skipped, so a handler doing multiple writes is **not atomic** on SQLite — a mid-handler failure leaves partial state.
- Note: explicit `env.db.transaction()` **is** wired for SQLite (`v8_classes/transaction.rs`; divergences doc confirms plain `BEGIN`). Only the *implicit* auto-tx wrapper is missing.
- **Fix:** wire the SQLite arm of `exec_auto_begin`/`exec_auto_end` to acquire the dedicated SQLite client and emit `BEGIN`/`COMMIT`/`ROLLBACK` (mirror `v8_classes/transaction.rs`'s SQLite begin path), installing the `TxConnection::Sqlite(..)` into the context slot the same way the PG arm installs `TxConnection::Postgres`. Regression test: a multi-op SQLite handler that fails on op 2 rolls back op 1.

---

## F1 — PG encrypted-read BYTEA decode assumes text-hex, but driver requests BINARY — IMPORTANT (DEFER: needs live PG)
`crud/unmask.rs:457-471` (and the crate-wide convention in `crud/encryption_pass.rs` `decrypt_row_on_read`, `mask_drift.rs`, `mask_backfill.rs`, `broker.rs`) read PG BYTEA as `Option<&str>` then `hex_to_bytes`, per the documented assumption "BYTEA arrives over the text protocol as `\xHH…`".
- **But** `compio-postgres/src/query.rs:419` passes `Some(1)` as the result-column format to `frontend::bind` — **format code 1 = BINARY**. Under binary results, BYTEA is raw bytes, and `&str` `FromSql` rejects `Type::BYTEA` outright (type mismatch) unless the SELECT casts the column to text. None of these SELECTs cast.
- Meanwhile `encryption/keys.rs:288` reads BYTEA as `Vec<u8>` (binary) — inconsistent with the hex-text paths. Exactly one of these conventions is correct against a live server.
- **Why deferred:** the lib unit tests exercise `hex_to_bytes` directly, not a real BYTEA round-trip; the actual PG wire behaviour needs the live-PG matrix (already a documented deferral — no PG available while offline). "Fixing" blind risks inverting a convention that may be correct (if a cast exists elsewhere, or if the read path uses `simple_query`/text). **Action: resolve under the PG-leg matrix** — confirm the real wire format once, then make all BYTEA reads consistent (binary `Vec<u8>` everywhere, or an explicit `encode(col,'hex')` cast everywhere).

## F5 — `SET ROLE` per-app fence leaks stale role into the pooled conn on cancellation — IMPORTANT, **security-relevant** (DEFER: needs live PG)
`exec.rs:176-197` `query_postgres_pool_with_autocommit_role`: checkout → `apply_autocommit_role` (`SET ROLE app_X`) → query → `reset_autocommit_role` (`RESET ROLE`). If the future is **dropped (cancelled) after SET ROLE but before RESET** (e.g. client disconnect / timeout mid-query), the pooled `client` guard returns to the pool still carrying `app_X`'s role.
- **Impact:** a later checkout that does *not* re-set the role (a privileged login-role path: DDL/register_model, migrations) would run under `app_X` → wrong privileges / cross-tenant fence. **Mitigation already present:** the autocommit CRUD path always `SET ROLE`s at checkout, so autocommit→autocommit is self-healing; the hazard is autocommit-leak → login-role-path checkout sharing the same pool.
- **Why deferred:** `SET ROLE` is session-level (can't use `SET LOCAL` outside a tx); async `Drop` can't `await` a reset. The robust fix is **reset-on-checkout/checkin in the pool** (`DISCARD ALL` / `RESET ROLE` before reuse) or wrapping autocommit in a real tx so `SET LOCAL ROLE` auto-reverts — both change pool/exec semantics for *every* PG query and must be validated against a live server. **Action: fix under the PG-leg matrix.** Whether the privileged paths share the autocommit pool determines real severity — confirm with live PG.

## F4 — SQLite GlobalApp `register_model` lock is backend-instance-local — IMPORTANT (ALREADY DEFERRED)
`backend/sqlite/mod.rs:426` — the lock serializes within one backend instance, not across the isolate-global register_model path (TOCTOU window). This is the **previously-documented deferred item** (isolate-global register_model lock / cross-thread registry redesign; SQLite-dev-scoped). No change this cycle.
