//! Neutral contract for vendor-owned analysis of catalog-stored DDL.
//!
//! Some backends retain their original `CREATE TABLE` statement and need to
//! parse or surgically rewrite that vendor grammar during a table rebuild. The
//! engine owns the rebuild plan, while the backend that owns the grammar owns
//! every parser and rewrite body behind this trait.

use std::collections::BTreeSet;

use crate::error::DeclarativeError;
use crate::schema::SchemaRenderer;
use crate::snapshot::{ColumnSnapshot, TableSnapshot};

/// Vendor-owned parser and surgical rewriter for catalog-stored table DDL.
///
/// There is deliberately no shared implementation. Backends without a stored-DDL
/// grammar return `None` from [`SchemaRenderer::stored_ddl`]; a backend that returns
/// a parser supplies every operation below itself.
pub trait StoredDdl: std::fmt::Debug + Sync {
    /// Locate the outer table body.
    fn create_body_bounds(&self, sql: &str) -> Option<(usize, usize)>;

    /// Split an outer table body into byte-preserving clauses.
    fn table_clauses<'a>(&self, body: &'a str) -> Option<Vec<&'a str>>;

    /// Consume one DDL word or quoted identifier.
    fn ddl_word(&self, sql: &str, cursor: &mut usize) -> Option<String>;

    /// Whether the first DDL word uses a quoted-identifier form.
    fn first_ddl_word_is_quoted(&self, sql: &str) -> bool;

    /// Column names whose stored definitions are generated.
    fn generated_columns(&self, create_sql: &str) -> BTreeSet<String>;

    /// Rewrite only the stored primary-key clauses.
    fn rewrite_stored_primary_key(
        &self,
        table: &str,
        stored: &str,
        target_columns: Option<&[String]>,
        materialize_not_null: Option<&str>,
        backend: &dyn SchemaRenderer,
    ) -> Result<String, DeclarativeError>;

    /// Whether the table uses a storage option that removes its implicit row id.
    fn create_is_without_rowid(&self, create_sql: &str) -> bool;

    /// Whether an inline primary key's ordering prevents row-id aliasing.
    fn inline_primary_key_is_desc(&self, create_sql: &str, column: &str) -> bool;

    /// Rewrite only the stored named foreign-key clauses.
    fn rewrite_stored_foreign_keys(
        &self,
        table: &str,
        stored: &str,
        live: &TableSnapshot,
        desired: &TableSnapshot,
        backend: &dyn SchemaRenderer,
    ) -> Result<String, DeclarativeError>;

    /// Return the module named by a stored virtual-table declaration.
    fn virtual_table_module(&self, sql: &str) -> Option<String>;

    /// Name the first dependency that makes a native dropped-column operation unsafe.
    fn dropped_column_dependent(
        &self,
        table: &str,
        column: &ColumnSnapshot,
        live: &TableSnapshot,
    ) -> Option<String>;
}
