# plugin-db Security Review — Round 11 (2026-05-22)

Target: `crates/plugin-db/` at HEAD `6cfa98df`. Prior round: r10
(cycle 11:17) — 84/100.

Four commits in plugin-db since r10's HEAD `2d34061e`:

| Commit | Subject | Security delta |
| --- | --- | --- |
| `251d53b4` | `row_to_json` O(N²) → O(N) via index lookup | neutral — pure perf, no security surface |
| `18aee490` | `finalise_backfill` F1 warn-shape drift fix | positive — observability unification; verify Display sink |
| `bac64c0e` | demote `mig_lock` accessors `pub` → `pub(crate)` | positive — surface narrowing |
| `f6adb68b` | privatize `IsolateDbContext` fields | positive — closes [I16] compile-time enforcement of `tx_token_counter` mutation invariant |

Inventory of accumulated `tracing::{warn,error}!` emissions across
cycles 10:47–13:47 (9 sites) covered by this audit.

---

## 1. Findings, by prompt lens

### 1.1 `18aee490` — DbError Display sink for `audit_err = %audit_err` — CLEAN

**Verdict: clean. No PII / DB URL / hostname / table-internal bytes
in the renamed structured field.**

```
[CLEAN] crates/plugin-db/src/migrations.rs:652-662 — finalise_backfill warn
  Source of `audit_err`:
    finalise_backfill → audit.rs:766 `coded_sql("finalise_backfill", e)`
      → error.rs:404 `coded_sql` returns DbError (variant from
        SQLSTATE via `DbError::from_pg(&e)`) with
        `message = prefix_message + walk_pg_chain(e)`.

  Display impl at error.rs:441-458:
        DbError::SchemaRefused { envelope_json, .. } => write(envelope_json)
        DbError::ValidationFailed { message, .. } | ... => write(message)

  `audit_err = %audit_err` invokes Display → writes the `message`
  field only.

  `message` is sourced from `walk_pg_chain(e)` at error.rs:498-506:
        let mut msg = format!("db: {e}");
        let mut cur: &dyn std::error::Error = e;
        while let Some(src) = std::error::Error::source(cur) {
            msg.push_str(&format!(" — caused by: {src}"));
            cur = src;
        }

  The compio_postgres::Error renders the server-side error body. The
  Postgres-side message for a finalise_backfill UPDATE failure
  (no RETURNING clause, no row values selected) carries SQLSTATE +
  error-class prose ("permission denied for schema", "relation does
  not exist", "lock not available", etc.) — NOT row contents and NOT
  the DB URL/host.

  Sibling structured fields:
    app_id   (tenant identifier; already operator-visible per
              architectural model — same exposure as every other
              app-scoped log line in the crate)
    name     (migration name from user code; not row data)
    collection (collection name from user schema; not row data)
    audit_id (i64 row PK; not user data)
    transition = ?terminal (AuditTerminal Debug — enum discriminator)

  No new leak vector vs r10.

  Verification:
    Read crates/plugin-db/src/migrations.rs:652-662 — emission site.
    Read crates/plugin-db/src/error.rs:441-458 — Display impl writes
      `message` only, no field other than `message`/`envelope_json`.
    Read crates/plugin-db/src/audit.rs:742-768 — finalise_backfill
      SQL uses no RETURNING; the UPDATE failure path returns Postgres
      error class strings, not row contents.
    `git show 18aee490 -- crates/plugin-db/src/migrations.rs` —
      field rename only (`error → audit_err`, `terminal → transition`).
```

### 1.2 `f6adb68b` — `tx_token_counter` compile-time enforcement — VERIFIED

**Verdict: the docstring assertion "tx_token_counter mutation is
private" holds at compile time. Only `next_tx_token()` references
the field; the field itself is private.**

