# plugin-db Security Review — Round 12 (2026-05-22)

Target: `crates/plugin-db/` at HEAD `44ec83db`. Prior round: r11
(cycle 13:47) — 85/100.

Two commits in plugin-db since r11's HEAD `6cfa98df`:

| Commit | Subject | Security delta |
| --- | --- | --- |
| `14d7608f` | F2 Pending → Failed+marker (superseded) | n/a — replaced in same cycle |
| `6afab751` | F2 upgrade — `ValidationRefused` terminal + CHECK ALTER | NEW SQL, audited below |
| `44ec83db` | docs only | neutral |

`git diff 6cfa98df..HEAD -- crates/plugin-db/src/{lib.rs,wal_consumer.rs,backend/postgres.rs}`: zero
non-comment lines touching the three r11 carry-finding bodies.

---

## 1. Findings, by prompt lens

### 1.1 `6afab751` ALTER TABLE DROP/ADD CONSTRAINT idempotent widen — CLEAN

**Verdict: clean. Constraint widen is bound by `validate_app_id`
ASCII allowlist, runs under a session-scoped advisory lock from the
caller, and PG's `ALTER TABLE` semantics make the DROP/ADD window
race-free for concurrent INSERTs. Status value `'validation_refused'`
is parameter-bound on the INSERT path (not interpolated).**

