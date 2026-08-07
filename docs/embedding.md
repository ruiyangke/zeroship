# Rust API

Most migration authors should use the JavaScript packages. Use the Rust API when
you need SQLite apply, custom policy, full engine plans, manifests, structural
drift, or a host that does not run Node.

## Add the engine

The Rust crate is not published yet. From this source checkout, use a path
dependency:

```toml
[dependencies]
zero-migrate = { path = "crates/zero-migrate" }
```

When building custom policy documents and the built-in registry, add the two
public policy packages as well:

```toml
zero-migrate-policy = { path = "crates/zero-migrate-policy" }
zero-migrate-ir = { path = "crates/zero-migrate-ir" }
```

Adjust the relative paths if your host's `Cargo.toml` is outside the repository
root.

Once the crates are published, replace these paths with the released versions
selected by your application.

## Main public types

| Type | Purpose |
| --- | --- |
| `MigrationEngine` | Build plans and apply them |
| `Migration` | One versioned database change |
| `MigrationPlan` | Guarded plan with denials, advisories, and approval state |
| `GuardConfig` | Target dialect and effective safety policy |
| `ExecutorConfig` | Project identity, schema, timeouts, and PostgreSQL role settings |
| `Approval` | Coarse destructive-change approval |
| `ApprovalScope` | Approval limited to selected migration versions |
| `Resolution` | Keep the destination (`Applied`) or source (`Aborted`) for a pending PostgreSQL rename |
| `PendingContract` | One outstanding PostgreSQL online rename and its stable resolution key |
| `PendingContractStatus` | Status view of an outstanding rename, including orphan diagnosis |
| `BlockedPlan` | A plan waiting on a dependency's pending rename |
| `AppliedPlanStatus` | Plan-aware status, including `applied`, `pending`, `aborted`, plan details, and pending contracts |
| `ReconciledPlanState` | Plan state: applied, aborted, pending, partial, drifted, blocked, or unknown dependency |
| `PlanStatusStepState` | Step state: pending, inflight, applied, aborted, or drifted |
| `PostgresBackend<S>` | PostgreSQL execution over a host-provided `SqlSession` |
| `apply::backend::MysqlBackend<S>` | MySQL execution over a host-provided `SqlSession` |
| `SqliteBackend` | SQLite execution from a Rust host |
| `SchemaSnapshot` | Captured schema used for explicit structural drift |

## Plan and apply PostgreSQL

The example begins after your host has produced a reviewed `Vec<Migration>`:

```rust
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, Migration, MigrationEngine,
    PostgresBackend, SqlDialect, SqlSession, effective_policy_from_charter_toml,
};

const POLICY_CHARTER: &str = r#"policy_version = 1

[[grant]]
key = "schema.cross_schema"
value = true
scope = { include = ["app_demo"] }

[[grant]]
key = "schema.create_table"
value = true
scope = { include = ["app_demo"] }

[[grant]]
key = "schema.rename"
value = true
scope = { include = ["app_demo"] }

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"
"#;

async fn apply_postgres<S: SqlSession>(
    session: &S,
    migrations: &[Migration],
) -> Result<(), Box<dyn std::error::Error>> {
    let policy = effective_policy_from_charter_toml(POLICY_CHARTER)
        .map_err(std::io::Error::other)?;
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);

    let engine = MigrationEngine::new();
    let plan = engine.plan(migrations, &guard);
    if !plan.is_appliable() {
        return Err(format!("{} migrations denied", plan.denied.len()).into());
    }

    let config = ExecutorConfig::new("project_demo", "app_demo", policy)
        .with_migrator_role("migrator_app_demo");
    let backend = PostgresBackend::new_generic(session);

    engine
        .apply(&plan, Approval::None, &backend, &config, "deploy")
        .await?;

    Ok(())
}
```

Use `Approval::Approved` only after trusted operator code has approved the exact
migration. `Approval::None` fails when the plan requires approval.

`ExecutorConfig::new` does not select a PostgreSQL migrator role automatically.
`with_migrator_role` is the least-privilege option when the role has already been
provisioned.

## PostgreSQL and MySQL sessions

Both network backends accept a `SqlSession`:

```rust
let postgres = zero_migrate::PostgresBackend::new_generic(&session);
let mysql =
    zero_migrate::apply::backend::MysqlBackend::new_generic(&session);
```

Give each operation a dedicated, idle session. In particular, never pass a
MySQL connection that contains caller-owned transaction work: zero-migrate
enables autocommit while schema SQL runs and rejects an active transaction before
making that change. The MySQL account must be able to read Performance Schema's
transaction tracking tables, with the `transaction` instrument and
`events_transactions_current` consumer enabled; verification fails closed when
that evidence is unavailable.

An interrupted, auto-committing MySQL schema migration leaves an inflight
marker. A Rust host that embeds this crate resolves it with the typed recovery
API. That API has no binding across the Node addon, so it is unreachable from
the CLI and the Node SDK; operators on those surfaces resolve the same marker by
restoring and verifying the complete pre-migration shape and then deleting that
version's row from the mutable `schema_migrations_inflight` side-table. Neither
route touches the append-only, trigger-guarded `schema_migrations` event table.