```
[CLEAN] crates/plugin-db/src/context.rs — IsolateDbContext fields privatized
  Why:  At HEAD, all 11 data fields on IsolateDbContext are private
        (no `pub` or `pub(crate)` modifier — implicit Rust default).
        Re-read:
          context.rs:78  `pool: Option<Rc<Pool>>,`
          context.rs:82  `db_url: Option<String>,`
          context.rs:86  `registered_models: HashSet<String>,`
          context.rs:97  `tx_conn: Option<Client>,`
          context.rs:107 `auto_tx_owned: bool,`
          context.rs:121 `tx_token: u64,`
          context.rs:127 `tx_token_counter: u64,`
          context.rs:144 `pending_emits: Option<Vec<ChangeEvent>>,`
          context.rs:151 `mig_lock: Option<MigrationLock>,`
          context.rs:157 `running_consumers: HashSet<String>,`
          context.rs:166 `backend: Option<Rc<PostgresBackend>>,`

        Grep across `crates/plugin-db/src/` for `tx_token_counter`
        yields 5 matches, all in `context.rs`:
          context.rs:18   module-level docstring
          context.rs:73   IsolateDbContext docstring (the assertion)
          context.rs:127  field declaration
          context.rs:184  `Self::new()` initializer (zero)
          context.rs:314  `self.tx_token_counter.wrapping_add(1)`
            inside `pub fn next_tx_token(&mut self) -> u64`

        Compile verifies: any out-of-impl mutation of `tx_token_counter`
        would fail with E0616 (field is private). The docstring at
        context.rs:73 ("only `next_tx_token` should touch it") is
        compile-time true at HEAD.

        The fields adjacent (`tx_token`, `tx_conn`, `pool`, `db_url`,
        `pending_emits`, `mig_lock`, etc.) are equally locked down;
        cycle 11:47 had already moved `mig_lock` mutation through the
        typed accessors, and `f6adb68b` finishes the sweep.

  Net: +1 on score. Compile-time invariant locks closed a class of
    future-regression risk (a contributor adding direct field
    mutation to bypass the debug_assert + tracing in the accessors).

  Verification:
    Read crates/plugin-db/src/context.rs:76-167 — no `pub`/`pub(crate)`
      on any data field.
    Read crates/plugin-db/src/context.rs:309-316 — `next_tx_token` is
      the only mutator of `tx_token_counter`.
    `cargo build -p zeroship-plugin-db --lib` at HEAD — 0 errors.
    `cargo build -p zeroship-plugin-db --lib --features hardening` —
      0 errors (the auth/* subtree does not reach across the
      privatised boundary).
```

### 1.3 `bac64c0e` — mig_lock accessor visibility — CLEAN

**Verdict: surface-narrowing only. All 6 mig_lock accessors now
`pub(crate)`; no production caller outside the crate.**

```
[CLEAN] crates/plugin-db/src/context.rs:366-426 — mig_lock accessors
  Grep at HEAD for `fn (has_mig_lock|clear_mig_lock|take_mig_client|
  return_mig_client|mig_lock_snapshot|set_mig_lock)` in
  crates/plugin-db/src/context.rs returns 6 production matches, all
  `pub(crate)`:
    context.rs:366  pub(crate) fn has_mig_lock(&self) -> bool
    context.rs:381  pub(crate) fn set_mig_lock(&mut self, lock: MigrationLock)
    context.rs:396  pub(crate) fn clear_mig_lock(&mut self)
    context.rs:403  pub(crate) fn take_mig_client(&mut self) -> Option<Client>
    context.rs:414  pub(crate) fn return_mig_client(&mut self, client: Client)
    context.rs:426  pub(crate) fn mig_lock_snapshot(&self) -> Option<(String, ...)>

  Asymmetry from r10 (`set_mig_lock` `pub(crate)` since `5d9acab8`,
  other 5 still `pub`) is closed.

  No security delta — these accessors return `pub(crate)` types
  (`Option<MigrationLock>` whose interior is `Client` from
  compio_postgres). Exposing them externally was already inert because
  the wrapping types are not re-exported, but the symmetry now
  matches the rest of the api-surface review's closure pattern.

  Verification:
    Grep `pub fn (has_mig_lock|clear_mig_lock|take_mig_client|
      return_mig_client|mig_lock_snapshot)` in crates/plugin-db/ —
      zero matches at HEAD.
    `git show bac64c0e --stat` — context.rs 5+/5- (visibility-only).
```

### 1.4 `251d53b4` — `row_to_json` index lookup — CLEAN

**Verdict: pure perf change. Bounds-checked index lookup has
equivalent runtime safety to the name lookup. No new security
surface.**