```
[CLEAN] crates/plugin-db/src/audit.rs:263-290 — DROP/ADD CONSTRAINT
  Why:  At HEAD:

    // F2 (r13): widen the status CHECK on pre-existing audit tables to
    // include `'validation_refused'`. `DROP CONSTRAINT IF EXISTS` makes
    // this idempotent...
    let drop_status_chk = format!(
        r#"ALTER TABLE "{app_id}"."__zeroship_migrations"
            DROP CONSTRAINT IF EXISTS __zeroship_migrations_status_chk"#
    );
    pool.query_text_params(&drop_status_chk, &empty)
        .await
        .map_err(|e| coded_sql("drop status_chk", e))?;
    let add_status_chk = format!(
        r#"ALTER TABLE "{app_id}"."__zeroship_migrations"
            ADD CONSTRAINT __zeroship_migrations_status_chk CHECK (
                status IN ('pending','running','applied','applied_with_dead_letter','failed','cancelled','rolled_back','validation_refused')
            )"#
    );
    pool.query_text_params(&add_status_chk, &empty)
        .await
        .map_err(|e| coded_sql("add status_chk", e))?;

  Audited surfaces:

  (a) app_id interpolation safety.
      `ensure_audit_table_exists` runs `validate_app_id(app_id)?` at
      audit.rs:203 before any `format!` produces SQL. The validator at
      audit.rs:855-871 rejects empty and any character outside
      `[A-Za-z0-9_-]`, so quoted-identifier interpolation cannot escape
      the `"…"` delimiters. The constraint name
      `__zeroship_migrations_status_chk` is a Rust string literal — no
      interpolation in the constraint identifier.

      Per-app schema bound: the schema name `"{app_id}"` and the table
      `"__zeroship_migrations"` are per-app. No cross-app reach is
      possible from inside this function — the SQL never references
      another app's schema. Confirmed by grepping `ensure_audit_table_exists`
      callsites:
        backend/postgres.rs:199 — single app_id arg per call.

  (b) Status value `'validation_refused'` SQL-injection.
      Two places to check:
        * The constraint body literal at audit.rs:285. Hard-coded
          string literal in Rust source — no user input reaches it.
        * The INSERT path at audit.rs:336-358 binds `row.status.as_sql()`
          as parameter `$7` (audit.rs:340 SQL is `… status, … VALUES
          ($1, $2, $3, $4, $5::jsonb, $6, $7, $8, $9, $10::integer) …`
          with `params[6] = row.status.as_sql()`). `as_sql()` is a
          match returning one of seven hard-coded `&'static str`
          literals (audit.rs:131-149 + audit.rs:168-176). No format!
          path interpolates the status value.

      Verified by `git show 6afab751 -- crates/plugin-db/src/audit.rs`
      and reading audit.rs:336-358 at HEAD.

  (c) DROP/ADD race window for concurrent INSERTs.
      The user prompt flags "another concurrent INSERT could land an
      invalid status, then ADD would fail". Postgres semantics close
      this:
        * Every form of `ALTER TABLE` acquires `AccessExclusiveLock`
          on the table (PG manual ch. 13.3 — `ALTER TABLE` is in the
          ACCESS EXCLUSIVE group). This lock conflicts with EVERY
          other lock mode, including the `RowExclusiveLock` an INSERT
          would take.
        * Concurrent INSERTs are therefore BLOCKED for the duration
          of `DROP CONSTRAINT … DROP/ADD CONSTRAINT`; they cannot land
          a row that violates the about-to-be-installed wider CHECK.
        * The `DROP` and `ADD` execute in two separate statements, so
          there IS a brief window where the constraint is absent.
          However, no INSERT can interleave (locks held until each
          statement commits or releases; PG holds `AccessExclusiveLock`
          to end-of-transaction, which here is end-of-statement since
          the pool runs in autocommit). Even if an INSERT did
          interleave (it can't), the worst case is a row landing with
          a value outside the OLD list, which the NEW list is a strict
          superset of — net no-op.

      Net: race-free per PG semantics. Multi-worker concurrent boot of
      the same app would serialise on the table lock; not a parallel
      ALTER scenario.

  (d) Idempotent over multiple boots.
      `IF NOT EXISTS` on CREATE + `IF EXISTS` on DROP + `ADD CONSTRAINT`
      with stable name. On second boot: CREATE no-ops (table exists),
      DROP succeeds (constraint exists from first boot's ADD), ADD
      re-installs identical body. Each boot does a real DROP+ADD,
      which takes `AccessExclusiveLock` on the audit table during
      cold start — fine, single-process, no concurrent INSERT.

  (e) SQLite tier (not yet shipped).
      `ALTER TABLE … DROP CONSTRAINT` is not supported by SQLite. When
      the dev tier lands the SQLite backend will need a different
      migration path (e.g., `ALTER TABLE` → CREATE-new-table + COPY +
      DROP + RENAME, or a one-shot DELETE-FROM if the audit table is
      considered transient in dev). Note as a SQLite-tier follow-up;
      no impact at HEAD (Postgres-only).

  Verification:
    Read crates/plugin-db/src/audit.rs:202-290 — validate_app_id +
      CREATE + ALTER chain.
    Read crates/plugin-db/src/audit.rs:855-871 — validate_app_id
      ASCII allowlist.
    Read crates/plugin-db/src/audit.rs:336-365 — write_audit_row
      INSERT parameterises status at $7 via row.status.as_sql().
    Read crates/plugin-db/src/audit.rs:131-149 + 168-176 — as_sql()
      returns one of seven static literals.
    Grep `ensure_audit_table_exists` in crates/plugin-db/src/ — single
      production caller at backend/postgres.rs:199.
    `git show 6afab751 -- crates/plugin-db/src/audit.rs` — verified
      diff scope: enum variant + CHECK literal + ALTER block + tests.
```

### 1.2 `6afab751` validate.rs INSERT-direct-terminal — CLEAN

**Verdict: clean. Replaces the two-step Pending+UPDATE with a
single-statement terminal INSERT. The status value
`'validation_refused'` flows through `InitialStatus::ValidationRefused.as_sql()`
to `row.status.as_sql()` parameter binding — never format!'d into
SQL.**

```
[CLEAN] crates/plugin-db/src/orchestrator/register_model/validate.rs:82-101
  Why:  At HEAD:
    for op in &destructive {
        let row = crate::audit::AuditRow {
            …
            status: crate::audit::InitialStatus::ValidationRefused,
            …
        };
        if let Err(e) = backend.write_audit_row(&ctx.app_id, &row).await {
            tracing::warn!(error = ?e, "audit: failed to log destructive op");
        }
    }

  Compared to the superseded 14d7608f shape: that commit wrote
  Pending then ran `update_audit_status(…, TerminalStatus::Failed,
  Some("validation_refused"))`. The user-facing string
  `"validation_refused"` was passed as the `error` $3 bind on a
  parameterised UPDATE — also safe but two-statement, and the
  in-between warn (apply.rs-style F1 warn-half) carried Display of
  the UPDATE failure. r13's collapse to a single direct-terminal
  INSERT removes the warn site (the second tracing::warn at the old
  apply.rs Failed-row pattern is gone; replaced by one bare audit-
  write-failure warn at validate.rs:99 that emits only `error = ?e`,
  a `DbError` Debug formatter walking the same DbError fields r11
  audited clean).

  The single remaining warn at validate.rs:99 sinks `?e` (DbError
  Debug). DbError Debug derives over its variants and emits:
    ValidationFailed { code, message, … }
    Internal { message }
    Configuration { message }
    SchemaRefused { envelope_json, … }
    Transient { message }
    LockContention { message }
  All `message` fields trace to `walk_pg_chain(e)` or static Rust
  strings — same audit conclusion as r11 §1.1 / §3.

  Verification:
    Read crates/plugin-db/src/orchestrator/register_model/validate.rs:60-109
      — INSERT-direct-terminal shape.
    Read crates/plugin-db/src/audit.rs:336-365 — write_audit_row
      INSERT, status bound as $7.
    `git diff 14d7608f..6afab751 -- crates/plugin-db/src/orchestrator/register_model/validate.rs`
      — confirms removal of the UPDATE round-trip and second warn.
```

---

## 2. r11 carries — re-verified at HEAD

### 2.1 1.8a `init_pool_async` source-chain hostname leak (LOW, CARRY) — UNCHANGED

```
[LOW, CARRY] crates/plugin-db/src/lib.rs:434-457 — byte-identical
  Why:  At HEAD line numbers shifted from r11's 361-379 to 440-453
        (the gap is a new bench-only `row_to_json_for_bench` and a
        new `test_support` mod, both inserted earlier in the file).
        Function body is bytewise identical:

          let pool = Pool::connect(&url, 8)
              .await
              .map_err(|e| {
                  let mut msg = format!("db: failed to connect: {e}");
                  let mut cur: &dyn std::error::Error = &e;
                  while let Some(src) = std::error::Error::source(cur) {
                      msg.push_str(&format!(" — caused by: {src}"));
                      cur = src;
                  }
                  msg
              })?;

        `git diff 6cfa98df..HEAD -- crates/plugin-db/src/lib.rs` shows
        the additions are at lines 110-130 (test_support gate) and
        232-280 (`row_to_json_for_bench`) — not touching
        `init_pool_async`.

  Impact: LOW — unchanged. Same fix path as r11: redact in
    `init_pool_async`, or `tracing::error!` operator-side and surface
    only the top-level kind to the SDK.

  Verification:
    Read crates/plugin-db/src/lib.rs:434-457 — body matches r11's
      quoted block.
    `git diff 6cfa98df..HEAD -- crates/plugin-db/src/lib.rs` — no
      diff inside init_pool_async.
```

### 2.2 1.8b `wal_consumer::is_fatal` substring (MEDIUM, CARRY) — UNCHANGED

```
[MEDIUM, CARRY] crates/plugin-db/src/wal_consumer.rs:708-725
  Why:  Re-read at HEAD. Bytewise identical to r11's quoted block:

    pub fn is_fatal(err: &ConsumerError) -> bool {
        match err {
            ConsumerError::Io(s) | ConsumerError::Connect(s) | ConsumerError::Decode(s) => {
                …
                let lc = s.to_ascii_lowercase();
                lc.contains("58p01")
                    || lc.contains("does not exist")
                        && (lc.contains("replication slot") || lc.contains("publication"))
                    || lc.contains("invalid slot name")
            }
        }
    }

  Impact: MEDIUM — unchanged. Locale-sensitive PG builds will under-
    classify (within-app DoS-adjacent; multi-tenant unaffected).

  Fix: (a) `Option<SqlState>` on each `ConsumerError` variant or
       (b) wrap `compio_postgres::Error` directly.

  Verification:
    Read crates/plugin-db/src/wal_consumer.rs:708-725 — bytewise
      identical to r11's quoted block.
    `git diff 6cfa98df..HEAD -- crates/plugin-db/src/wal_consumer.rs`
      — empty (0 lines).
```

### 2.3 [I43] blocking `pg_advisory_lock` — UNCHANGED

```
[INFO, CARRY] crates/plugin-db/src/backend/postgres.rs:118 — pg_advisory_lock
  Why:  At HEAD:
          let sql = "SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)";
          client
              .query_text_params(sql, &[key1, key2])
              .await
              .map_err(|e| { … })?;

        Bytewise identical to r11. `git diff 6cfa98df..HEAD --
        crates/plugin-db/src/backend/postgres.rs` is empty.

  Impact: INFO/IMPORTANT (carryover). Within-app DoS only.
    Cross-tenant unaffected — advisory locks key on
    `(hashtext(app_id)::int4, hashtext(name)::int4)`; each tenant
    has a distinct keyspace.

  Fix: tracked in [I43] backlog — switch to `pg_try_advisory_lock` +
    surface as `DbError::LockContention { code: "lock_not_available" }`
    at the trait boundary so callers can apply typed retry/backoff.

  Verification:
    Read crates/plugin-db/src/backend/postgres.rs:110-134 — body
      unchanged from r11.
    `git diff 6cfa98df..HEAD -- crates/plugin-db/src/backend/postgres.rs`
      — empty.
```

### 2.4 `hardening` gate — HELD, auth/* invisible in default builds

```
[POSITIVE, HELD] crates/plugin-db/src/lib.rs:78-81
  Why:  At HEAD:
    #[cfg(all(feature = "hardening", not(feature = "test-helpers")))]
    pub(crate) mod auth;
    #[cfg(all(feature = "hardening", feature = "test-helpers"))]
    pub mod auth;

  Verified at HEAD:
    $ cargo build -p zeroship-plugin-db --lib
      → 17 warns, 0 errors. (r11 saw 15; the +2 are the new
        `row_to_json_for_bench` and `test_support` gate dead-code
        warnings — both `#[doc(hidden)]` / `#[cfg(test)]` items
        intentionally unused under the default profile.)
    $ cargo build -p zeroship-plugin-db --lib --features hardening
      → 63 warns, 0 errors. (r11 saw 61; +2 same source.)

  No change to the auth/* gate. The auth subtree (HMAC keys,
  SECURITY DEFINER bodies, session token mint, classify_p0001_detail)
  remains excluded from default builds.

  Verification:
    Read crates/plugin-db/Cargo.toml — `hardening = []` feature decl
      unchanged.
    Read crates/plugin-db/src/lib.rs:78-81 — cfg gates intact.
    Built both profiles at HEAD.
```

---

## 3. Cycle 15:17 SQL surface scan

Added in 6afab751 (the only SQL-touching commit since r11):

| Statement | File:line | app_id | Status value path |
| --- | --- | --- | --- |
| `ALTER TABLE … DROP CONSTRAINT IF EXISTS` | audit.rs:275-278 | validate_app_id-bound | n/a (no row values) |
| `ALTER TABLE … ADD CONSTRAINT … CHECK (status IN (…))` | audit.rs:282-287 | validate_app_id-bound | hard-coded literal list, no interp |
| CREATE TABLE CHECK list extended in-place | audit.rs:240 | validate_app_id-bound | hard-coded literal |
| `InitialStatus::ValidationRefused.as_sql() → "validation_refused"` | audit.rs:131-149 | n/a | bound as $7 on INSERT (audit.rs:336-365) |
| `TerminalStatus::ValidationRefused.as_sql() → "validation_refused"` | audit.rs:168-176 | n/a | bound as $2 on UPDATE (audit.rs:381-393) |

No `format!` site interpolates the new status value. No new schema or
table name introduced. No cross-tenant reach (single per-app schema).

### 3.1 Concurrent-ALTER scenario

If two workers (in a multi-worker pool) bootstrap the same app
simultaneously, both run `ensure_audit_table_exists`. The PG locking
contract:
- Both contend for `AccessExclusiveLock` on the audit table.
- One wins, runs CREATE-IF-NOT-EXISTS (no-op or full create), then
  DROP/ADD CONSTRAINT.
- The other blocks for the duration, then runs the same sequence
  against the table the first worker just installed. DROP IF EXISTS
  finds the constraint (just installed by first worker), drops it,
  ADD reinstalls the identical body. Net: idempotent serial repeat,
  no race.

The orchestrator wraps the bootstrap in a session-scoped advisory
lock via `acquire_advisory_lock` (see r11 §2.3 / [I43]) so in
practice only one worker reaches the ALTER per app per boot — the PG
lock is the second line of defence.

### 3.2 SQLite tier

`ALTER TABLE … DROP CONSTRAINT` is not supported by SQLite (only
`ALTER TABLE … RENAME` / `ADD COLUMN`). The dev-tier SQLite backend
(not yet implemented) will need an alternative migration shape — for
example, dropping and recreating the audit table from scratch since
dev audit history is non-durable, or using SQLite's table-recreation
pattern (`CREATE new`, `INSERT SELECT`, `DROP old`, `ALTER RENAME`).
This is a SQLite-tier follow-up, not a HEAD-impacting finding;
plugin-db currently ships Postgres only.

---

## 4. Cross-check: r11 cleanliness reverified

- **r11 §1.1 finalise_backfill warn DbError Display sink** — sink
  unchanged this cycle; the validate.rs warn at line 99 emits
  `error = ?e` (Debug, not Display), but Debug walks the same struct
  fields and the same `message`-from-walk_pg_chain pipeline. No new
  PII vector.
- **r11 §1.2 IsolateDbContext field privatization** — context.rs
  unchanged; no field re-exposed.
- **r11 §1.3 mig_lock accessor pub(crate)** — unchanged.
- **r11 §3 9-emission tracing inventory** — net change: one warn
  REMOVED (the apply.rs-style Failed/validation_refused warn from the
  superseded 14d7608f shape is gone; the validate.rs INSERT-direct-
  terminal path emits one warn instead of two). No new warn sites.
- **r10 §1.1 validate_field_name non-ASCII reject** — query.rs
  unchanged.
- **r9 §1.2 sanitise_app_id contract** — replication.rs:82-101
  unchanged. validate_app_id at audit.rs:855-871 unchanged.

No regressions across the eleven prior rounds' findings.

---

## 5. Score delta vs r11

| | r11 | r12 |
| --- | --- | --- |
| Findings closed since prior round | 0 | 0 |
| Findings opened this round | 0 | 0 |
| Security-positive structural changes | `IsolateDbContext` field privatization; all 6 mig_lock accessors `pub(crate)` | `validate.rs` Pending→UPDATE→Failed compressed to single-statement terminal INSERT — one fewer SQL round-trip + one fewer warn sink to audit |
| Re-verified clean | 9-emission tracing inventory carries no new PII; cycle 13:17+13:47 add no new SQL | 6afab751's new ALTER + status literal audited under PG `AccessExclusiveLock` semantics + validate_app_id ASCII allowlist + INSERT $7 parameterisation; cycle 15:17 adds 3 new format!-built SQL strings, all bound by validate_app_id and per-app schema |
| Carryover | [I43] unchanged, 1.8a unchanged, 1.8b unchanged | [I43] unchanged, 1.8a unchanged (line numbers shifted), 1.8b unchanged |

Delta breakdown:

- **+1** for `6afab751`. Two compounding improvements:
  1. The destructive-op audit row now lands directly terminal — one
     statement, one lock, no two-write orphan window. Removes a
     class of partial-state corruption (Pending row stuck if worker
     crashes mid-transition) that the superseded 14d7608f shape
     mitigated only with a warn-half emission.
  2. One fewer `tracing::warn` sink for the destructive-op refusal
     path (the apply-style second warn is removed). Net: smaller
     attack surface for the §3-style PII / credential leak audits;
     fewer sinks to vet on future cycles.

  The new SQL (ALTER DROP/ADD CONSTRAINT + extended CHECK literal)
  is fully bound by `validate_app_id`, runs under PG's
  `AccessExclusiveLock` so concurrent INSERTs cannot interleave
  through the brief constraint-absent window, and the new status
  value `'validation_refused'` flows only through parameter binds
  and a static `as_sql()` literal — never through format!.

- **+0** for `14d7608f` — superseded in-cycle by 6afab751; no
  net contribution.

- **+0** for `44ec83db` (docs only).

- **+0** for the carried r11 issues (1.8a hostname LOW, 1.8b
  is_fatal MEDIUM, [I43] blocking advisory lock).

- **Net: +1.** Score moves to **86 / 100**.

What would push past 88 (unchanged from r11's list):

1. Close 1.8a (`init_pool_async` source-chain redaction).
2. Close 1.8b by typing `ConsumerError` with `Option<SqlState>` so
   `is_fatal` reads SQLSTATE directly.
3. Resolve [I43] (`pg_try_advisory_lock` + typed
   `LockContention { code: "lock_not_available" }`).

What would push past 92 (unchanged from r11's list):

4. Split `hardening` into `hardening-bootstrap` + `hardening-session`.
5. Convert `ConsumerError` to typed-SQLSTATE-carrying variants.
6. Per-app PG role ownership of WAL slot.
7. `tracing::error!` source chain on operator side + sanitised
   top-level kind to tenant for DNS-failure observability.

New for the SQLite tier (follow-up, not HEAD-impacting):

8. SQLite-tier ALTER strategy for the audit-table CHECK widen —
   `DROP CONSTRAINT` is not supported by SQLite; the dev-tier
   migration shape will need table-recreation or full-reset.

---

## Score (1-100)

**86 / 100**

Up +1 from r11 (85). Cycle 15:17 shipped 6afab751, which closes the
F2 destructive-op orphan-Pending audit window by landing those rows
directly terminal as `'validation_refused'` instead of
Pending→UPDATE→Failed-with-marker. The change introduces 3 new SQL
strings via `format!` — all of them bound by `validate_app_id`
(ASCII allowlist, audit.rs:855-871), per-app-schema-scoped (no
cross-tenant reach; sole caller at backend/postgres.rs:199), and
running under PG's `AccessExclusiveLock` which serialises against
concurrent INSERTs through the DROP/ADD constraint window. The new
status literal `'validation_refused'` reaches SQL only as a `$7`
parameter bind on INSERT (audit.rs:336-365) or `$2` on UPDATE
(audit.rs:381-393), via `InitialStatus::as_sql()` / `TerminalStatus::as_sql()`
returning hard-coded `&'static str` literals — never format!-
interpolated. The three r11 carries (1.8a hostname LOW, 1.8b is_fatal
MEDIUM, [I43] blocking advisory lock) remain open at the same severity
and same fix path; 1.8a's line numbers shifted from 361-379 to 440-453
due to the new bench/test-support modules at lib.rs:110-130 and
232-280, but the function body itself is bytewise identical. A
SQLite-tier follow-up is noted: `ALTER TABLE … DROP CONSTRAINT` is
not supported by SQLite, so the dev backend (not yet shipped) will
need an alternative shape (table recreation or dev-reset).
