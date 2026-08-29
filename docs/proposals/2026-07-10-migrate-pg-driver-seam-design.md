# Postgres driver seam for the `zeroship-migrate` core — design

**Status:** proposed 2026-07-10. Structural refactor of `crates/zeroship-migrate` (branch `refactor/migrate-driver-seam`, Phase 2). No engine-logic change; no wire/IR change; no behavior change; commit-only.

**Goal.** Make the Postgres **driver** adaptable. Today `PostgresBackend` and its collaborators (`PgOnline`, `PgShadow`, backfill, journal) thread a concrete `&compio_postgres::Client` through the whole apply path. This milestone extracts a small in-session driver trait — **`PgSession`** — that those types are generic over, and provides `compio-postgres` as the **default** impl behind a new **`native-pg`** feature (default-on). Because the native impl is a one-line forward per method, the default build is **byte-for-byte behaviorally unchanged**. The abstraction makes the driver swappable so a later Node/napi shell can supply a host-callback driver (the `pg` npm client) as a *second* impl.

<!-- Revised 2026-07-10 (round 2): the milestone framing changed. The trait's own signatures name `compio_postgres::{Row, Error, types::ToSql}` (§2.2), and the ungated apply modules carry `impl From<compio_postgres::Error>` in nine+ places (journal/executor/drift/baseline/precondition/capability/role/conn — grep-verified). Those are mutually exclusive with "the trait compiles with zero `compio-postgres` in the tree." Neutralizing them (associated `type Error`, a local `SeamRow`, dropping the concrete `ToSql`) is exactly the wide churn this doc defers to §9. So the milestone is **generic-over-driver, single compio-named surface, whole seam gated behind `native-pg`** — NOT a zero-impl abstraction build. The old §6a "`--no-default-features` proves the abstraction" claim is retracted; §6a is now a `native-pg`-off *omission* proof (the PG modules genuinely disappear), and genericity is proven by an in-crate recording driver under §6d. -->

> **What "adaptable" buys, and what it does not.** The seam is a **transport abstraction**: the in-session verbs (`batch_execute` / `execute` / `execute_text_params` / `query` / `query_one`) become trait methods; everything above them (guard, IR, render, journal logic, advisory locks, confinement SQL) is untouched. `PostgresBackend<'a, D: PgSession>` becomes generic over the driver so a later Node/napi shell can drop in a second impl (the `pg` npm client) without touching the apply logic. The second (Node) driver impl and any compio removal are **FUTURE WORK** (§9), positioned but not built here.
>
> **Honest scope of "swappable" — HALF the surface is swappable today; the read path is NOT.** <!-- Revised round 4 (addressing MINOR): the milestone's central promise ("a later Node driver drops in without touching apply logic") is validated for only the write/DDL half. The concrete `Row` RETURN type is a hidden SECOND coupling a napi driver cannot satisfy (napi cannot return a compio `Row`), so the §3.2 SeamRow widening is a PREREQUISITE for the Node driver, not additive polish. State it up front so "makes the driver swappable" stays honest. --> This seam makes the **write/DDL/txn/journal-write** verbs (`batch_execute` / `execute` / `execute_text_params`) genuinely swappable — those are run-proven generic against a non-compio driver (§6d). The **read verbs** (`query` / `query_one`) return the **concrete `compio_postgres::Row`**, which is a **second, hidden coupling point**: a napi/Node driver *cannot* construct or return a compio `Row` across the FFI boundary (`Row` has private fields, is not constructible outside its crate — §6d). Therefore the §3.2 `SeamRow`/`SeamValue` widening is a **hard PREREQUISITE** for the Node driver, **not** the additive follow-up §3.2/§9.1 might read as. **The Node driver is NOT reachable by *this* seam alone** — this milestone swaps the write path and *type-checks* (but does not run-prove) the read path; the read path only becomes swappable after the `Row`-return coupling is widened to `SeamRow`. "Makes the driver swappable" is true for the write half now, and for the read half only after §3.2 lands.
>
> **This milestone ships exactly one impl — `compio_postgres::Client` — and the *entire* `PgSession` seam (trait + generic backend + PG apply modules) is gated behind the default-on `native-pg` feature.** The trait's method signatures still name `compio_postgres::{Row, Error, types::ToSql}` for zero row-decode / error-conversion churn (§2.2, §2.3, §3.1); those concrete names are exactly why the seam **cannot** compile with `compio-postgres` absent from the tree. We do **not** claim a zero-impl `--no-default-features` build of the seam. What `native-pg`-off proves instead is that the PG apply path is a cleanly *removable* module — a `native-pg`-off build omits `mod postgres` and every `compio_postgres`-naming apply leaf and still compiles (SQLite + IR + render + guard only). Genericity — that `PostgresBackend` monomorphizes against a *non-*compio driver — is proven by an in-crate recording `PgSession` (§6d), not by a driverless build. The neutralized surface (associated `type Error`, a local `SeamRow`, dropping the concrete `ToSql` from the trait) is the widening deferred to §9 when the Node impl lands.

This is the sibling of the V8-decoupling milestone (`docs/proposals/2026-07-10-migrate-runtime-decoupling-design.md`, Phase 1). Phase 1 cut a V8-free core behind `zsv8`; Phase 2 cuts a driver-neutral apply path behind `native-pg`. The two features are **independent axes** (§6).

---

## 1. Objective + non-goals

**Objective.** Introduce a `PgSession` driver trait covering the entire in-session coupling to `compio_postgres::Client`, make `PostgresBackend<'a, D: PgSession>` (and its collaborators) generic over it, and supply `impl PgSession for compio_postgres::Client` behind a default-on `native-pg` feature. The trait's signatures still name `compio_postgres::{Row, Error, types::ToSql}` (zero-churn choice, §2.2), so the **entire seam** — trait, generic backend, and every `compio_postgres`-naming apply leaf — is gated behind `native-pg`. The default build keeps the concrete compio driver and identical behavior; a `native-pg`-**off** build *omits* the PG apply path entirely (proving it is a removable module — SQLite + IR + render remain) rather than compiling it driverless.

<!-- Revised round 2: the objective no longer claims "compile without any driver impl under --no-default-features". That is falsified by the trait naming compio types + the ungated From<compio_postgres::Error> impls. The honest objective is "generic-over-driver, whole seam gated, PG path cleanly removable". Genericity against a non-compio D is proven by §6d's recording driver (compiled under native-pg), not by a driverless build. -->

**Generic-vs-gated split (the load-bearing distinction).** `PostgresBackend<'a, D: PgSession>` is *generic* (a second driver can be plugged in) but the trait and its sole impl are *feature-gated* (they name compio types). These are not in tension: genericity is a type-system property proven by monomorphizing against the §6d recording driver; gating is a dependency-omission property proven by the `native-pg`-off build. The milestone delivers both, and claims **neither** a driverless compile of the seam nor a second real driver.

**Non-goals (explicitly out of scope for this milestone):**

- **No behavior change.** The native impl forwards each verb one-to-one to today's `Client` methods. The SQL emitted, the txn boundaries, the advisory-lock acquisition, the confinement `SET`s — all identical. The full PG apply suite (DB :5440) is the regression bar and must pass **through** the trait.
- **compio STAYS in the core.** The `compio-postgres` impl keeps it; `native-pg` is default-on. This is not a compio-removal milestone. (A future compio-free core is only reachable *because* the seam exists — §9.)
- **No driverless compile of the seam.** <!-- Revised round 2 --> We do NOT claim `cargo build --no-default-features` compiles the `PgSession` trait or `PostgresBackend`. The trait names `compio_postgres::{Row, Error, ToSql}` and the apply modules carry `impl From<compio_postgres::Error>`; both require the dep. `native-pg`-off *omits* the whole PG seam (the modules are `#[cfg(feature = "native-pg")]`), leaving SQLite + IR + render + guard. Genericity over a non-compio driver is proven separately (§6d), by monomorphizing against an in-crate recording impl under `native-pg`, not by a driverless build.
- **The guard, least-priv role, advisory locks, `statement_timeout`, confinement `SET`s stay in the core.** These are SQL/parse-level (they flow through the trait as SQL strings via `batch_execute`/`execute`); the seam does **not** move them (per the scope fence).
- **No IR / render / SQLite / journal-logic change.** The seam is below render and beside journal-logic; `BindValue`, the IR, the SQLite backend semantics, and journal SQL are untouched.
- **No Node/napi shell, no second driver impl, no compio removal** — FUTURE WORK (§9).
- **The shadow-DB harness stays PG-concrete.** `PgShadow`'s `CREATE DATABASE` throwaway + `connect_with_handle` run-loop lifecycle is compio-specific; it is **not** parameterized this milestone (§4.4). A non-PG driver reports `shadow() == None`, already permitted by the `MigrationBackend` seam.

---

## 2. The `PgSession` trait

### 2.1 Verified coupling surface

The whole coupling to `compio_postgres::Client` across the five target files (`apply/backend/postgres.rs`, `postgres/{online,shadow,backfill}.rs`, `apply/journal.rs`) plus `executor.rs`'s PG leaf functions is **six in-session verbs**, the `Row` return type of the query verbs, and the `ToSql` param element type. Verified call-site counts (from the surface map):

Sites are the **re-grepped** counts across the six shared/PG apply files (`journal.rs`, `backfill.rs`, `executor.rs`, `precondition.rs`, `baseline.rs`, `drift.rs`); the earlier round-2 draft's `~12 query` was **understated** — the true `.query(` count is 27. <!-- Revised round 3: exact grep, not the earlier approximations. -->

| Verb | Concrete `Client` signature | Role | Sites (6 files) |
|---|---|---|---|
| `batch_execute(&self, &str) -> Result<(), Error>` | `client.rs:615` | DDL, txn control (`BEGIN`/`COMMIT`/`ROLLBACK`), multi-stmt session setup (`SET LOCAL …;…`), `RESET ROLE` | ~40 (dominant; incl. shadow/online) |
| `execute<T: ?Sized + ToStatement>(&self, &T, &[&(dyn ToSql + Sync)]) -> Result<u64, Error>` | `client.rs:536` | parameterized DML → rows-affected; advisory lock (`execute` discarding rows) | **26** (journal 13, executor 7, backfill 4, baseline 2) |
| `execute_text_params(&self, &str, &[Option<String>]) -> Result<u64, Error>` | `client.rs:482` | schema-blind op.* DML bind (text-format params, server-inferred types) | 1 (`apply_dml_transactional`, executor.rs:2278) |
| `query<T>(&self, &T, &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Error>` | `client.rs:303` | catalog / journal introspection → all rows | **27** (drift 12, journal 7, backfill 4, executor 2, baseline 1) |
| `query_one<T>(&self, &T, &[…]) -> Result<Row, Error>` | `client.rs:344` | introspection → exactly one row (errors otherwise) | **11** (precondition 4, backfill 3, executor 2, journal 1) |

Two structural facts decide the trait shape:

1. **Transactions are SQL strings, never client methods.** Every `BEGIN`/`COMMIT`/`ROLLBACK` goes through `batch_execute("BEGIN")` etc.; there are **zero** `.transaction()`, `PREPARE TRANSACTION`, or `COMMIT PREPARED` uses in the migrate apply path (grep-verified). `pg_advisory_xact_lock` is issued through `execute` (backfill.rs:782). So the trait needs **no** transaction-object abstraction and **no** advisory-lock method — txn control and locking ride `batch_execute`/`execute` for free.
2. **SQL is always a string, and the trait pins the param to `&str`.** Every call site passes a `&format!(...)` (which is `&String`), a bound `&sql_string` local (also `&String` — e.g. `&obligation_sql` at journal.rs:1202/1212, `&batch_sql` at backfill.rs:833/835, `&stmt` at precondition.rs:331), or a **string literal** (already `&str`); no site passes a prepared `Statement`. The concrete `Client`'s generic `T: ?Sized + ToStatement` accepts `&String` directly today (`String: ToStatement`). Through the trait, the `&str` param requires **`&String`→`&str` deref-coercion at the arg position** at *only the `&String`-passing sites* — literal sites are already `&str` and coerce trivially.
   <!-- Revised round 2: this is a COMPILE-VERIFIED ASSUMPTION, not an asserted fact. Revised round 4 (addressing MINOR): the round-2 RATIONALE ("the arg drives D inference") is technically wrong — D is inferred from the receiver `conn: &D`, never from the SQL arg. Correct the rationale; keep the per-site build-check as belt-and-suspenders, but frame .as_str() as a rare fallback, not the expected outcome. --> **The correct mechanism: `D` is inferred from the *receiver* `conn: &D`, not from the SQL argument.** At every call site — `conn.query(&sql_string, …)`, `conn.execute(&batch_sql, …)`, `conn.query_one(&obligation_sql, …)` — `conn` is the method receiver, so `D` is pinned by `conn`'s type *before* argument type-checking; the SQL argument **never participates in `D` inference**. The trait method's SQL param is a **fixed `&str`** (no type var), so `&String`→`&str` deref-coercion to that fixed param **fires normally and reliably**, exactly as it does today against the concrete `Client`'s `ToStatement`. So the expected outcome at the `&String` sites is: **coerces cleanly, no edit**. **The per-site build-check is retained as belt-and-suspenders, not because a failure is expected** — `.as_str()` is a **rare fallback** for any site the compiler happens to reject (a 1-token, behavior-neutral fix), not the anticipated result at "the `&String` sites." The `&format!(...)`-inside-`batch_execute` sites (the dominant ~40) are the same story a fortiori — `batch_execute`'s only param is the fixed `&str` SQL. A prepared-statement variant is additive future work, not needed here.

   <!-- Revised round 3: the round-2 draft named only 3 param-bearing sites and implied that was exhaustive. Re-grepped: there are far more query/execute sites, most passing literals (which coerce trivially), a minority passing &String (the real .as_str() candidates). State the TRUE total and that step 4 must build every file, not just the 3 named. -->
   **True site inventory (re-grepped across the six shared/PG apply files — `journal.rs`, `backfill.rs`, `executor.rs`, `precondition.rs`, `baseline.rs`, `drift.rs`):**

   | Verb | Total sites (these 6 files) | Per file |
   |---|---|---|
   | `.query(` | **27** | drift 12, journal 7, backfill 4, executor 2, baseline 1, precondition 0 |
   | `.query_one(` | **11** | precondition 4, backfill 3, executor 2, journal 1, drift 0, baseline 0 |
   | `.execute(` | **26** | journal 13, executor 7, backfill 4, baseline 2, drift 0, precondition 0 |

   Of these ~64 SQL-arg sites, the **majority pass a string literal** (verified: all 12 `drift.rs` `.query(` sites pass a `"…"` literal, already `&str`, coerce for free). The `.as_str()` obligation lands only at the **`&String`-passing** sites; the round-2-named three (`journal.rs:1202/1212`, `backfill.rs:833/835`, `precondition.rs:331`) are **examples, not the exhaustive set** — the true `&String` count is whatever the compiler rejects. **Step 4 must build each of the six files individually and add `.as_str()` at every rejected `&String` site**, not just the three named. The named three are the known-likely ones; the count of 27+11+26 is the true search space.

The established Phase-1 grep listed three verbs; `query_one` (backfill.rs:833/835, journal.rs:1517, precondition.rs) and `execute_text_params` (executor.rs:2278) are the fourth and fifth — both load-bearing, both distinct methods on `Client`, both in the trait. `query_one` *could* be a provided default over `query` + take-first; §2.3 keeps it an explicit method for a faithful "exactly one row" error.

### 2.2 The trait

The trait module is **gated behind `native-pg`** (`#[cfg(feature = "native-pg")]` on the module), because its signatures name `compio_postgres::{Row, Error, types::ToSql}` — types from an optional crate. This is a deliberate zero-churn choice (§2.3, §3.1); the neutralized (compio-free) trait surface is deferred to §9. <!-- Revised round 2: was presented as ungated; that contradicted the concrete compio names in the signatures. -->

```rust
// module: #[cfg(feature = "native-pg")]  — the trait itself names compio types
use compio_postgres::{types::ToSql, Row, Error};

/// The in-session Postgres driver surface the migrate apply path is generic over.
///
/// Covers exactly the four in-session verbs (+ a text-param DML variant) the
/// engine issues on a live session. Connect / lifecycle is a per-impl free
/// function (§4.4), NOT part of this trait: the compio impl detaches a
/// run-loop `JoinHandle`; a Node impl has no run-loop. Transaction control,
/// advisory locks, and confinement `SET`s are SQL strings issued through
/// `batch_execute`/`execute` — they are engine logic, not driver methods.
///
/// NOTE: signatures name `compio_postgres::{Row, Error, ToSql}`, so the whole
/// module lives behind `native-pg`. A future compio-free variant (associated
/// `type Error`, a local `SeamRow`, dropped concrete `ToSql`) is §9 work.
#[allow(async_fn_in_trait)] // !Send is by design on the single-thread compio runtime
pub trait PgSession {
    /// DDL / txn control / multi-statement session setup. Simple-query protocol:
    /// one `&str`, may contain `;`-separated statements, no params, no rows.
    async fn batch_execute(&self, sql: &str) -> Result<(), Error>;

    /// Parameterized DML → rows affected.
    async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, Error>;

    /// Schema-blind op.* DML: text-format params with server-inferred types.
    /// (Distinct from `execute`: a concrete-OID binary bind would make PG
    /// refuse `text → timestamptz`; the assembler needs text-format coercion.)
    async fn execute_text_params(&self, sql: &str, params: &[Option<String>]) -> Result<u64, Error>;

    /// Parameterized SELECT → all rows.
    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Error>;

    /// Parameterized SELECT → exactly one row (errors otherwise).
    async fn query_one(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Row, Error>;
}
```

**The `native-pg` impl is a one-line forward per method** — byte-for-byte identical behavior:

```rust
#[cfg(feature = "native-pg")]
impl PgSession for compio_postgres::Client {
    async fn batch_execute(&self, sql: &str) -> Result<(), Error> {
        compio_postgres::Client::batch_execute(self, sql).await
    }
    async fn execute(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64, Error> {
        compio_postgres::Client::execute(self, sql, params).await
    }
    async fn execute_text_params(&self, sql: &str, params: &[Option<String>]) -> Result<u64, Error> {
        compio_postgres::Client::execute_text_params(self, sql, params).await
    }
    async fn query(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>, Error> {
        compio_postgres::Client::query(self, sql, params).await
    }
    async fn query_one(&self, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Row, Error> {
        compio_postgres::Client::query_one(self, sql, params).await
    }
}
```

### 2.3 Design decisions on the trait shape

- **`async fn` in trait under `#[allow(async_fn_in_trait)]` — no proc-macro, no `async-trait` crate.** This mirrors `MigrationBackend` exactly (`apply/backend/mod.rs`, which uses `#[allow(async_fn_in_trait)]` because it is only ever monomorphized). Edition is **2021**, toolchain **Rust ≥ 1.75** (the existing code already relies on native `async fn` in trait — `mod.rs` says so verbatim). `PgSession` sits one layer *below* `MigrationBackend` on the same hot path; it adopts the same async strategy. The lint fires only because such traits are not auto-`Send`-bounded — a non-issue here: **the whole stack is `!Send` by design** on the single-threaded compio runtime (`backfill.rs:907` carries `#[allow(clippy::future_not_send)] // compio single-thread runtime; the stack is !Send by design`). So the trait's futures need not be `Send`, and no boxing is required.
  - *Alternative documented for completeness:* if a future toolchain constraint forbade `async fn` in trait, the fallback is hand-desugared `-> Pin<Box<dyn Future<Output = …> + '_>>` (the pattern already used for the object-safe capability traits `CrossDeployObligations`/`OnlineSchemaChange`/`ShadowDryRun`). That allocates per statement and is **not** adopted here — static-dispatch `async fn` is the house style and the edition allows it.
- **Generic bound (`<D: PgSession>`), static dispatch — NOT object-safe (`&dyn`).** This is consistent with Phase-1's decided posture: `MigrationBackend` is used through static dispatch (`apply_with_lock_backend<B: MigrationBackend>`), *"no boxing, no `dyn`, no `async-trait` allocation on the apply hot path."* `PgSession` mirrors it. Object-safety is impossible for free anyway: (a) every `async fn` would become `Pin<Box<dyn Future>>`, allocating on every DDL statement; (b) `query`/`execute` carry `&[&(dyn ToSql + Sync)]` generic param slices that a single `dyn` representation cannot erase. The capability sub-traits that *do* need `&dyn` (returned as `Option<&dyn OnlineSchemaChange>` etc.) already box their futures and are untouched — `PgSession` does not intersect them.
- **Param type: `&[&(dyn ToSql + Sync)]` verbatim** — byte-identical to the concrete `Client` signature, so **no `ToSql`-annotation churn** at the ~29 param call sites. Because the trait is `native-pg`-gated it may name `compio_postgres::types::ToSql` directly, so the one explicit annotation in the whole surface (`journal.rs:1188`, `let obligation_params: [&(dyn compio_postgres::types::ToSql + Sync); 8] = […]` — grep-verified as the sole explicit `ToSql` site in `src/`) keeps resolving against the **same** `ToSql`. Being inside a `native-pg`-gated apply module, that array is fine — it is *not* an ungated-driverless-build hazard (the whole module is gated). Every other site's `&[&a, &b]` literal coerces implicitly, unchanged. (The `&String`→`&str` SQL-arg coercion is the one compile obligation — §2.1 item 2 — not the param slice.)
- **`Error = compio_postgres::Error` concrete** (not an associated `type Error`) for the native impl's convenience and zero churn. It is already funneled through `From<compio_postgres::Error> for {JournalError (journal.rs:362), BackendError (executor.rs:157), ApplyError (executor.rs:438), RollbackError (executor.rs:2956), DriftError (drift.rs:101)}` plus `#[from]`-derived variants in `conn.rs:18`, `baseline.rs:53`, `precondition.rs:105`, `capability.rs:36`, `role.rs:102` (all grep-verified). **These `From<compio_postgres::Error>` impls are the second reason the whole seam is `native-pg`-gated** <!-- Revised round 2: this is the concrete list the blocker flagged. -->: naming `compio_postgres::Error` in an ungated `impl From` (or a `#[from]` variant) is a compile error when the dep is optional-and-off, exactly like the trait signatures. So each such `impl From` / `Db(#[from] compio_postgres::Error)` variant is `#[cfg(feature = "native-pg")]`. **Note:** several of these live on error enums the SQLite path *also* uses (`JournalError`, `BackfillError`, `DriftError`, `BaselineError`), so the gate is at **variant/impl granularity, not whole-module** — see §4.5's Tier B for why whole-module gating those would wrongly delete types the SQLite backend imports. Neutralizing the error (associated type) is a widening the native-pg-only scope does not need; it is deferred to when the Node impl lands (§9). The `MigrationBackend` boundary one layer up already wraps it in `BackendError`/`ApplyError::Db`.
- **`BindValue` never enters the trait.** `BindValue` (render/step.rs:32: `Null | Bool | Int | Decimal | Text`) is IR-level. **`render/step.rs` carries NO `compio_postgres` name — it only defines the `BindValue` enum** (grep-verified: no `ToSql`, no `compio_postgres` reference in the file), so it needs **no gating and no change**. <!-- Revised round 2: prior drafts erroneously listed render/step.rs as a ToSql/gating site; corrected here and in §5.1/§5.4. --> `BindValue` is lowered to `Vec<Option<String>>` **in `apply_dml_transactional`** (executor.rs:2236) — a dialect-neutral text fold that belongs in the executor, not the driver — and only the already-lowered `&[Option<String>]` crosses into `execute_text_params`. This is the right seam: the IR scalar stays engine-side; the text lowering is what the driver receives. A future Node driver maps `Vec<Option<String>>` to a JS `null | string` array for `pg`'s text-format bind. The only explicit `compio_postgres::types::ToSql` name in the whole crate is the `journal.rs:1188` array (inside a `native-pg`-gated module) — that is the `ToSql` site to account for, not `render/step.rs`.

**Param surface the trait accepts, at a glance:**

| Trait method | Param type | Origin |
|---|---|---|
| `execute` / `query` / `query_one` | `&[&(dyn ToSql + Sync)]` | engine-owned Rust values (incl. the one explicit array at journal.rs:1188) |
| `execute_text_params` | `&[Option<String>]` | `BindValue` fold, done in-executor before the call |
| `batch_execute` | — (SQL string only) | DDL + txn control + `SET LOCAL` + `RESET ROLE` |

---

## 3. The Row / value return type + where decode-to-domain lives

### 3.1 Phase-2 decision: keep `Row = compio_postgres::Row` concrete

