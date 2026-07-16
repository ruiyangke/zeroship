//! The SQL deploy-target dialect — a closed, pure-data enum.
//!
//! `SqlDialect` names which SQL engine a migration is being rendered/applied
//! for. It is a wire-level target descriptor (three closed variants, no
//! behaviour of its own), so it lives in the leaf `zero-migrate-ir` contract
//! rather than the engine: both the engine's schema-render layer
//! (`zero_migrate::schema::query`, which re-exports it) and the SQL security
//! layer (`zero-migrate-guard`, which is below the engine) name it, and neither
//! may depend on the other.
//!
//! The *dialect-specific spelling* (the `SchemaRenderer` trait, the DDL builders,
//! canonical type maps) lives engine-side in `zero_migrate::schema::query`; this
//! enum carries only the identity of the target.

/// The SQL dialect a migration renders/applies against.
///
/// A closed enum: adding a fourth dialect breaks the exhaustive dispatch matches
/// (e.g. `zero_migrate::schema::query::renderer`) at compile time, forcing every
/// dialect-specific spelling to be wired before the crate can build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDialect {
    /// Postgres dialect: binary DML values wrap their text placeholder with
    /// `decode($N, 'base64')` so a `BYTEA` column receives the original bytes.
    Postgres,
    /// `SQLite` dialect: binary DML values use a numbered `?N` placeholder and
    /// are bound directly as a `BLOB` by the in-process SQLite actor.
    Sqlite,
    /// `MySQL` dialect: binary DML values wrap their text placeholder with
    /// `FROM_BASE64(?)` so a binary column receives the original bytes.
    Mysql,
}