```
[CLEAN] crates/plugin-db/src/v8_bridge.rs:354-362 — row_to_json
  Why:  The diff swaps `row.try_get::<_, T>(col.name())` →
        `row.try_get::<_, T>(idx)` and `row.raw_value(col.name())` →
        `row.raw_value(idx)`. The `idx` is enumerated from
        `row.columns().iter().enumerate()` — bounded by the actual
        row column count, so no out-of-bounds path.

        `RowIndex for usize` (compio-postgres/src/row.rs:49) is a
        bounds-check + return — same Result handling as the name
        lookup. The Err paths (line 366, 371, ...) still fall through
        to `Value::Null`, so the wire shape JS sees is unchanged.

  Verification:
    Read crates/plugin-db/src/v8_bridge.rs:354-362 — enumerate +
      index threaded through `column_to_json(row, idx, oid)`.
    `git show 251d53b4 --stat` — v8_bridge.rs 25+/17- (mechanical).
```

---

## 2. r10 carries — re-verified at HEAD

### 2.1 1.8a `init_pool_async` source-chain hostname leak (LOW, CARRY) — UNCHANGED

```
[LOW, CARRY] crates/plugin-db/src/lib.rs:361-379 — byte-identical
  Why:  Re-read at HEAD. lib.rs:367-379 still:

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

        The line numbers shifted by +4 from r10 (was 357-374, now
        361-379) due to upstream docstring edits, but the source-chain
        walk is bytewise identical.

  Impact: LOW — unchanged. Same fix path as r8–r10: redact in
    `init_pool_async`, or `tracing::error!` operator-side and surface
    only the top-level kind to the SDK.

  Verification:
    Read crates/plugin-db/src/lib.rs:361-379 — body matches r10's
      quoted block (whitespace + identifiers identical).
    `git diff 2d34061e..HEAD -- crates/plugin-db/src/lib.rs` —
      empty for this function body (the surrounding cfg gates moved,
      not the function).
```

### 2.2 1.8b `wal_consumer::is_fatal` substring-match (MEDIUM, CARRY) — UNCHANGED

```
[MEDIUM, CARRY] crates/plugin-db/src/wal_consumer.rs:708-725
  Why:  Re-read at HEAD. wal_consumer.rs:708-725 still:

          pub fn is_fatal(err: &ConsumerError) -> bool {
              match err {
                  ConsumerError::Io(s) | ConsumerError::Connect(s)
                  | ConsumerError::Decode(s) => {
                      ...
                      let lc = s.to_ascii_lowercase();
                      lc.contains("58p01")
                          || lc.contains("does not exist")
                              && (lc.contains("replication slot")
                                  || lc.contains("publication"))
                          || lc.contains("invalid slot name")
                  }
              }
          }

        Bytewise identical to r10. ConsumerError variants still hold
        `String` (no SQLSTATE field added).

  Impact: MEDIUM — unchanged. Locale-sensitive PG builds will under-
    classify (within-app DoS-adjacent; multi-tenant unaffected).

  Fix: (a) `Option<SqlState>` on each `ConsumerError` variant or
       (b) wrap `compio_postgres::Error` directly.

  Verification:
    Read crates/plugin-db/src/wal_consumer.rs:708-725 — byte-
      identical to r10's quoted block.
    `git diff 2d34061e..HEAD -- crates/plugin-db/src/wal_consumer.rs`
      — empty.
```

### 2.3 [I43] blocking `pg_advisory_lock` — UNCHANGED + corrected location

**Carry note:** r10's carry summary describes [I43] as living in
`auth/bootstrap.rs` (gated behind `hardening`). That is incorrect.
The blocking `pg_advisory_lock` site is in `backend/postgres.rs:118`
(always-compiled, not gated). At HEAD `crates/plugin-db/src/auth/`
contains no `pg_advisory_lock` callsite. The dispatch flow is:

  orchestrator/lock_guard.rs → backend.acquire_advisory_lock(...)
  → backend/postgres.rs:118 `SELECT pg_advisory_lock(...)`

