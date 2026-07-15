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
| `PostgresBackend<S>` | PostgreSQL execution over a host-provided `SqlSession` |
| `apply::backend::MysqlBackend<S>` | MySQL execution over a host-provided `SqlSession` |
| `SqliteBackend` | In-process SQLite execution |
| `SchemaSnapshot` | Captured schema used for explicit structural drift |

## Plan and apply PostgreSQL

The example begins after your host has produced a reviewed `Vec<Migration>`:

```rust
use zero_migrate::{
    Approval, ExecutorConfig, GuardConfig, Migration, MigrationEngine,
    PostgresBackend, SqlDialect, SqlSession, confined_no_inject_policy,
};

async fn apply_postgres<S: SqlSession>(
    session: &S,
    migrations: &[Migration],
) -> Result<(), Box<dyn std::error::Error>> {
    let policy = confined_no_inject_policy("app_demo")
        .map_err(std::io::Error::other)?;
    let guard = GuardConfig::from_policy(policy, SqlDialect::Postgres);

    let engine = MigrationEngine::new();
    let plan = engine.plan(migrations, &guard);
    if !plan.is_appliable() {
        return Err(format!("{} migrations denied", plan.denied.len()).into());
    }

    let config = ExecutorConfig::new("project_demo", "app_demo")
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
is currently Rust-only.

The SQLite backend supports transactional DDL, schema snapshots, table rebuilds,
one-shot DML, and batched backfill. It rejects non-transactional migrations and
does not coordinate a project lock across separate processes.

## Backend capabilities

| Capability | PostgreSQL | MySQL | SQLite |
| --- | --- | --- | --- |
| Versioned DDL apply | Yes | Yes | Yes |
| Atomic DDL + journal event | Transactional changes | No | Yes |
| Schema snapshots | Yes | No | Yes |
| Preconditions | Yes | No | Empty only |
| DML plan steps | Yes | No | Yes |
| Batched backfill | No | No | Yes |
| Baseline | Yes | No | Yes |
| Pending expand/contract state | Yes | No | No |

Unsupported capabilities return an error; they are not silently approximated.

## Policy

The simple confined policy is:

```rust
let policy = zero_migrate::confined_no_inject_policy("app_demo")
    .map_err(std::io::Error::other)?;
let guard =
    zero_migrate::GuardConfig::from_policy(policy, zero_migrate::SqlDialect::Postgres);
```

To load a root ceiling:

```rust
let policy =
    zero_migrate::effective_policy_from_ceiling_toml(policy_toml)?;
```

Use the same `EffectivePolicy` for table-shape resolution, planning, and host
approval decisions. The current public executor configuration does not accept an
arbitrary effective policy, so a fully custom apply posture needs a reviewed host
integration. See [Policy model](policy.md) for the public policy types.

## Plans and verified apply

`MigrationEngine::plan` is database-free and returns:

- accepted migrations;
- denials;
- operational advisories;
- destructive classification;
- approval requirements.

Apply checks the plan again before executing it.

For a reviewed migration set, `apply_verified` compares a trusted
`ManifestHash` before apply. Keep the expected manifest outside the submitted
migration bundle. `apply_verified_scoped` also accepts an `ApprovalScope` so a
host can approve only selected destructive versions.

Declarative and structured plan APIs are available for Rust hosts that need
schema diffs, DML steps, expand/contract obligations, or touched-table metadata.

## Status, history, and drift

Rust hosts can:

- call `status_via_backend` with the complete migration set;
- read PostgreSQL append-only history with the public `history` helper;
- rely on automatic checksum/kind checks during apply;
- capture `SchemaSnapshot` values and compare them explicitly.

Structural drift is not checked automatically on every apply. PostgreSQL and
SQLite support snapshots; MySQL currently does not.

## Rollback

There is no high-level `MigrationEngine::rollback` workflow. Some backends expose
limited low-level down operations, but they do not provide safe target
selection, approval, complete state validation, or high-level guard
orchestration. Prefer a reviewed forward-fix migration.

## JavaScript boundary

The public `zero-migrate-engine` API is the supported JavaScript integration.
It exposes `apply`, `plan`, `validate`, `status`, `history`, and
`currentIrVersion`.

Current JavaScript boundaries:

- apply supports PostgreSQL and MySQL DDL, not SQLite;
- authored `insert`, `update`, `delete`, and `backfill` steps are not executed by
  the public JavaScript apply path;
- plan/validate are offline checks, not full Rust engine plans;
- status/history are PostgreSQL-only in practice;
- custom policy can drive Rust planning and host decisions, but a general
  end-to-end custom-policy executor configuration is not public yet;
- scoped approvals are Rust integration features.

## Next

- [Node API](node-api.md)
- [Policy model](policy.md)
- [Security model](security-model.md)
- [Operating migrations](operations.md)
- [Documentation home](README.md)
