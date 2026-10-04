//! Recorded artifacts the suites of more than one crate pin against.
//!
//! [`DB_TODOS_MIGRATIONS_IR`] is the recorded migration set for
//! `examples/db-todos`, as `zeroship migrate` puts it on the wire.
//! `zeroship-cli` binds it to the example's `.ts` migrations by recording them
//! and to the committed descriptor by hashing that file; `zeroship-migrate-node`
//! pins the artifacts it generates against it. Both read it from here.

use std::path::PathBuf;

/// The recorded migration set for `examples/db-todos`.
pub const DB_TODOS_MIGRATIONS_IR: &str = include_str!("recorded/db-todos.ir.json");

/// Where [`DB_TODOS_MIGRATIONS_IR`] lives on disk, for a case that hands the
/// file itself to a reader.
#[must_use]
pub fn db_todos_migrations_ir_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/recorded/db-todos.ir.json")
}