```
[INFO, CARRY] crates/plugin-db/src/backend/postgres.rs:118 — pg_advisory_lock
  Why:  At HEAD:
          let sql = "SELECT pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)";
          client
              .query_text_params(sql, &[key1, key2])
              .await
              .map_err(|e| { ... })?;

        Blocking — waits indefinitely on lock contention. The within-app
        DoS shape from r5/r6 is unchanged. Note that
        `backend/postgres.rs:145` also exposes the non-blocking
        `pg_try_advisory_lock` variant via the
        `try_acquire_advisory_lock` trait method (used by migrations.rs
        — non-blocking path).

        The `backend` module (lib.rs:65) is always-compiled — NOT
        gated behind `hardening`. r10's carry description ("gated
        behind hardening") was incorrect.

  Impact: INFO/IMPORTANT (carryover from r3-r5). Within-app DoS only.
    Cross-tenant unaffected — Postgres advisory locks key on
    `(int4, int4)` from `hashtext(app_id)`; each tenant has a distinct
    keyspace.

  Fix: tracked in [I43] backlog — switch to `pg_try_advisory_lock` +
    surface as `DbError::LockContention { code: "lock_not_available" }`
    at the trait boundary so callers can apply a typed retry/backoff.

  Verification:
    Grep `pg_advisory_lock` in crates/plugin-db/src/ — 7 production
      matches; the only blocking-form callsite in Rust is
      backend/postgres.rs:118.
    Grep `pg_advisory_lock` in crates/plugin-db/src/auth/ — zero
      matches at HEAD (confirms r10 location was wrong).
    Read crates/plugin-db/src/lib.rs:65 — `pub(crate) mod backend;`
      with no `cfg(feature)` — always-compiled.
```

### 2.4 `hardening` gate — HELD, auth/* invisible in default builds

```
[POSITIVE, HELD] crates/plugin-db/Cargo.toml:57 + lib.rs:78-81
  Why:  At HEAD:
          Cargo.toml:57 — `hardening = []` feature.
          lib.rs:78-81 — `mod auth` gated:
            #[cfg(all(feature = "hardening", not(feature = "test-helpers")))]
            pub(crate) mod auth;
            #[cfg(all(feature = "hardening", feature = "test-helpers"))]
            pub mod auth;

        Verified at HEAD:
          $ cargo build -p zeroship-plugin-db --lib
            (no --features) — compiles cleanly, 15 warns.
            The auth/* subtree (bootstrap.rs HMAC keys + SECURITY
            DEFINER CREATE FUNCTION bodies, session.rs mint_session_token,
            keys.rs rotation, mod.rs) is excluded entirely.

          $ cargo build -p zeroship-plugin-db --lib --features hardening
            — compiles cleanly, 61 warns (the additional 46 are dead-
            code warns inside auth/*; consistent with the architect r7
            note that no production caller hits the subtree yet).

        Implication for r11's lenses:
          - The five P0001 DETAIL discriminator tokens
            (`classify_p0001_detail` in auth/session.rs) only exist in
            `--features hardening` builds.
          - HMAC keys (auth/keys.rs) only exist in `--features
            hardening` builds.
          - Session-token mint (auth/session.rs) only exists in
            `--features hardening` builds.
          - The two `LIKE '__zs_%'` literals in auth/bootstrap.rs are
            server-side SQL inside SECURITY DEFINER bodies and only
            compile under `--features hardening`.

  Net: +0 (already priced into r10's 84). Held at HEAD.

  Verification:
    Read crates/plugin-db/Cargo.toml:50-64 — feature decl + test
      required-features `["test-helpers", "hardening"]`.
    Read crates/plugin-db/src/lib.rs:78-81 — cfg gates intact.
    `cargo build -p zeroship-plugin-db --lib` and `... --features
      hardening` — both compile.
```

---

## 3. New surface: 9 accumulated `tracing::{warn,error}!` emissions

**Verdict: clean. None of the 9 emissions surface DB credentials,
passwords, host/URL strings, table-internal-bytes (raw row contents
outside the constraint-error path already user-visible), or PII
beyond the operator-visible app_id / migration name / collection
name identifiers that every other crate log line already carries.**

Inventory (cycles 10:47–13:47):

| Site | File:line | Fields emitted | Sink for `audit_err` |
| --- | --- | --- | --- |
| `5d9acab8` set_mig_lock shadow-replace | context.rs:383-389 | prev_name, prev_audit_id, new_name, new_audit_id | n/a |
| `5d9acab8` return_mig_client slot-empty | context.rs:417-419 | (none — static message) | n/a |
| `fcf7ce3c` apply.rs Applied-row warn | apply.rs:178-184 | app_id, audit_id, transition="Applied", audit_err | DbError Display → message |
| `fcf7ce3c` apply.rs Failed-row warn | apply.rs:209-216 | app_id, audit_id, transition="Failed", ddl_err, audit_err | DbError Display → message |
| `fcf7ce3c` postgres.rs invalid_index warn | backend/postgres.rs:496-503 | app_id, audit_id, transition, attempt, audit_err | DbError Display → message |
| `fcf7ce3c` postgres.rs data_violation warn | backend/postgres.rs:548-555 | app_id, audit_id, transition, sqlstate, audit_err | DbError Display → message |
| `fcf7ce3c` postgres.rs index_build warn | backend/postgres.rs:597-605 | app_id, audit_id, transition, attempt, transient, audit_err | DbError Display → message |
| `51c342e8` release_advisory_lock cancelled-refusal warn | migrations.rs:291-296 | app_id, name, error=%e | DbError Display → message |
| `51c342e8` release_advisory_lock backfill-finalise warn | migrations.rs:670-675 | app_id, name, error=%e | DbError Display → message |
| `18aee490` finalise_backfill warn | migrations.rs:652-662 | app_id, name, collection, audit_id, transition, audit_err | DbError Display → message |