`query`/`query_one` return `compio_postgres::Row`, consumed via `.get::<_, T>(name|idx)`, `.try_get::<_, T>(name)`, `.len()`, and `Vec<Row>` iteration. <!-- Revised round 4 (addressing MINOR): the round-3 "~12" was a significant undercount. Re-grep `\.get::<`/`\.try_get::<`/`\.try_get(` across the five Row-consuming files gives 29, not ~12. Correct it; note it's non-load-bearing this milestone (Row stays concrete → zero churn) but the future SeamRow port inherits the true count. --> Re-grepping the three decode patterns (`.get::<`, `.try_get::<`, `.try_get(`) across the five Row-consuming files gives **~29 decode sites**, not the ~12 an earlier draft asserted (drift.rs 20, journal.rs 4, backfill.rs 3, precondition.rs 1, baseline.rs 1). **The count is NOT load-bearing this milestone — `Row` stays concrete (below), so the decode churn is exactly zero regardless of the site count.** It matters only for the *future* §9.1 `SeamRow` port, which inherits these ~29 sites. **For this milestone the trait's `Row` stays the concrete `compio_postgres::Row`.** Rationale:

- Every `query` consumer is a PG-catalog / journal introspection call (drift `snapshot_schema`, journal `applied`/`history`, backfill `resolve_cursor_type`, precondition `run_sql_boolean_in_txn`) that **immediately maps the `Row` into a dialect-neutral owned struct within the same already-PG-coupled function**. `SchemaSnapshot`, `AppliedEntry`, `PendingContract`, `BackfillProgress` do **not** name `Row` — the abstraction boundary already exists one level up, at the owned-struct return.
- Pinning `Row` concrete keeps the ~29 `.get(...)`/`.try_get(...)` decode sites **untouched** — a large churn saving and zero decode-regression risk.
- The native-pg impl is the only impl this milestone; there is no second `Row` to abstract yet.

### 3.2 The value vocabulary crossing the boundary (for the future seam)

To make the eventual second driver tractable, the design records the **complete** decoded scalar vocabulary the current `query` consumers use — verified across `drift.rs`, `journal.rs`, `backfill.rs`, `executor.rs`, `precondition.rs`, `baseline.rs`, `shadow.rs`. It is deliberately narrow — **six kinds, each also in a nullable form**:

```rust
// FUTURE seam value enum (documented now, NOT built this milestone):
pub enum SeamValue {
    Null,
    Text(String),           // String / Option<String> / JSON-text  (dominant)
    Bool(bool),             // indisunique, cycle, rolcanlogin, EXISTS-probes, …
    Int8(i64),              // bigint, count(*)::bigint, exec_ms, event_seq, seq bounds
    Int4(i32),              // character_maximum_length — the ONLY i32; nullable
    Char(i8),               // PG "char": partstrat / relkind / typtype / contype
    TextArray(Vec<String>), // text[]: columns / include / elements / member_of / reloptions
}
```

No `bytea`, no `f64`, no `oid`-typed return, no custom composite `FromSql`. When the Node impl lands, `query` becomes `-> Vec<SeamRow>` where a `SeamRow` exposes `get(name_or_idx) -> &SeamValue` plus `len()`/`is_empty()` (owned values, not the borrowed `FromSql<'a>` — no consumer keeps a borrow past the row, and a napi driver can only return marshalled values). **Out of Phase-2 scope** — but note its status precisely below.

<!-- Revised round 4 (addressing MINOR): SeamValue is NOT optional polish; it is a hard prerequisite for the second (Node) driver, because the concrete `Row` return type is a coupling a napi driver cannot cross. Say so here, not just in the top callout. -->
> **`SeamRow`/`SeamValue` is a PREREQUISITE for the Node driver, not additive polish.** Because `query`/`query_one` return the concrete `compio_postgres::Row` this milestone (§3.1), and a napi driver **cannot construct or return** a compio `Row` (private fields — §6d), the second driver **cannot implement the read verbs at all** until `query`/`query_one` are widened to `-> Vec<SeamRow>`. So this widening is a **hard blocker on the stated end goal** ("a later Node driver drops in"), sequenced **before** the Node impl — not an optional cleanup done afterward. This milestone deliberately ships only the write-path-swappable seam; the read-path swap is gated on this §3.2 work (tracked in §9.1). Recording the exact `SeamValue` vocabulary now keeps that future port mechanical, but does not make it optional.

### 3.3 Where decode-to-domain lives — the core consumer, never the driver

The driver's only job is raw cell → scalar (PG wire → Rust value, today via `compio_postgres`'s `FromSql`). **All domain mapping stays in the core consumer**, unchanged:

- `i8`→`char`→`PartitionSpec.strategy` (`drift.rs`), `relkind`/`typtype`/`contype` label decode;
- `"YES"`/`"NO"` string → bool for `is_nullable`;
- `SafeI64::new(...)` clamping, `serde_json::from_str` for `contract_versions`, `canonical_extension_type()`;
- `String` → `Phase` / `EventKind` enum parses in journal.

None of that moves into the driver. The seam is a **transport** boundary, not a decode boundary — the domain logic in `drift.rs`/`journal.rs` is engine logic and must not migrate into a `PgSession` impl.

### 3.4 Risk spots the future second impl must replicate (recorded now)

These are the two genuine format-coupling points a non-compio driver must reproduce exactly (they impose **no** Phase-2 code change — `Row` stays concrete — but they shape the future `SeamValue` boundary):

1. **PG `"char"` → `i8`** (`drift.rs` partstrat/relkind/typtype/contype). Single-byte `"char"` catalog type, *not* `text`. A `pg`-npm driver gets these as a JS string/number and must reproduce the exact byte semantics (`'r'`/`'l'`/`'h'`, `'p'`/`'f'`/`'u'`/`'c'`/`'x'`, …). Modeled as `SeamValue::Char(i8)`.
2. **`text[]` → ordered `Vec<String>` with NULL-vs-empty distinction** (`array_agg(... ORDER BY ...)` / `WITH ORDINALITY`; drift index/partition columns, `member_of`, `reloptions`). Two subtleties: (a) `array_agg` over an empty set returns SQL **NULL**, so these are `try_get::<Vec<String>>().unwrap_or_default()` / `Option<Vec<String>>` — the driver must distinguish array-NULL from empty-array; (b) **element order is load-bearing** (composite index columns). A `pg`-npm driver gets JS arrays "for free" but must guarantee order + the NULL-vs-empty split.

Two lower-risk notes: `character_maximum_length` is the lone `i32` (keep `SeamValue::Int4` distinct, nullable). `precondition.rs:530` runs **arbitrary user SQL** and asserts "exactly one column, boolean-typed" via `row.len()==1` + a **fallible** `try_get::<bool>(0)` (a non-bool is `NotABooleanSelect`, not a panic) — the future seam must keep `len()` + a fallible positional bool getter, not eagerly stringify. The `_bf_cursor ::text` render (`backfill.rs:182`) is the safe idiom: push type-narrowing into SQL so the value crosses as plain text.

---

## 4. The generic refactor

One generic type param `D` (working name) flows in parallel with the existing borrow lifetime `'a`, defaulted to the native driver so every call site compiles unchanged.

### 4.1 The backend structs

The whole `mod postgres` (and thus `PostgresBackend`/`PgOnline`/`PgShadow`) is `#[cfg(feature = "native-pg")]` (§4.5). Within that gated module the structs are generic over `D`:

```rust
// entire module gated: #[cfg(feature = "native-pg")]
pub struct PostgresBackend<'a, D: PgSession = compio_postgres::Client> {
    conn: &'a D,
    online: online::PgOnline<'a, D>,
    // shadow stays PG-concrete (&compio_postgres::Client — §4.4). It CANNOT be
    // built from a generic `&D`, so it is Option and only Some on the native
    // path — see "The shadow field must be Option" below.
    shadow: Option<shadow::PgShadow<'a>>,
}

pub struct PgOnline<'a, D: PgSession = compio_postgres::Client> {
    conn: &'a D,
}
```

<!-- Revised round 3: the round-2 "shadow field needs no cfg split, and needs no split at all" claim was WRONG. Feature-gating the whole module does NOT make `PgShadow::new(conn: &D)` compile — that is a GENERIC-construction type error, orthogonal to the feature axis. Redesigned the shadow field as Option + split constructors so `PostgresBackend::<'_, RecordingSession>::new` (§6d, the flagship genericity proof) actually compiles. -->
**The shadow field must be `Option<PgShadow<'a>>`, built only on the native path — the round-2 "it's fine" claim was wrong.** The blocker is a *construction* type error, not a feature-omission error, and gating the module does not fix it. Two axes are genuinely orthogonal but the round-2 draft conflated them:

- **Feature axis (`native-pg`):** whether `mod postgres` (and thus `PgShadow`, `PostgresBackend`) is compiled at all. Gating the whole module together is correct for this axis — there is no config where `PostgresBackend` exists but `PgShadow`'s *type* does not.
- **Generic axis (`D`):** even with the module fully present under `native-pg`, `PostgresBackend<'a, D>::new(conn: &'a D)` must *construct* its `shadow` field. `PgShadow::new` takes the **concrete** `admin_conn: &'a compio_postgres::Client` (`shadow.rs:571`) and is deliberately not parameterized (§4.4). When `D = RecordingSession` (the §6d proof), `PgShadow::new(conn)` is a hard type error: `&RecordingSession` is not `&compio_postgres::Client`. **No amount of `#[cfg]` fixes this — the two types differ at monomorphization, inside the gated module.**

`PgOnline` has no such problem: it *is* parameterized (`PgOnline<'a, D>`, `new(conn: &'a D)` — §4.2), so `online: PgOnline::new(conn)` builds for any `D`. Only `PgShadow` — which stays concrete — is unbuildable from a generic `&D`.

**Fix — split construction, `shadow: Option<PgShadow<'a>>`:**

```rust
// entire module gated: #[cfg(feature = "native-pg")]
impl<'a> PostgresBackend<'a, compio_postgres::Client> {
    /// The native path: full backend WITH the PG shadow harness.
    /// D is fixed to the concrete Client here, so `PgShadow::new(conn)` type-checks.
    #[must_use]
    pub fn new(conn: &'a compio_postgres::Client) -> Self {
        Self {
            conn,
            online: online::PgOnline::new(conn),
            shadow: Some(shadow::PgShadow::new(conn)),
        }
    }
}

impl<'a, D: PgSession> PostgresBackend<'a, D> {
    /// Generic path (any driver): no PG shadow harness (shadow() == None).
    /// Used by §6d's RecordingSession proof and any future non-compio driver.
    #[must_use]
    pub fn new_generic(conn: &'a D) -> Self {
        Self {
            conn,
            online: online::PgOnline::new(conn), // PgOnline<'a, D> — generic, builds for any D
            shadow: None,                        // PgShadow is Client-only; absent here
        }
    }
}
```

- The **native `new`** is an inherent impl **specialized to `D = compio_postgres::Client`** (an `impl<'a> PostgresBackend<'a, compio_postgres::Client>` block — Rust allows inherent methods on a concrete monomorphization of a generic type). Inside it, `D` *is* `Client`, so `PgShadow::new(conn)` and `PgOnline::new(conn)` both type-check, and `shadow` is always `Some`. **Every existing call site (`executor.rs:809/3029`, `submit.rs:658`, `runner.rs:861/887/1010/1280`, `shadow.rs:788/950/995`, all tests) calls `PostgresBackend::new(&client)` and keeps compiling unchanged** — the concrete-`Client` inference selects this block, byte-for-byte behavior preserved.
- The **generic `new_generic`** is available for **all** `D: PgSession`; it leaves `shadow = None`. It exists precisely so `PostgresBackend::<'_, RecordingSession>::new_generic(&rec)` (§6d) compiles — proving the backend monomorphizes over a non-compio driver.

The `'a` is only a borrow lifetime; there is no owned driver state. The **default type param `D = compio_postgres::Client`** plus the specialized `new` keep every current call site compiling unchanged and preserve byte-for-byte behavior under `native-pg`.

### 4.2 Exact signatures that change

**Structs / impls (add `<D: PgSession>`, default `= compio_postgres::Client`):**