```rust
use zero_migrate::apply::backend::MysqlInflightResolution;

mysql
    .recover_inflight_ddl(
        &config,
        &reviewed_migration,
        MysqlInflightResolution::MarkAppliedAfterVerification,
        "operator@example.com",
        "verified the complete intended schema shape",
    )
    .await?;
```

Choose `ClearForRetryAfterRollback` instead only after restoring and verifying
the full pre-migration shape. The exact migration identity is checked and the
decision is recorded in immutable recovery history; that identity check and that
audit row are what this route adds over the direct marker delete, along with
resolving the "the DDL fully landed" case without rolling anything back. It does
not inspect the database. Recovery records your assertion about the shape rather
than verifying it, so verify the live shape first, outside zero-migrate. See
[Interrupted MySQL or non-transactional work](operations.md#interrupted-mysql-or-non-transactional-work)
for the operator-facing checklist.

The public trait has five asynchronous operations:

```rust
pub trait SqlSession {
    async fn batch(&self, sql: &str) -> Result<(), DbError>;
    async fn exec(&self, sql: &str, binds: &[Bind]) -> Result<u64, DbError>;
    async fn exec_text(
        &self,
        sql: &str,
        params: &[Option<String>],
    ) -> Result<u64, DbError>;
    async fn query(&self, sql: &str, binds: &[Bind])
        -> Result<Vec<Row>, DbError>;
    async fn query_one(&self, sql: &str, binds: &[Bind])
        -> Result<Row, DbError>;
}
```

A session must keep one physical database connection for the complete engine
operation. Preserve exact 64-bit integers/decimals, return a non-empty
`DbError.message`, and carry SQLSTATE when available.

The public JavaScript package already supplies PostgreSQL and MySQL sessions;
JavaScript users do not implement this trait.

## SQLite

SQLite does not use `SqlSession`:

```rust
use std::path::Path;
use zero_migrate::SqliteBackend;

let backend = SqliteBackend::open(
    Path::new("app.sqlite"),
    Path::new("app.migrations.sqlite"),
)?;
```

Use different files for application data and the migration journal. SQLite apply
is also available through Node and the CLI; this Rust API is an additional host
for advanced integrations.

The SQLite backend supports transactional DDL, schema snapshots, table rebuilds,
insert, update, delete, and batched backfill. It rejects non-transactional
migrations. SQLite apply coordinates zero-migrate processes that use the same
application database, and it refuses to open for migration when crash-safe
application and journal settings cannot be established.

SQLite backfills require an exact ordered, non-null primary or unique
candidate-key tuple whose components have supported declared `INTEGER` or
`TEXT` affinity. Every live cursor value must use the matching SQLite storage
class. Partial tuples and unsupported or mixed storage classes are rejected
before that backfill changes rows. `WITHOUT ROWID` tables are supported when
their candidate key meets these rules.

## Backend capabilities

| Capability | PostgreSQL | MySQL | SQLite |
| --- | --- | --- | --- |
| Versioned DDL apply | Yes | Yes | Yes |
| Atomic DDL + journal event | Transactional changes | No | Yes |
| Schema snapshots | Yes | Limited: tables, columns, and ordered indexes | Yes |
| Preconditions | Yes | No | Empty only |
| Insert/update/delete | Yes | Yes, on trigger-free InnoDB | Yes |
| Batched backfill | Yes, with an exact ordered primary/unique cursor tuple and explicit stability | Yes, on InnoDB with an exact ordered primary/unique cursor tuple and explicit stability | Yes, with a supported exact ordered primary/unique cursor tuple and explicit stability |
| Baseline | Yes | No | Yes |
| Pending expand/contract state | Yes | No | No |

Unsupported capabilities return an error; they are not silently approximated.

## Policy

Policies are authored as root charter TOML and loaded explicitly:

```rust
let policy =
    zero_migrate::effective_policy_from_charter_toml(policy_toml)?;
let guard = zero_migrate::GuardConfig::from_policy(
    policy.clone(),
    zero_migrate::SqlDialect::Postgres,
);
let executor = zero_migrate::ExecutorConfig::new(
    "project_demo",
    "app_demo",
    policy,
);
```

Use the same `EffectivePolicy` for table-shape resolution, planning, and host
approval decisions. The executor requires that policy explicitly and never selects
one for the caller. See [Policy model](policy.md) for the public policy types.

## Plans and verified apply

`MigrationEngine::plan` is database-free and returns:

- accepted migrations;
- denials;
- operational advisories;
- destructive classification;
- approval requirements.

Apply checks the plan again before executing it.

Schema and data steps execute in authored order. Every executable step receives
a stable journal identity tied to the complete migration checksum. Backfills
require approval, an exact ordered non-null primary/unique candidate-key cursor
tuple, and an explicit stability mode. Approval is preflighted across the complete plan before any
authored step runs. A backfill captures a fixed terminal cursor before its first
batch, commits bounded batches, resumes after the last committed cursor, and
stops at its original boundary. The bounded cohort does not cover concurrent
inserts by itself: make new rows fail the filter before capture or arrange a
final catch-up while writes are stopped.

MySQL structured insert, update, delete, and backfill additionally require an
InnoDB target without user triggers. Apply refuses the operation when it cannot
prove that target and journal effects stay transactionally consistent.

For a reviewed migration set, `apply_verified` compares a trusted
`ManifestHash` before apply. Keep the expected manifest outside the submitted
migration bundle. `apply_verified_scoped` also accepts an `ApprovalScope` so a
host can approve only selected destructive versions.

Declarative and structured plan APIs are available for Rust hosts that need
schema diffs, DML steps, expand/contract obligations, or touched-table metadata.

## Resolve a PostgreSQL online rename

Rust hosts can resolve a pending rename with the public `Resolution` type and
`MigrationEngine::resolve_pending_contract`:

```rust
use zero_migrate::{Approval, MigrationEngine, Resolution};

engine
    .resolve_pending_contract(
        pending_version,
        Resolution::Applied,
        "app_demo",
        Approval::Approved,
        &backend,
        &config,
        "deploy:rename-finish",
    )
    .await?;
```

`Resolution::Applied` keeps the destination column and drops the source.
`Resolution::Aborted` keeps the source and drops the destination. Both require
`Approval::Approved`. The host should choose `Applied` only after every
application instance and database consumer has moved to the destination, or
choose `Aborted` only after moving them back to the source.

The initial rename requires a matching live source type and `id` as the
complete, non-null, single-column primary key with a supported cursor type. It
also requires no pre-existing enabled user triggers and row-level policy that
allows every selected update. In a PostgreSQL migration, the rename must be the
only operation targeting its table; other tables may still have operations in
that migration. Apply same-table follow-up work only from a later migration and
only after resolution.

The destination is nullable but otherwise keeps the source's exact live
PostgreSQL type and modifiers. Equivalent built-in aliases are accepted without
discarding modifiers, while modifier drift is refused during resolution. Review
and recreate required defaults, constraints, indexes, comments, and dependent
objects after resolution. Source dependencies can block resolution and must be
audited before rollout. `PendingContract`,
`PendingContractStatus`, `BlockedPlan`, and
`AppliedPlanStatus` expose the public status needed to retain and monitor the
obligation. Resolution cleanup is all-or-nothing. A failure leaves both columns
and the managed rename trigger intact and keeps the obligation pending, so retry
the same action after correcting the cause.

A successful apply or abort makes the original rename identity terminal. Exact
replay cannot reopen it; after abort, author a new migration name to try again.
`AppliedPlanStatus.aborted` contains terminal aborted plan IDs, and the matching
`ReconciledPlanState` and deferred `PlanStatusStepState` values are `Aborted`.
An aborted plan does not satisfy `depends_on`, so dependent work remains
blocked until it points to a replacement migration identity.

## Status, history, and drift

Rust hosts can:

- call `status_via_backend` with the complete migration set;
- read PostgreSQL append-only history with the public `history` helper;
- reconcile every schema, data, and backfill step and rely on automatic
  checksum/kind checks during apply;
- capture `SchemaSnapshot` values and compare them explicitly.

Structural drift is not checked automatically on every apply. PostgreSQL and
SQLite support structural snapshots for explicit comparison. MySQL returns a
limited snapshot of tables, columns, and ordered indexes so a host can prepare
live-dependent work; it is not a complete MySQL structural-drift view.

## Rollback

There is no high-level `MigrationEngine::rollback` workflow. Some backends expose
limited low-level down operations, but they do not provide safe target
selection, approval, complete state validation, or high-level guard
orchestration. Prefer a reviewed forward-fix migration.

## JavaScript boundary

The public `zero-migrate-cli` API is the supported JavaScript integration.
It exposes `apply`, `resolvePending`, `plan`, `validate`, `status`, `history`, and
`currentIrVersion`.

Current JavaScript boundaries:

- apply supports complete ordered schema and data migrations on PostgreSQL,
  MySQL, and SQLite;
- pending deletes and backfills require Node `approved: true` or CLI
  `--approve`;
- PostgreSQL online rename returns `pendingContracts` from `apply()` and uses
  `resolvePending()` or CLI `resolve` for approved completion or abort;
- plan/validate are offline checks, not full Rust engine plans;
- status is plan-aware on PostgreSQL and MySQL when supplied the ordered
  migration set; history remains PostgreSQL-only;
- custom policy can drive Rust planning and host decisions, but a general
  end-to-end custom-policy executor configuration is not public yet;
- scoped approvals are Rust integration features.

## Next

- [Node API](node-api.md)
- [Policy model](policy.md)
- [Security model](security-model.md)
- [Operating migrations](operations.md)
- [Documentation home](README.md)