That's 10 emissions actually (the user's "9" inventory missed the
second context.rs return_mig_client warn at context.rs:417-419, a
static-string warn that emits no fields). All sinks for `%audit_err`
/ `%e` / `%ddl_err` go through DbError Display (error.rs:441-458),
which writes only the `message` field (or the SchemaRefused
envelope_json).

`message` is sourced from one of:
- `walk_pg_chain(e)` (`db: <pg-error-body> — caused by: ...`) for
  Postgres-side failures
- `coded_sql("context", e)` adds a `<context>: ` prefix in front of
  walk_pg_chain
- variant-direct construction via `DbError::internal(...)` /
  `validation(...)` / `config(...)` — all-Rust-controlled strings

Audited the constructor sites for the variant-direct cases:

```
[CLEAN] DbError::Configuration construction sites do not leak DB URL
  Why:  Grep for `DbError::Configuration` constructors in
        crates/plugin-db/src/ at HEAD:
          exec.rs (lazy_init_failed) — uses the upstream String error
            (which IS the source-chain walk from init_pool_async,
            covered by r10 §1.4 / r11 §2.1 — known carry; not a new
            leak)
          orchestrator/register_model/mod.rs:120 — same pattern
          error.rs:114 (config helper) — static strings only

        None of these directly format `db_url` or `host` into the
        message body. The hostname leak is solely confined to the
        `init_pool_async` source-chain walk (r11 §2.1).

  Verification: re-reading walk_pg_chain at error.rs:498-506 — walks
    `std::error::Error::source`, which for compio_postgres::Error
    surfaces the server-rendered SQLSTATE message body, never the
    connect-time hostname.
```