| Item | File | Change |
|---|---|---|
| `struct PostgresBackend<'a>` | `apply/backend/postgres.rs:26` | → `PostgresBackend<'a, D: PgSession = compio_postgres::Client>` |
| `impl MigrationBackend for PostgresBackend<'_>` | `apply/backend/postgres.rs` | → `impl<D: PgSession> MigrationBackend for PostgresBackend<'_, D>` |
| `PostgresBackend::new` | `apply/backend/postgres.rs:32-42` | **SPLIT** (§4.1): native `new` stays `impl<'a> PostgresBackend<'a, Client>` taking `conn: &'a Client` (shadow=`Some`); add generic `new_generic` on `impl<'a, D: PgSession>` taking `conn: &'a D` (shadow=`None`). The native `new` cannot be generic — it constructs the concrete `PgShadow`. |
| `shadow: PgShadow<'a>` field + `shadow()` accessor | `apply/backend/postgres.rs:29,272` | field → `Option<PgShadow<'a>>`; accessor → `self.shadow.as_ref().map(\|s\| s as &dyn ShadowDryRun)` (§4.3) |
| `reset_role_best_effort` (`self.conn.batch_execute("RESET ROLE")`) | `apply/backend/postgres.rs:72` | body unchanged (calls trait method) |
| `struct PgOnline<'a>` | `postgres/online.rs:131` | → `PgOnline<'a, D: PgSession = …>` |
| `impl OnlineSchemaChange for PgOnline<'_>` | `postgres/online.rs` | → `impl<D: PgSession> OnlineSchemaChange for PgOnline<'_, D>` |
| `struct PgShadow<'a>` | `postgres/shadow.rs:565` | **UNCHANGED — stays concrete** (§4.4) |

**Free functions (add `<D: PgSession>`, change `conn: &Client` → `conn: &D`; bodies unchanged — every call is already `conn.batch_execute/execute/query`):**

| Module | Functions |
|---|---|
| `postgres/online.rs` | `run_expand_pg` (`conn: &compio_postgres::Client` at online.rs:79 → `conn: &D`; its body only forwards `conn` to `executor::apply_with_lock` + `run_backfill`, both generic — verified) and any `pg`-leg sibling |
| `apply/journal.rs` | `ensure_journal`, `applied`, `net_rolled_back`, `history`, `record_started`, `record_completed`, `record_rolled_back`, `clear_inflight`, `record_pending_contract_with_recovery`, `resolve_pending_contract`, `outstanding_pending_contracts`, `mark_deploy_recovery_committed[_batch]`, `mark_deploy_recovery_reconciled`, `outstanding_deploy_recoveries`, `superseded_versions`, `latest_completed_checksums`, `applied_count`, `record_baseline[_inner]`, and every other `conn: &Client` fn |
| `postgres/backfill.rs` | `run_backfill` and its leaves: `backfill_progress`, `list_backfills`, `resolve_cursor_type`, `validate_cursor_column`, `run_batch`, `mark_complete`, the advisory-lock + `SET LOCAL` + txn-control block |
| `apply/executor.rs` (PG leaf fns) | `apply_transactional`, `apply_dml_transactional`, `configure_session_non_txn`, `snapshot_session`, `restore_session`, `acquire_project_lock`, the `apply_with_lock` / rollback entries that take `&Client`, and the `pg::…` introspection helpers |
| `apply/precondition.rs` | `table_exists`, `column_exists`, `row_count`, `run_sql_boolean_in_txn` (`conn: &Client` → `&D`) |
| `apply/baseline.rs` | `first_baseline_version` (and any `conn: &Client` sibling) |
| `apply/drift.rs` | `snapshot_schema` and its per-object introspection helpers (`conn: &Client` → `&D`) |

Recommendation: use `<D: PgSession>` (not `<D: PgSession + ?Sized>`) — static dispatch is the house style; no site needs a `&dyn`.

**Owned return structs are already dialect-neutral** (`SchemaSnapshot`, `AppliedEntry`, `PendingContract`, `BackfillProgress`, …) — they do not name `Row`, so nothing downstream of the trait changes.

### 4.3 The `MigrationBackend` impl body — unchanged except the `shadow()` accessor

Every `PostgresBackend` method already calls `crate::apply::executor::pg::…(self.conn, …)` / `journal::…(self.conn, …)`. Those free functions become generic (§4.2), so **all `self.conn`-forwarding method bodies are untouched** — the generic param threads mechanically. `online()` (the *accessor*) is likewise untouched: `online: PgOnline<'a, D>` is genuinely generic, its `impl<D: PgSession> OnlineSchemaChange for PgOnline<'_, D>` holds for every `D`, so `Some(&self.online) as Option<&dyn OnlineSchemaChange>` (postgres.rs:268) is correct for any `D`. `PostgresBackend<'_, D>` is still handed to the generic executor (`apply_with_lock_backend<B: MigrationBackend>`), which does not care that `B` is itself generic over `D`.

<!-- Revised round 4 (addressing MINOR): "online() is unchanged" is precise for the ACCESSOR, but the OnlineSchemaChange impl BODY in online.rs is not literally zero-edit. run_online returns Pin<Box<dyn Future + 'a>> (capability.rs:111-124); its impl `Box::pin(run_expand_pg(self.conn, …))` (online.rs:162) now captures a generic `&'a D` where it captured `&'a Client`. Compile-fine (no Send bound, matching the !Send stack), but a real edit to the async/boxed body's captured type. -->
> **Nuance — the `OnlineSchemaChange` impl *body* in `online.rs` is not literally zero-change (the accessor is; the impl body has one real, trivial edit).** `run_online` returns `Pin<Box<dyn Future<Output = …> + 'a>>` (`capability.rs:111–124`), and `PgOnline`'s impl is `Box::pin(run_expand_pg(self.conn, …))` (`online.rs:144–174`). After parameterizing `PgOnline<'a, D>` and making `run_expand_pg` generic (§4.2), the boxed future **captures `&'a D` where it captured `&'a compio_postgres::Client`**. This is **compile-fine** — the boxed `dyn Future` carries **no `Send` bound** (`+ 'a` only), which matches the `!Send`-by-design stack, so no boxing/`Send`-shape change is needed — but it is a **real (if mechanically trivial) change to the captured type inside the impl body**, not literally a no-op. The framing "`online()` is unchanged" is exact for the **accessor** at postgres.rs:268; the **impl at online.rs:144** threads `<D>` and its `Box::pin(run_expand_pg(self.conn, …))` captures `&D` — no code-shape edit beyond the generic param, no `Send`/boxing consequence.

<!-- Revised round 3: the round-2 claim "the MigrationBackend impl body is unchanged" was false for shadow(). Once the shadow FIELD is Option<PgShadow> (§4.1, the blocker fix), the shadow() accessor CANNOT return `Some(&self.shadow)` — that no longer type-checks. Specify the exact new body. -->
**The one accessor that changes is `shadow()`** (postgres.rs:272). Because the field became `Option<PgShadow<'a>>` (§4.1), the body changes from:

```rust
fn shadow(&self) -> Option<&dyn ShadowDryRun> {
    Some(&self.shadow)                              // OLD — field was PgShadow<'a>
}
```

to:

```rust
fn shadow(&self) -> Option<&dyn ShadowDryRun> {
    self.shadow.as_ref().map(|s| s as &dyn ShadowDryRun)   // NEW — field is Option<PgShadow>
}
```

This is **behavior-preserving on the native path**: the specialized `new` (§4.1) always sets `shadow = Some(PgShadow::new(conn))`, so `shadow()` returns `Some(&dyn ShadowDryRun)` exactly as before — the entire shadow/dry-run suite (§6b) passes unchanged. It is **correct on the generic path**: `new_generic` sets `shadow = None`, so a non-native `D` (the §6d `RecordingSession`, a future Node driver) reports `shadow() == None` — consistent with §4.4's "a non-PG driver reports `shadow() == None`" (the `ShadowDryRun` capability is already `Option` on the `MigrationBackend` seam, so a `None` is fully legal). This is the *same* root cause as the §4.1 blocker surfacing in the trait impl — the field's presence now depends on the constructor, so the accessor must consult the `Option`.

### 4.4 `PgShadow` stays PG-concrete (blast-radius confinement)

The shadow path is a self-contained PG-only harness: it `CREATE DATABASE`s a throwaway (`shadow.rs:330/393/464`), `connect_with_handle`s a **second** concrete `compio_postgres::Client` + its compio run-loop `JoinHandle` (`shadow.rs:660`), then `close_shadow_session` does `drop(shadow); run_loop.cancel().await` (`shadow.rs:1047`). This lifecycle is compio-specific and has no Node analogue. **`PgShadow` is therefore NOT parameterized** — it keeps its concrete `admin_conn: &compio_postgres::Client`. A non-PG driver simply returns `shadow() == None` (already permitted by the `MigrationBackend` seam — the `ShadowDryRun` capability is `Option`). This confines the generic cascade to `postgres.rs`, `online.rs`, `backfill.rs`, `journal.rs`, `executor.rs` (PG leaves), `precondition.rs`, `baseline.rs`, `drift.rs`; **`shadow.rs` stays concrete**.

---

### 4.5 The `native-pg` module-gating map (the cfg strategy, explicitly)

<!-- Added round 2: the blocker required an explicit cfg strategy for every module that names compio ungated today. This is that map. -->

Today (grep-verified) the following are **ungated** and each names `compio_postgres` in a way that fails to compile once `compio-postgres` is optional-and-off:

- `use compio_postgres::Client;` in: `apply/backend/postgres.rs:7`, `apply/journal.rs:33`, `apply/executor.rs:39`, `apply/drift.rs:36`, `apply/precondition.rs:65`, `apply/baseline.rs:31`, `apply/backend/postgres/shadow.rs:75`, `apply/backend/postgres/backfill.rs:58`, `apply/role.rs:91`, `engine.rs:26`, `test_support.rs:8`, plus `ops/submit.rs`/`ops/status.rs`.
- `impl From<compio_postgres::Error>` / `#[from] compio_postgres::Error` in: `journal.rs:362`, `executor.rs:{157,438,2956}`, `drift.rs:101`, `baseline.rs:53`, `precondition.rs:105`, `capability.rs:36`, `role.rs:102`, `conn.rs:18`.
- the explicit `ToSql` array at `journal.rs:1188`.
- `mod postgres;` itself is **unconditional** in `apply/backend/mod.rs:42` (only `mod mysql` is `zsv8`-gated) — verified.

The cfg strategy is **two-tier**, because some compio-naming lives in *pure-PG* modules (safe to gate wholesale) and some lives in *shared* modules the SQLite path also imports (must gate at item/variant level). <!-- Revised round 2: the earlier "gate whole modules" blanket was WRONG for journal.rs/capability.rs — grep proved they define types (JournalError, AppliedEntry, BackfillError) that the SQLite backend imports. Whole-gating them would delete types SQLite needs. Corrected below. -->

**Tier A — whole-module gating** (pure-PG, no SQLite consumer):

| Module | Gating | Rationale |
|---|---|---|
| `mod postgres` (`apply/backend/mod.rs:42`) → `#[cfg(feature = "native-pg")]` | whole module | carries `PostgresBackend`, `PgOnline`, `PgShadow`, `backfill` — all name compio; mirrors the existing `#[cfg(feature = "zsv8")] mod mysql` next to it |
| the `PgSession` trait module (new `src/apply/backend/postgres/session.rs`) + `impl PgSession for compio_postgres::Client` | whole module | signatures name `compio_postgres::{Row, Error, ToSql}` |
| `apply/role.rs` | whole module | least-priv role provisioning is PG-only (`compio_postgres::{Client, Error}`, no SQLite consumer — verified) |
| `conn.rs`'s `connect`/`connect_with_handle`/`ConnectError` | the connect fns + `ConnectError` | PG-only; `migrated` calls its *own* connect, not this one (§5.4). **Every in-crate caller is PG-path — see the caller audit below.** |