The `ddl_err = %msg` field at apply.rs:213 is the only emission that
DOES carry constraint-detail prose (which may include row values via
Postgres' DETAIL line — e.g. `Key (email)=(foo@bar.com) already
exists`). This is operator-side observability, NOT a new leak: the
same `msg` IS already flowing back to the JS caller via the
DbError's `to_op_error()` path (apply.rs:192 `let msg =
e.clone().into_string();`). The user who saw the error in JS
already has the same string. No new exfiltration surface introduced
by `fcf7ce3c`.

### 3.1 Zero new SQL strings

```
[CLEAN] git diff 2d34061e..HEAD -- 'crates/plugin-db/src/**/*.rs'
  filtered for `(SELECT|INSERT|UPDATE|DELETE|CREATE|DROP|ALTER|
  TRUNCATE).*(FROM|VALUES|TABLE|SCHEMA|INDEX|CONSTRAINT)` matches in
  added lines (`^\+`) — zero hits.

  The 4 cycle commits touch only:
    - v8_bridge.rs (row_to_json index threading)
    - migrations.rs (warn field rename)
    - context.rs (visibility + privatization)

  No SQL surface change, no new query construction site, no new
  format!(SQL, …) callsite.
```

---

## 4. Cross-check: things that did NOT regress

Re-verified against HEAD:

- **r10 §1.1 `validate_field_name` non-ASCII rejection** — query.rs
  unchanged since `403b3891`; the ASCII allowlist + 2 unit tests
  still pin the contract.
- **r10 §1.2 `OBJECT_PREFIX` LIKE retro-closure** — replication.rs
  unchanged this cycle; all LIKE predicates still parameter-bind.
- **r10 §1.7 `mig_lock` single-threaded slot** — `bac64c0e`
  surface-narrowing and `f6adb68b` privatization do not alter the
  state-machine semantics; `take_mig_client() → await →
  return_mig_client()` contract preserved.
- **r9 §1.2 app-id isolation** — sanitise_app_id +
  resolve_*_app_id pattern unchanged at HEAD.
- **r9 §1.3 SQL-injection sweep** — re-run against the 4 cycle
  commits, zero new SQL strings (§3.1).
- **r4 `sanitise_app_id` contract** — empty-reject +
  `[A-Za-z0-9_]`-only + lowercase on success unchanged at
  replication.rs:82-101.

No regressions observed across the ten prior rounds' findings.

---

## 5. Score delta vs r10

| | r10 | r11 |
| --- | --- | --- |
| Findings closed since prior round | 1 (r9 §1.1.b unicode MINOR) | 0 — no carry closed |
| Findings opened this round | 0 | 0 |
| Security-positive structural changes | hardening committed; `slot_status` `pub→pub(crate)` | `IsolateDbContext` fields privatized (compile-time `tx_token_counter` invariant); all 6 mig_lock accessors `pub(crate)` |
| Re-verified clean | all r9 cleanlines + cycle 11:17's 5 commits add no new SQL / V8 surface / state-machine race | all r10 cleanlines + cycle 13:17 + 13:47 add no new SQL / V8 surface / state-machine race; 9-emission tracing inventory carries no new PII / credential / tenant-row leak |
| Carryover | [I43] unchanged, 1.8a unchanged, 1.8b unchanged | [I43] unchanged + location corrected (`backend/postgres.rs:118`, not `auth/bootstrap.rs`), 1.8a unchanged, 1.8b unchanged |

Delta breakdown:

- **+1** for `f6adb68b` (privatize IsolateDbContext fields). The
  `tx_token_counter` mutation invariant is now compile-time
  enforceable — a regression where a future contributor reaches
  past `next_tx_token` would fail with E0616. This is a meaningful
  structural narrowing of an invariant that previously relied on
  reviewer vigilance. Same class of improvement as the [I12]
  unicode allowlist that delta-bumped r10.

- **+0** for `bac64c0e` (mig_lock accessor visibility). Surface
  narrowing without a new compile-time invariant being enforced
  (the slot was already pub-but-unused externally).

- **+0** for `18aee490` (finalise_backfill warn-shape unification).
  Pure observability shape; Display sink audited clean of new PII
  vectors (§1.1).

- **+0** for `251d53b4` (row_to_json index lookup). Pure perf.

- **+0** for the carried r10 issues (1.8a hostname LOW, 1.8b
  is_fatal MEDIUM, [I43] blocking advisory lock).

- **Net: +1.** Score moves to **85 / 100**.

What would push past 88 (no change from r10's list):

1. Close 1.8a (`init_pool_async` source-chain redaction). Easiest;
   the operator-vs-tenant info asymmetry is the only LOW open.
2. Close 1.8b by adding `Option<SqlState>` to
   `ConsumerError::{Connect,Io,Decode}` so `is_fatal` reads SQLSTATE
   directly.
3. Resolve [I43] (`pg_try_advisory_lock` + typed
   `LockContention { code: "lock_not_available" }`).

What would push past 92 (no change from r10's list):

4. Split `hardening` into `hardening-bootstrap` + `hardening-session`
   sub-features.
5. Convert `ConsumerError` to typed-SQLSTATE-carrying variants.
6. Per-app PG role ownership of WAL slot.
7. `tracing::error!` source chain on operator side + sanitised
   top-level kind to tenant for DNS-failure observability.

---

## Score (1-100)

**85 / 100**

Up +1 from r10 (84). Cycle 13:17 + 13:47 added one structural
narrowing (compile-time enforcement of `tx_token_counter` mutation
invariant via `IsolateDbContext` field privatization at `f6adb68b`)
on top of three security-neutral cleanups (visibility narrowing,
warn-shape unification, perf-only row_to_json index threading). The
9-emission `tracing` inventory across cycles 10:47–13:47 was audited
for PII / credential / tenant-data leakage; all sinks for
`%audit_err` / `%e` flow through `DbError::Display` which writes
only the `message` field (Postgres-side server-rendered error body
+ static Rust context prefix), never DB URL / hostname /
table-internal-bytes. Zero new SQL strings, zero new V8-boundary
entry points, zero new state-machine race surface. The three r10
carries (1.8a hostname LOW, 1.8b is_fatal MEDIUM, [I43] blocking
advisory lock) remain open at the same severity and same fix path.
One r10 inaccuracy corrected: [I43]'s blocking
`pg_advisory_lock` lives at `backend/postgres.rs:118` (always-
compiled), not under the hardening gate as r10's carry table
described.