<!-- Added round 3: the minor flaw asked to grep-verify that no ungated (native-pg-off-reachable) code path calls crate::conn::connect. Enumerate the callers and confirm each rides the native-pg gate. -->
> **`crate::conn::connect` / `connect_with_handle` caller audit (grep-verified).** Gating the connect fns is only safe if **every** in-crate caller is itself inside a `native-pg`-gated region (or is `migrated`, which uses its own connect — §5.4). The callers are:
> - `command/runner.rs` (`connect(&cfg.database_url)` at :850/876/917/1008/1263/1552/1877/2116/2283) — **PG-path**: each connect is immediately followed by `PostgresBackend::new(&conn)` (e.g. runner.rs:861), and `runner.rs` imports `PostgresBackend` ungated at line 21. `runner.rs` also uses `SqliteBackend` (line 22) — it is a **mixed** module. **Resolution:** gate the `use …PostgresBackend`, the `connect`-calling PG apply functions, and their PG error arms behind `native-pg` (Tier-B item-level, like the other shared modules), keeping the `SqliteBackend` CLI leg ungated. Add `command/runner.rs` to the Tier-B list (§4.5, below).
> - `frontend/generate.rs:121` (`conn::connect` → `snapshot_schema(&client, …)`) — **PG live-introspection**, and `mod frontend` is `#[cfg(feature = "zsv8")]`-gated (not `native-pg`). The config `zsv8`-on + `native-pg`-off would reach this ungated-w.r.t.-`native-pg` call. **Resolution:** the live-introspection entry in `frontend/generate.rs` additionally requires `native-pg` (gate the `connect`+`snapshot_schema` block with `#[cfg(all(feature = "zsv8", feature = "native-pg"))]`, or make the crate's `zsv8` feature imply `native-pg` if the JS authoring front-end always needs live PG introspection — decide at impl step 6 and let step 7's `--no-default-features --features zsv8` build confirm).
> - `apply/backend/postgres/shadow.rs:660` (`connect_with_handle`) — inside `mod postgres` (Tier A, whole-module `native-pg`-gated). Safe.
> - the former JS authoring CLI source at line 451 — a `zsv8`/`standalone-cli` binary; its apply path is PG, so it resolves `native-pg` via its feature set (like `migrated`, §5.4).
> - `#[cfg(test)]` sites (`ops/submit.rs:758`, `backfill.rs:920`, `executor.rs:3877`, `render/expand_contract.rs:1066`, `test_support.rs:95`) — test-only; they run under `native-pg`-on (default) and are addressed by the test-support gating note (§6 / step 7's `--no-default-features --tests` build).
>
> **Conclusion:** no *ungated* (native-pg-off-reachable) production code path calls `connect`/`connect_with_handle` **once `command/runner.rs`'s PG functions and `frontend/generate.rs`'s live-introspection block are gated** (added to the Tier-B / feature-implication list). The `--no-default-features` build (step 7) is the catcher if any connect caller was missed.

**Tier B — item/variant-level gating** (SHARED modules the SQLite path imports — gate only the compio-naming items, keep the dialect-neutral types):

| Location | Gate ONLY | Keep ungated (SQLite needs it) |
|---|---|---|
| `apply/journal.rs` | `use compio_postgres::Client` (l.33), `impl From<compio_postgres::Error> for JournalError` (l.362), the `ToSql` array (l.1188), and every `conn: &Client`/`conn: &D` PG journal free fn | `enum JournalError` (l.332) and its non-compio variants, `AppliedEntry`, `Phase`/`EventKind` — imported by `sqlite/mod.rs` (9 sites, verified) |
| `apply/backend/capability.rs` | the **variant** `#[cfg(feature = "native-pg")] Db(#[from] compio_postgres::Error)` on `BackfillError` (l.36) | `enum BackfillError` itself + `InvalidIdentifier`/`InvalidBatchSize`/`Journal`/`Fault`/`CursorNotUniqueNotNull`/… — constructed by `sqlite/backfill_sql.rs` (verified) |
| `apply/executor.rs` | the `pub(crate) mod pg` submodule (l.57), `use compio_postgres::Client` (l.39), and the four `impl From<compio_postgres::Error>` (l.157/438/2956 + the `BackendError` one) — the last as gated variants where the enum is shared | `ExecutorConfig`, `PgConfinement`, the confinement SQL-string builders, the dialect-neutral executor spine, `BackendError`/`ApplyError`/`RollbackError` enums minus their compio `From` |
| `apply/drift.rs` | `use compio_postgres::Client` (l.36), `impl From<compio_postgres::Error> for DriftError` (l.101), the PG per-object introspection fns (`snapshot_schema` PG leg + helpers) | `enum DriftError` + `DriftError::Backend`, `SchemaSnapshot`, `check_checksum_drift`, `diff_snapshots`, `compare_applied_to_set` — **all imported by `sqlite/drift_sql.rs` + `sqlite/mod.rs` (verified)**; the SQLite path has its own `snapshot_schema` and reuses the neutral diff helpers |
| `apply/baseline.rs`, `apply/precondition.rs` | `use compio_postgres::Client`, the `Db(#[from] compio_postgres::Error)` variant (baseline.rs:53 / precondition.rs:105), the `conn: &D` PG fns | `enum BaselineError`+`BaselineOutcome`+`BaselineError::Backend` (imported by `sqlite/mod.rs:32/779/788` — verified), and `PreconditionError` if the SQLite path imports it |
| `command/runner.rs` | the `use …PostgresBackend` (l.21), each `connect(&…)` → `PostgresBackend::new(&conn)` PG apply fn (l.850–2283), and their PG error arms | the `use …SqliteBackend` (l.22) + the whole SQLite CLI leg (`open_sqlite_backend`, `apply_sqlite_*`) — SQLite path stays ungated |
| `frontend/generate.rs` (already `zsv8`-gated) | the `conn::connect` + `snapshot_schema` live-introspection block → `#[cfg(all(feature = "zsv8", feature = "native-pg"))]` (or make `zsv8` imply `native-pg`) | the IR eval / desired-snapshot build (no compio name) |
| `engine.rs`, `test_support.rs`, `ops/submit.rs`, `ops/status.rs` | the `compio_postgres::Client`-naming fns/blocks | the dialect-neutral engine/ops surface |

**The `lib.rs` re-exports** of `PostgresBackend`, `PgOnline`, `PgShadow`, `run_backfill`, `BackfillProgress`, `PgSessionSnapshot`, … (lib.rs:117–137) get `#[cfg(feature = "native-pg")]` per PG re-export; the dialect-neutral re-exports (`JournalError`, `AppliedEntry`, `MigrationBackend`, `SqliteBackend`, `BackfillError`, render) stay ungated so `plugin-db` (which consumes only those — verified) is unaffected.

<!-- Revised round 4 (addressing MAJOR): the primary PG re-export is ONE combined `pub use apply::backend::{…}` statement (lib.rs:117-121) that MIXES PG-only symbols (must gate) with dialect-neutral symbols (must stay ungated). You cannot put `#[cfg]` on individual items inside a single `use` group — the statement must be SPLIT. State the exact split. -->
> **Mechanical warning — the primary PG re-export is one *combined* `pub use`; it must be SPLIT, not per-item-`cfg`'d.** `#[cfg(...)]` attaches to a whole `use` **statement**, never to individual items inside a `{…}` group. The verified re-export at **lib.rs:117–121** is a **single combined statement** that mixes PG-only symbols with dialect-neutral ones the SQLite path needs:
> ```rust
> // lib.rs:117-121 TODAY — ONE statement, PG-only + neutral symbols mixed:
> pub use apply::backend::{
>     BackfillError, BackfillOutcome, CrossDeployObligations, DryRunError, DryRunReport,
>     MigrationBackend, MigrationResult, OnlineSchemaChange, PgSessionSnapshot, PostgresBackend,
>     SeedError, ShadowConfig, ShadowDryRun,
> };
> ```
> A mechanical implementer who writes `#[cfg(feature = "native-pg")]` above this statement would wrongly gate `MigrationBackend`/`BackfillError`/`ShadowDryRun`/… out of the SQLite path (compile error under `native-pg`-off, since `sqlite/mod.rs` and `plugin-db` import them). The statement must be **surgically split into two**:
> ```rust
> // GATED — PG-only (only PG consumers): PostgresBackend + PgSessionSnapshot.
> #[cfg(feature = "native-pg")]
> pub use apply::backend::{PgSessionSnapshot, PostgresBackend};
> // UNGATED — dialect-neutral (SQLite path + plugin-db import these):
> pub use apply::backend::{
>     BackfillError, BackfillOutcome, CrossDeployObligations, DryRunError, DryRunReport,
>     MigrationBackend, MigrationResult, OnlineSchemaChange, SeedError, ShadowConfig, ShadowDryRun,
> };
> ```
> (`CrossDeployObligations`/`OnlineSchemaChange`/`ShadowDryRun`/`ShadowConfig`/`DryRunReport`/`DryRunError`/`SeedError`/`MigrationResult` are the capability *traits*/neutral types — NOT PG-only; they stay ungated.) The **separately-stated** PG re-exports each already stand alone as their own `pub use` statement and take a whole-statement `#[cfg]` cleanly (no splitting needed):
> - `pub use apply::backend::postgres::online::PgOnline;` (lib.rs:131) → prefix `#[cfg(feature = "native-pg")]`
> - `pub use apply::backend::postgres::shadow::PgShadow;` (lib.rs:132) → prefix `#[cfg(feature = "native-pg")]`
> - `pub use apply::backend::postgres::backfill::{ backfill_progress, ensure_backfill_progress, list_backfills, run_backfill, run_backfill_bounded, BackfillProgress };` (lib.rs:134–137) → prefix `#[cfg(feature = "native-pg")]` (all six names are PG-path — `backfill.rs` is inside `mod postgres`).
> - `pub use apply::baseline::{BaselineError, BaselineOutcome};` (lib.rs:133) — **stays UNGATED** (`sqlite/mod.rs` imports `BaselineError`/`BaselineOutcome` — verified §4.5 Tier B; the enum + `Backend` variant are neutral, only the compio `Db` variant is gated *inside* baseline.rs, not at the re-export).
>
> So: **one split** (lib.rs:117–121) + **three prefix-only whole-statement gates** (online re-export at 131, shadow at 132, backfill block at 134–137), and baseline's re-export (133) left ungated.

<!-- Revised round 3: the re-export gate RULE was imprecise. PgSessionSnapshot (lib.rs:119) names NO compio type — it is a pure-String struct (mod.rs:80). It is gated because its only CONSUMERS are PG, not because it names compio. A reviewer applying "gate compio-naming symbols" would wrongly leave it ungated. State the rule as consumer-based. -->
> **The re-export gate rule is *consumer-based*, not *name-based*.** The rule is **"gate PG-only symbols"** (symbols the SQLite / dialect-neutral surface never uses), **not** the narrower "gate symbols that name `compio_postgres`." `PgSessionSnapshot` (mod.rs:80) is the illustrative case: it is a **pure-`String` struct** — `statement_timeout` / `lock_timeout` / `search_path` GUC text — and names **no compio type**, yet its only consumers (`snapshot_session`/`restore_session`, executor.rs:481, the `pg::` leaves) are PG-path. So its re-export is `native-pg`-gated on a **consumer** basis. Applying the name-based rule alone would wrongly leave it ungated (and then `native-pg`-off would re-export a symbol whose *type definition* survives — harmless for the type itself, but the symbol is dead under PG-omission and the intent is that the whole PG surface disappears). The type `PgSessionSnapshot` *itself* (the `struct` at mod.rs:80) is compio-name-free and can stay defined ungated (it is `MigrationBackend::SessionSnapshot`'s value and the SQLite backend has its own associated type); only its **PG-specific producers and its `lib.rs` re-export** ride the gate.

**Why Tier B matters (the trap):** `enum BackfillError` (capability.rs) and `enum JournalError` (journal.rs) are **shared** — the SQLite backfill/journal path constructs their non-compio variants and imports the enums. Gating the whole module would delete `BackfillError`/`JournalError`/`AppliedEntry` from under the SQLite path and break the `native-pg`-off build *for a different reason*. So the compio-naming **variant** (`Db(#[from] compio_postgres::Error)`) is `#[cfg(feature = "native-pg")]`, and the enum + its dialect-neutral variants stay ungated. Step 7's `--no-default-features` build is the catcher for getting this split wrong in either direction (too much gated → SQLite path loses a type; too little → a compio name survives ungated).

**The `native-pg`-off build compiles because** every compio-naming *item* (module in Tier A, variant/fn/impl in Tier B) ceases to exist, while the shared dialect-neutral enums and the whole SQLite backend remain. This is the honest replacement for the retracted "driverless seam compiles" claim: the PG path is *removable* at item granularity, and removing it leaves a coherent V8-free-and-PG-free SQLite core. `mod sqlite` (`apply/backend/mod.rs:43`) is unconditional and driver-independent.

---

## 5. The `native-pg` feature

### 5.1 What it gates

**Gated behind `native-pg` (default-on)** — see §4.5 for the exact module map:
- the `compio-postgres` **dependency** (`Cargo.toml`), flipped to `optional = true`;
- the **`PgSession` trait** *and* **`impl PgSession for compio_postgres::Client`** (§2.2) — both name compio types;
- the whole **`mod postgres`** (Tier A): `PostgresBackend<'a, D>`, `PgOnline<'a, D>`, `PgShadow`, `backfill`; plus `role.rs` and `conn.rs`'s connect path;
- **item/variant-level** (Tier B — §4.5): in the SHARED modules `journal.rs`, `capability.rs`, `executor.rs`, `drift.rs`, `baseline.rs`, `precondition.rs`, `command/runner.rs`, `frontend/generate.rs`, `test_support.rs`, gate only the `compio_postgres`-naming items (and, for `runner.rs`, the `PostgresBackend`-using PG connect/apply fns — keeping the SQLite CLI leg ungated) — the `use compio_postgres::Client`, the `impl From<compio_postgres::Error>` / `Db(#[from] compio_postgres::Error)` variants, the `pg::…` submodule, the `journal.rs:1188` `ToSql` array, the PG introspection fns, and the `conn: &D` PG fns — while keeping the dialect-neutral enums + helpers (`JournalError`, `BackfillError`, `DriftError`, `SchemaSnapshot`, `AppliedEntry`, `check_checksum_drift`, …) the SQLite path imports;
- **`conn.rs`'s connect path** — `connect` / `connect_with_handle` (the `compio_postgres::connect(dsn, NoTls)` + `compio::runtime::spawn(connection.run())` run-loop + `.detach()`, `conn.rs:403–435`) and `ConnectError::Connect(#[from] compio_postgres::Error)` (conn.rs:18);
- the shadow harness's second connect (`connect_with_handle` at `shadow.rs:660`) and `PgShadow` (names `compio_postgres::Client` concretely — §4.4);
- least-priv role provisioning `apply/role.rs` (names `compio_postgres::{Client, Error}`, and is PG-only);
- the `native-pg`-gated `lib.rs` re-exports of the above PG types (§4.5).

<!-- Revised round 2: (a) render/step.rs REMOVED from this list — it carries no compio name (grep-verified); (b) the PgSession trait + PostgresBackend moved from "ungated" to "gated" — they name compio types. -->

**Ungated (stays in the core, always compiled):**
- **`render/step.rs`** — defines `BindValue` only; **no `compio_postgres` name, no gating** (grep-verified). The `BindValue`→`Vec<Option<String>>` fold lives in `executor.rs:2236`, dialect-neutral;
- `mod sqlite` (the whole SQLite backend) — driver-independent, unconditional;
- `ExecutorConfig`, `PgConfinement`, and every confinement **SQL-string builder** (`search_path_clause`, `statement_timeout_ms`, `lock_timeout_ms`, the `SET LOCAL …`/`RESET ROLE`/`current_setting`/`set_config` emitters) — pure config + SQL, no compio type;
- the guard (`pg_query` deny-list), advisory-lock SQL — SQL/parse-level, no compio type, per the scope fence. (Note: `apply/role.rs` *does* name `compio_postgres::Client`, so despite being "SQL/parse-level in spirit" its **compio-naming fns are gated** — the SQL strings it builds are pure, but the fn that runs them takes a driver ref. §4.5 gates the fns, not the SQL builders.)

**Crucial:** connect does **no** session setup. `connect()` returns a bare `Client`; all `SET ROLE` / `statement_timeout` / `lock_timeout` / `search_path` / `RESET ROLE` are emitted **later, per-migration, by the executor as SQL strings** through `batch_execute`/`query`. So the confinement **SQL and logic** are 100% core (dialect-neutral string builders, ungated) and flow through the trait. The nuance <!-- Revised round 2 -->: the *functions that run* that SQL take a driver ref (`&D`/`&Client`) and live inside `native-pg`-gated apply modules (e.g. `role.rs` names `compio_postgres::Client`), so they ride the gate. Only the confinement **string builders** (`search_path_clause`, `statement_timeout_ms`, …) are genuinely ungated — the socket-open + run-loop + `NoTls` + `compio_postgres::Error` + the driver-ref-taking runners are driver-specific and gated.

### 5.2 Feature declaration

```toml
[features]
# ... existing zsv8 / js-cli / standalone-cli ...
default = ["js-cli", "native-pg"]   # native-pg added — default behavior unchanged

# The compio-postgres driver: the default (and today only) PgSession impl.
# Gates the compio-postgres dep AND the entire PG seam: the PgSession trait,
# `impl PgSession for compio_postgres::Client`, `mod postgres` (PostgresBackend/
# PgOnline/PgShadow/backfill), the PG-apply leaves (journal/drift/baseline/
# precondition + executor pg:: leaves + their From<compio_postgres::Error> impls),
# conn.rs connect, role.rs, and the PG lib re-exports. The trait names compio
# types, so it CANNOT be ungated. native-pg-OFF omits the whole PG path (SQLite +
# IR + render remain) — that omission (not a driverless compile) is what §6a proves.
native-pg = ["dep:compio-postgres"]

[dependencies]
compio-postgres = { workspace = true, optional = true }
```

`native-pg` is **default-on**, so the current default build and default `cargo test` are byte-for-byte unchanged. (`compio` itself likely stays unconditional — it backs `#[compio::main]`/tests and the SQLite `dump_sql` event loop; only the `compio_postgres` types are the seam.)

### 5.3 Interaction with `zsv8` — independent axes

`native-pg` (driver transport) and `zsv8` (V8 host: JS authoring front-end + live-MySQL isolate + recorder child) are **orthogonal**. Neither implies the other:

| Build | `zsv8` | `native-pg` | Result |
|---|---|---|---|
| `default` (`js-cli`) | on | on | today's full behavior — JS authoring + PG/SQLite/MySQL apply |
| `--no-default-features` | off | off | **PG-omitted core**: SQLite backend + IR + render + guard; the `PgSession` trait, `PostgresBackend`, and all PG apply leaves are absent (not present-but-driverless). No V8. <!-- Revised round 2: was "trait + generic backend, no driver impl" — falsified; the seam is omitted, not compiled driverless. --> |
| `--no-default-features --features native-pg` | off | on | V8-free PG apply core with the compio driver (what `migrated` resolves to; `plugin-db` does NOT enable this and uses only the SQLite path) |
| `--no-default-features --features zsv8` | on | off | V8 host without the PG apply path (authoring/render/SQLite only) |

The default set is now `["js-cli", "native-pg"]`; the two flags are separate lines. This mirrors §9.2 of the Phase-1 doc, which flagged the compio DB-I/O seam as an *independent axis* from the V8 gate — this milestone builds exactly that independent axis.

### 5.4 Downstream dependents

- **`migrated`** (`default-features = false` on the migrate edge, Cargo.toml:30) is the one true external consumer of the migrate PG seam. <!-- Revised round 2: migrated does NOT call zeroship_migrate::connect — verified. --> It does its **own** connect via its **own direct `compio-postgres` dep** (`compio_postgres::connect(&dsn, NoTls)` at `main.rs:193`, `migration_store.rs:190`, `policy_store.rs:150`), then constructs `zeroship_migrate::PostgresBackend::new(&conn)` (`apply.rs:298`) and calls `conn.batch_execute(...)` on the client. So it needs two things: (1) `native-pg` **explicitly enabled** on its `zeroship-migrate` edge — **add `native-pg` to `migrated`'s `zeroship-migrate` feature list** — so `impl PgSession for compio_postgres::Client` and `PostgresBackend` are in scope; and (2) the default type param `D = compio_postgres::Client` so `PostgresBackend::new(&conn)` compiles with the concrete `Client` unchanged.
  - **Two distinct `migrated` sites depend on the default type param — not just `::new`.** <!-- Revised round 4 (addressing MAJOR): the round-3 audit analyzed only apply.rs:298 (::new); apply.rs:619 uses PostgresBackend<'_> as an EXPLICIT type annotation in a fn signature — a separate site relying on the default filling D in a reference position. -->
    - **Site (a) — `PostgresBackend::new(&conn)` (apply.rs:298):** the concrete-`Client` argument drives inference, selecting the specialized `impl<'a> PostgresBackend<'a, compio_postgres::Client>::new` (§4.1); `D` is fixed to `Client` by the arg. Compiles unchanged.
    - **Site (b) — `backend: &PostgresBackend<'_>` (apply.rs:619, `preflight_ir_documents`'s parameter):** this is an **explicit type annotation in a function signature**, not a `::new` call. After `PostgresBackend<'a, D: PgSession = compio_postgres::Client>`, the written `PostgresBackend<'_>` elides **only** the lifetime and relies on the **default type param** filling the `D` slot in a *reference-type* position. Current Rust *does* elaborate `PostgresBackend<'_>` to `PostgresBackend<'_, compio_postgres::Client>` via the default here, but this is a **distinct, independently-verified site** from `::new` — default-type-param elaboration in non-`::new` positions (annotations, trait bounds, where-clauses) has historically had edge cases. **Step 8 (`cargo build -p zeroship-migrate-server --features …native-pg`) must confirm apply.rs:619 elaborates against the default.** If it does not, the fix is trivial and in-scope: `migrated` writes the `D` explicitly — `backend: &PostgresBackend<'_, compio_postgres::Client>` (one-token annotation, `compio_postgres` already in `migrated`'s scope via its own direct dep). Re-grep `migrated/src` for any further `PostgresBackend<'_>`/`PostgresBackend<` annotation sites (there are exactly these two today — verified) and apply the same explicit-`D` fix to any the build rejects.
  - **Version-identity invariant (was under-specified).** <!-- Revised round 2: the minor flaw. --> `migrated`'s own `compio-postgres` (`{ workspace = true, features = [...] }`, Cargo.toml:15) and migrate's now-optional `compio-postgres` (`{ workspace = true, optional = true }`) resolve through the **same** `[workspace.dependencies] compio-postgres = { path = "libs/compio-postgres" }` edge (root Cargo.toml:193, verified), so they are the **same crate instance** — `migrated`'s `Client` is exactly the type migrate's gated `impl PgSession for compio_postgres::Client` targets. **Checked invariant:** `cargo tree -p zeroship-migrate-server | grep compio-postgres` must show a **single** `compio-postgres` node (no duplicate versions). A duplicated `compio-postgres` would make `PostgresBackend::new(&migrated_client)` fail opaquely with "the trait `PgSession` is not implemented for `compio_postgres::Client`" (two distinct `Client` types). The shared `features` union (`with-chrono-0_4`, `with-serde_json-1` etc.) is fine — Cargo unifies features on one node.
- **`plugin-db`** (`default-features = false`, Cargo.toml:37) does **not** use the migrate PG seam. Verified: it imports only `zeroship_migrate::apply::backend::{MigrationBackend, sqlite::SqliteBackend}` + `render::declarative::*` (`register_model/sqlite_engine.rs:61-66`); its `crate::backend::PostgresBackend` is plugin-db's *own* data-plane type (`plugin-db/src/backend.rs`), unrelated to migrate. **But "no change needed" was wrong** <!-- Revised round 2: the blocker. -->: today plugin-db's `default-features=false` build compiles migrate's ungated `mod postgres` **only because `compio-postgres` is currently a non-optional migrate dep**. Once `compio-postgres` is `optional` behind `native-pg` (which plugin-db does *not* enable), the ungated PG module would lose its dep and fail — **unless `mod postgres` is `native-pg`-gated** (§4.5). It is. So the fix is: **gate `mod postgres`** so a `native-pg`-off plugin-db build genuinely omits it, and **re-verify with an actual `cargo build -p zeroship-plugin-db`** (not reasoning) that plugin-db compiles with `native-pg` off. No feature change to plugin-db's Cargo.toml is needed *provided* the gating is correct; the build is the proof.
- **`schema-authority-e2e`** (dev-dep, `features = ["zsv8"]`) gets `native-pg` via whichever feature set resolves it; ensure its exercised apply path has a driver impl (add `native-pg` to its feature list if it constructs a real `zeroship_migrate::PostgresBackend`).

**Files the `native-pg` feature touches:** `crates/zeroship-migrate/Cargo.toml` (feature def + flip `compio-postgres` to `optional`), `src/apply/backend/mod.rs` (gate `mod postgres`), the new `PgSession` trait module + `impl`, `src/conn.rs` (gate connect + `ConnectError::Connect`), the PG-apply leaves (`journal.rs`, `drift.rs`, `baseline.rs`, `precondition.rs`, `executor.rs` `pg::` leaves + `From` impls, `role.rs`), `src/command/runner.rs` (gate the `PostgresBackend` import + PG connect/apply fns; keep SQLite leg — §4.5), `src/frontend/generate.rs` (gate the live-introspection `connect`+`snapshot_schema` block behind `native-pg` too — §4.5), `src/test_support.rs` (gate its `compio_postgres::Client` helpers — §7 step 7), `src/lib.rs` (gate PG re-exports), and `crates/zeroship-migrate-server/Cargo.toml` (add `native-pg`). `render/step.rs` is **not** touched.

---

## 6. Seam validation plan

The seam is validated on two independent properties: **removability** (the PG path is a cleanly omittable module) and **genericity** (the backend monomorphizes against a non-compio driver). <!-- Revised round 2: the old "compiles with zero driver impl" single-property claim is retracted; it was falsified by the trait naming compio types. --> The tiers:

### 6a. PG-omission proof — `--no-default-features` (no `native-pg`, no `zsv8`)

`cargo build -p zeroship-migrate --no-default-features` **and `cargo build -p zeroship-migrate --no-default-features --tests`** must compile with the **entire PG seam absent** — no `PgSession` trait, no `PostgresBackend`, no PG apply leaves, no `compio-postgres` dependency — leaving the SQLite backend + IR + render + guard + dialect-neutral executor spine. The `--tests` variant is required because `pub mod test_support` (test_support.rs:8) names `compio_postgres::Client` ungated today and is compiled by `--tests` but not by a plain lib `build` (§7 step 7). This proves the PG path is a **cleanly removable module** (the honest thing a `native-pg`-off build proves), not that a driverless *seam* compiles. Assert `cargo tree -p zeroship-migrate --no-default-features` shows **no `compio-postgres`**.

> **This is NOT an abstraction/genericity proof, and does not claim to be.** <!-- Revised round 2 --> Because the `PgSession` trait itself names `compio_postgres::{Row, Error, ToSql}`, it is *inside* the gated module — a `native-pg`-off build has no trait, no generic backend, nothing to instantiate. That is correct and intended: `native-pg`-off is the "PG-omitted core" configuration (§5.3), useful for embedders that only need SQLite. **Genericity — that `PostgresBackend<'a, D>` works for a `D` other than `compio_postgres::Client` — is proven separately by §6d**, which compiles and runs the generic backend against an in-crate recording `PgSession` *under `native-pg`* (the only config where the trait exists). The two proofs are complementary: 6a proves removability; 6d proves genericity. Neither alone would suffice, and the milestone claims exactly these two, not a driverless compile of the seam.

### 6b. Behavior-unchanged — default build + full PG apply suite **through the trait**

`cargo build -p zeroship-migrate` (⇒ `native-pg` on) + `cargo test -p zeroship-migrate` (**all targets**, full-suite discipline — not `--lib`) with Postgres on :5440 (docker `appbase-migrate-postgres-1`). Every apply, journal, backfill, drift, precondition, and shadow-DB test now runs **through** `impl PgSession for compio_postgres::Client` (the generic `PostgresBackend<'_, compio_postgres::Client>` monomorphization). The existing suite passing unchanged **is** the regression bar — the native impl forwards one-to-one, so any SQL/behavior delta is a bug. Render/unit tests are the DB-free clean signal if :5440 is down (restart `docker start appbase-migrate-postgres-1` if a prior codex run tore it down).

### 6c. SQL-identity — behavior unchanged, verified

The native impl's one-line forwards guarantee identical SQL by construction. Backstop it: the existing DB-backed suite asserts observable outcomes (rows applied, journal state, drift snapshots); a passing default-build suite through the trait is the SQL-identity proof. No new SQL-diff harness is needed — the forwards are literally the same method calls.

### 6d. Genericity proof — a tiny in-crate mock/recording `PgSession` (optional but recommended)

To prove the backend is genuinely generic **without** a real second driver, add a `#[cfg(test)]` recording impl in-crate:

```rust
#[cfg(test)]
struct RecordingSession {
    log: RefCell<Vec<String>>,
    // canned Vec<Row> responses keyed by SQL, if a query path is exercised
}

#[cfg(test)]
impl PgSession for RecordingSession {
    async fn batch_execute(&self, sql: &str) -> Result<(), compio_postgres::Error> {
        self.log.borrow_mut().push(sql.to_string());
        Ok(())
    }
    // execute / execute_text_params record + return Ok(1);
    // query / query_one return canned rows or a controlled error.
}
```

A test constructs `PostgresBackend::<'_, RecordingSession>::new_generic(&rec)` — **note `new_generic`, not `new`** (§4.1): the specialized `new` only exists for `D = compio_postgres::Client` because it builds the concrete `PgShadow`; the generic `new_generic` is the constructor that monomorphizes the backend over a **non-compio** `D` (shadow = `None`). The test drives a DDL-only apply and asserts the **recorded SQL string sequence** matches the expected `BEGIN … DDL … COMMIT`.

<!-- Revised round 3: proof that this compiles, per the blocker's "prove PostgresBackend::<'_, RecordingSession>::new compiles" demand. -->
**Why `PostgresBackend::<'_, RecordingSession>::new_generic(&rec)` compiles (the flagship proof, made concrete):**
- `new_generic` lives on `impl<'a, D: PgSession> PostgresBackend<'a, D>` (§4.1) — available for `D = RecordingSession`.
- Its body builds `online: PgOnline::new(conn)` where `PgOnline<'a, RecordingSession>::new(&RecordingSession)` type-checks (`PgOnline` is generic — §4.2).
- Its body sets `shadow: None` — so it **never calls `PgShadow::new`**, sidestepping the concrete-`Client` requirement that made the round-2 design uncompilable. This is exactly why the field had to become `Option<PgShadow>`.
- `RecordingSession: PgSession`, so the `impl<D: PgSession> MigrationBackend for PostgresBackend<'_, D>` (§4.2) is satisfied and the backend is usable through the `MigrationBackend` surface.
- `shadow()` returns `None` for this `D` (§4.3), which is legal (`ShadowDryRun` is `Option`), so the DDL-apply path — which never touches the shadow harness — runs to completion.

**What this proves, and what it does NOT (be precise):** <!-- Revised round 2: the minor flaw — do not let §6d imply the whole backend is proven generic. -->

- **Proven:** the `batch_execute` / `execute` / `execute_text_params` generic paths compile *and run* against a non-compio `D`. This covers the DDL apply path, txn control, advisory-lock issue, confinement `SET`s, and journal *writes* (`record_started`/`record_completed` etc. are `execute`).
- **NOT run-proven this milestone:** the `query` / `query_one` *return* paths — drift `snapshot_schema`, journal *reads* (`applied`/`history`), backfill cursor resolution, precondition boolean probes. Reason: `compio_postgres::Row` has **private fields** (row.rs:101, verified) and is not constructible outside its crate, so a recording `D` cannot synthesize a `Vec<Row>` to return. These are exactly the paths where a future non-compio driver is most likely to diverge (row decoding), and their run-time genericity is **deferred to the `SeamValue` boundary (§3.2, §9)** — acknowledged as unproven here, not silently assumed.
- **Compile-proven (cheap, do it):** add a `#[cfg(test)]` compile-only assertion that the `query`-returning free fns *monomorphize* over a mock `D` whose `query`/`query_one` return `Err(...)` (a controlled error, no `Row` needed). This does not exercise decode, but it proves the generic *signatures* instantiate against a non-compio `D` — closing the "does it even type-check generically" gap for the query paths without needing a constructible `Row`. Drive one such path (e.g. `journal::applied`) to its first `query().await?` and assert it surfaces the injected `Err` — proving the fn body monomorphizes end-to-end even though decode is never reached.

So §6d proves: DDL/execute paths *run* generically; all query paths *type-check* generically; query decode is deferred. The milestone does **not** claim the whole backend is run-proven generic — only that it is generic (compiles/monomorphizes) everywhere and runs generically on the write paths.

---

## 7. Impl steps (in order)

<!-- Revised round 2: step order reworked — the trait is native-pg-gated (not ungated); step 6/7 reframed from "abstraction-only build" to "gate mod postgres + prove PG-omission". -->

1. **Introduce the `PgSession` trait** (new module `src/apply/backend/postgres/session.rs`) **behind `#[cfg(feature = "native-pg")]`** — the five `async fn`s under `#[allow(async_fn_in_trait)]`, referencing `compio_postgres::types::ToSql`/`Row`/`Error`. Gated (it names compio types). Build default — green (trait unused yet).
2. **Add `impl PgSession for compio_postgres::Client`** in the same `native-pg`-gated module — five one-line forwards. Build default — green.
3. **Make `PostgresBackend<'a, D: PgSession = compio_postgres::Client>` generic** (postgres.rs) + its `impl<D: PgSession> MigrationBackend`. Change the `shadow` field to `Option<PgShadow<'a>>` (§4.1) and split the constructor: native `new` on `impl<'a> PostgresBackend<'a, compio_postgres::Client>` (shadow = `Some`, keeps every existing `PostgresBackend::new(&client)` call site green), plus generic `new_generic` on `impl<'a, D: PgSession>` (shadow = `None`, for §6d). Update the `shadow()` accessor to `self.shadow.as_ref().map(|s| s as &dyn ShadowDryRun)` (§4.3); `online()` is unchanged (`PgOnline<'a, D>` is generic). Build default — green (native-path behavior byte-for-byte identical; shadow always `Some`).
4. **Thread `<D: PgSession>` through `PgOnline`** (online.rs — `PgOnline<'a, D>`, `new(conn: &'a D)`) and the free functions in **journal.rs, backfill.rs, executor.rs (PG leaves), precondition.rs, baseline.rs, drift.rs** — mechanical `conn: &Client` → `conn: &D` + fn generic param; bodies unchanged. **Per-file compile obligation (§2.1 item 2) — the search space is the FULL site inventory, not 3 sites:** the six files carry **27 `.query(` + 11 `.query_one(` + 26 `.execute(`** SQL-arg sites (re-grepped; §2.1). Most pass **string literals** (already `&str`, coerce free — e.g. all 12 `drift.rs` `.query(` are literals); only the **`&String`-passing** sites need `.as_str()`. **Build each of the six files individually and add `.as_str()` at every site the compiler rejects.** Known-likely `&String` sites: `journal.rs:1202/1212` (`&obligation_sql`), `backfill.rs:833/835` (`&batch_sql`), `precondition.rs:331` (`&stmt`) — **examples, not the exhaustive set**; the compiler enumerates the rest. Build default per file — green.
5. **Route `execute_text_params` through the trait** (executor.rs:2278 `conn.execute_text_params(...)` now resolves to `<D as PgSession>::execute_text_params`); confirm the `BindValue`→`Vec<Option<String>>` fold (executor.rs:2236) stays in-executor; **`render/step.rs` is untouched** (it names no compio type). Build default — green.
6. **Cargo.toml + gating (this crate) — apply the §4.5 two-tier map:** flip `compio-postgres` to `optional = true`; add `native-pg = ["dep:compio-postgres"]`; set `default = ["js-cli", "native-pg"]`. **Tier A** (whole-module `#[cfg(feature = "native-pg")]`): `mod postgres` (apply/backend/mod.rs:42), the `PgSession` trait module, `role.rs`, `conn.rs`'s connect path. **Tier B** (item/variant gating in SHARED modules `journal.rs`/`capability.rs`/`executor.rs`/`drift.rs`/`baseline.rs`/`precondition.rs`/`command/runner.rs`/`frontend/generate.rs`/`test_support.rs`): gate the `use compio_postgres::Client`, the `impl From<compio_postgres::Error>` / `Db(#[from] compio_postgres::Error)` **variants**, the `pg::` submodule, the `ToSql` array (journal.rs:1188), the PG `conn: &D` fns, the `runner.rs` `PostgresBackend`-using CLI fns (keep its SQLite leg), the `generate.rs` live-introspection block (`#[cfg(all(feature="zsv8", feature="native-pg"))]`), and the `test_support.rs` `Client`-naming helpers — **but keep** the dialect-neutral enums/helpers (`JournalError`, `BackfillError`, `DriftError`, `SchemaSnapshot`, `AppliedEntry`, …) and the SQLite CLI leg the SQLite path imports. Gate the PG `lib.rs` re-exports (lib.rs:117–137).
7. **Validate PG-omission** (§6a): `cargo build -p zeroship-migrate --no-default-features` compiles with **every compio-naming item absent** and **no** `compio-postgres` in `cargo tree`, while the SQLite backend still has its `JournalError`/`BackfillError`/`DriftError`/`SchemaSnapshot`. This build is the two-way catcher: too-much gating → SQLite path loses a shared type (compile error); too-little → a compio name survives ungated (compile error). Fix what the compiler points at. Proves removability, **not** genericity (§6d does that).
   - <!-- Added round 3: the minor flaw — a plain `build` misses test-support + standalone-CLI compio-naming under native-pg-off. --> **Also run `cargo build -p zeroship-migrate --no-default-features --tests`** (and `--no-default-features --features zsv8 --tests`). `test_support.rs` is `pub mod test_support` (NOT `#[cfg(test)]`) and names `compio_postgres::Client` at its top (test_support.rs:8) plus uses `snapshot_schema` (PG-path) — a lib-only `build` compiles it, but a `--tests` build under `native-pg`-off would surface any un-gated `compio_postgres::Client` there. **Resolution options** (pick at impl time, prove by this build): (a) gate `test_support.rs`'s `compio_postgres::Client`-naming helpers behind `#[cfg(feature = "native-pg")]`; or (b) gate the whole `pub mod test_support` behind `native-pg` (its PG helpers are the only consumers of the connect/snapshot path). Likewise confirm `engine.rs:26` (`use compio_postgres::Client`), `ops/submit.rs`, `ops/status.rs` compile with `native-pg` off — each has its `compio_postgres::Client`-naming block Tier-B-gated (§4.5). The `--tests` build is the catcher the plain lib `build` would miss.
8. **Dependent flips + re-verify:** add `native-pg` to `crates/zeroship-migrate-server/Cargo.toml`'s `zeroship-migrate` feature list; assert `cargo tree -p zeroship-migrate-server` shows a **single** `compio-postgres` node (version-identity, §5.4). **`cargo build -p zeroship-migrate-server` must confirm BOTH default-type-param sites elaborate** (§5.4): `PostgresBackend::new(&conn)` at apply.rs:298 *and* the explicit `&PostgresBackend<'_>` annotation at apply.rs:619 — if either fails, write `D` explicitly (`PostgresBackend<'_, compio_postgres::Client>`; a trivial in-scope fix). **Run an actual `cargo build -p zeroship-plugin-db`** (native-pg off) — it must compile with `mod postgres` omitted (it uses only `SqliteBackend`). Verify `schema-authority-e2e` resolves a driver impl for any real `PostgresBackend` path.
9. **Add the `#[cfg(all(test, feature = "native-pg"))]` recording `PgSession`** (§6d): DDL/`execute` run-recording + a compile-only `query`-path monomorphization assertion (query returns `Err`, no `Row` needed). Build + test default — green.
10. **Full-suite validation** (§6b): default build + `cargo test -p zeroship-migrate` on :5440 through the trait; render/unit as DB-free signal.

Steps 1–5 keep the **default build green** at every step; step 6 introduces the gating and the PG-omission build, proven at step 7; steps 8–9 verify the dependents and prove genericity.

---

## 8. Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| **`async fn` in trait `Send`/object-safety pitfall** — a caller needs `&dyn PgSession` or a `Send` future | Low | Static dispatch `<D: PgSession>` only (§2.3); the stack is `!Send` by design (`backfill.rs:907`), so no `Send` bound is wanted; `MigrationBackend` already proves this pattern one layer up. No `&dyn PgSession` site exists (the only `&dyn` traits are the capability sub-traits, untouched). |
| **A `compio_postgres` name leaks *outside* a `native-pg`-gated module** — breaks the `--no-default-features` PG-omission build | Med | §6a is the exact catcher: the omission build fails to compile if any *ungated* item still names `compio_postgres` (a `Client`, an `Error`, a `From` impl). Step 7 fixes leaks the compiler points at. Note the known ungated compio-namers (journal/executor/drift/baseline/precondition/capability/role/conn — §4.5): each must land inside a gated module. `render/step.rs` is NOT one of them — it names no compio type (grep-verified). |
| **`&String`→`&str` deref-coercion fails at a `.query(&sql_string, …)` site** — the trait pins `&str` but sites pass `&format!`/`&String` | Low | <!-- Round 2: the major flaw; round 3: true site count; round 4: corrected rationale (D is inferred from the receiver, not the SQL arg → coercion fires normally, low likelihood). --> `D` is inferred from the **receiver** `conn: &D`, **not** the SQL arg (§2.1 item 2), so the SQL argument never drives `D` inference and `&String`→`&str` coercion to the trait's fixed `&str` param fires reliably — the expected outcome is **no edit**. Kept as a belt-and-suspenders per-site compile obligation, not an expected failure. The search space is the **full inventory: 27 `.query(` + 11 `.query_one(` + 26 `.execute(`** across the six files (§2.1), most passing literals (coerce free). `.as_str()` is a **rare fallback** for any rejected site (1-token, behavior-neutral). Known-likely `&String` sites (still expected to coerce cleanly): `journal.rs:1202/1212`, `backfill.rs:833/835`, `precondition.rs:331`. Step 4 builds each PG file individually as the catcher. |
| **`migrated`'s `compio-postgres` and migrate's gated `compio-postgres` resolve to *different* crate instances** — `PostgresBackend::new(&migrated_client)` fails "`PgSession` not implemented for `Client`" | Low | <!-- Round 2: the minor flaw. --> Both go through the root `[workspace.dependencies] compio-postgres = { path = … }` edge (verified). Checked invariant: `cargo tree -p zeroship-migrate-server \| grep compio-postgres` shows a **single** node (step 8). A feature-union on one node is fine; a *duplicate version* is the failure mode to catch. |
| **`plugin-db`'s `default-features=false` build breaks** once `compio-postgres` is optional — it compiled the ungated `mod postgres` only via migrate's formerly-non-optional dep | Med | <!-- Round 2: the blocker. --> Gating `mod postgres` behind `native-pg` (§4.5) makes a `native-pg`-off plugin-db genuinely omit it. plugin-db uses only `SqliteBackend`+`MigrationBackend`+render (verified). Proof is an **actual `cargo build -p zeroship-plugin-db`** (step 8), not reasoning. |
| **`query`/`query_one` return the concrete `Row` — a SECOND coupling the Node driver cannot cross** — the stated end goal ("Node driver drops in") is unreachable on the read path via this seam alone | Med | <!-- Round 2: the minor flaw; round 4: escalated — concrete Row is a hard blocker, SeamRow is a PREREQUISITE not polish. --> **Disclosed as a known prerequisite, not silently deferred.** `compio_postgres::Row` has private fields (not FFI-constructible — §6d), so a napi driver **cannot implement the read verbs** until `query`/`query_one` widen to `-> Vec<SeamRow>` (§3.2). That widening is a **hard PREREQUISITE (P2, §9.1)** for the second driver, sequenced *before* it — not additive polish. This milestone therefore makes the driver swappable **on the write path only**; the read path is Node-unreachable by design until §3.2 lands. Generic *signatures* are compile-proven now (§6d monomorphization assertion, query returns `Err`); run-proof + the actual Node-reachability are deferred to §3.2/§9.1. The doc claims neither read-path run-proof nor read-path swappability today. |
| **Row-decode regression** — the ~29 `.get(...)`/`.try_get(...)` sites break | Low | `Row` stays the **concrete `compio_postgres::Row`** this milestone (§3.1) — zero decode churn regardless of site count; the `SeamValue`/`SeamRow` boundary (which inherits these ~29 sites) is future work (§9.1). |
| **Generic param cascades into dependents** — `migrated` / `plugin-db` call sites break | Low | Default type param `D = compio_postgres::Client` keeps `PostgresBackend::new(conn)` compiling unchanged; §5.4 confirms `migrated` uses the concrete `Client` (adds `native-pg`), `plugin-db` doesn't touch the seam. |
| **`native-pg`-off omits `PostgresBackend` entirely** — a reviewer expecting a "present-but-driverless" type is confused | Low | <!-- Round 2: corrected — the type is ABSENT under native-pg-off, not present-and-uninstantiable. -->  Intended posture (§5.3, §6a): `native-pg`-off is the "PG-omitted core" (SQLite + IR + render). The whole PG module — trait, `PostgresBackend`, `PgShadow`, apply leaves — is gated together, so it is genuinely absent, not compiled driverless. Any real PG embedder enables `native-pg` (default-on). Documented so a reviewer does not expect a driverless-`PostgresBackend` compile (there is none — §6a proves *omission*, §6d proves genericity). |
| **Cargo `workspace = true` + `default-features = false` ignores the consumer's flag** (the Phase-1 addendum gotcha) | Med | If `migrated`/`plugin-db` set `default-features = false` on a `workspace = true` edge, the **root `[workspace.dependencies]`** entry must also declare it, else Cargo ignores it (warns). Since `native-pg` is default-on and consumers opt *in*, verify with `cargo tree -p zeroship-migrate-server` that a driver impl is actually present (nonzero `compio-postgres`), not accidentally gated out. |
| **Shadow-harness lifecycle leaks into the generic path** — `JoinHandle`/`cancel` is compio-specific | Low | `PgShadow` stays **concrete** (§4.4); it is never parameterized; a non-PG driver reports `shadow() == None`. The generic cascade explicitly excludes `shadow.rs`. |
| **`execute_text_params` mistaken for foldable into `execute`** — losing text-format server-inferred typing | Low | Kept a **distinct** trait method (§2.1/§2.2); the doc comment records *why* (binary OID bind would break `text → timestamptz`). The `BindValue` fold stays in-executor (§2.3). |
| **Full suite not run through the trait** — a `--lib`-only pass hides an integration regression | Med | §6b mandates the **full** per-crate suite on :5440 (full-suite discipline), all targets, through `PostgresBackend<'_, compio_postgres::Client>`. Render/unit is only the DB-free fallback signal. |

---

## 9. FUTURE WORK (not this milestone)

### 9.1 The Node/napi host-callback driver (the second `PgSession` impl)
The eventual direction is a Node shell (napi-rs) that embeds the V8-free (`zsv8`-off) **and** compio-free-capable core and supplies the driver from JS: `impl PgSession for NodePgSession` marshals each verb to the `pg` npm client over the napi boundary. This is what makes `native-pg` an *alternative* rather than the only impl.

**The Node driver has two HARD PREREQUISITES this milestone does not deliver** <!-- Revised round 4 (addressing MINOR): distinguish the true prerequisites (Error neutralization + SeamRow widening — WITHOUT which the Node impl cannot be WRITTEN) from the additive marshalling detail. The concrete-Row return is the load-bearing blocker; frame it as a blocker, not a step. -->:
- **(P1) `Error` becomes an associated/neutral `type Error`** (§2.3) — the trait currently pins `compio_postgres::Error` in every method return, which a napi driver cannot produce. Blocking.
- **(P2) `query`/`query_one` widen to `-> Vec<SeamRow>`** (§3.2/§3.1) so the ~29 decode sites port to `SeamRow::get` — **this is the load-bearing blocker**: a napi driver **cannot return the concrete `compio_postgres::Row`** (private fields, not FFI-constructible), so *without* this widening the Node driver **cannot implement `query`/`query_one` at all**. This milestone leaves `Row` concrete (§3.1) and thus leaves the read path Node-unreachable by design; P2 is the gate that opens it.

Only *after* P1+P2 is the third piece **additive**: (P3) the two format-coupling risk spots (§3.4) — `"char"`→`i8` and ordered `text[]` with NULL-vs-empty — reproduced in the Node marshalling. **No code now** — but note the honest ordering: the seam makes the Node driver *reachable in principle* and swappable **on the write path today**; the read path is reachable only once P1+P2 land. The seam is not, by itself, sufficient for a working Node driver.

### 9.2 Compio removal from the core
With `native-pg` off and a Node driver supplying the session, the core's compio DB-I/O could leave the tree entirely (mirroring the Phase-1 doc's §8.2 "compio DB-I/O seam" note). **Explicitly deferred** — compio stays in the core this milestone (§1 non-goal). The `PgSession` seam is the enabling precondition, exactly as Phase-1's `zsv8` gate was the enabling precondition for the napi shell. The two seams (`PgSession` transport + the `MigrationBackend` dialect seam) are understood as independent axes, not conflated.
