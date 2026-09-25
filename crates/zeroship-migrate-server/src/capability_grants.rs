//! The column-scoped DML a database's capability and unmask roles hold on its
//! creator tables.
//!
//! The cluster reconciler mints `zs_db_<dbs>_rw`, `zs_db_<dbs>_ro` and
//! `zs_db_<dbs>_unmask` and gives them `USAGE` on the schema and nothing else,
//! because which COLUMNS each of them may reach is a fact about the tables an
//! apply creates rather than about the database. This module is where that fact
//! becomes SQL.
//!
//! # Column-scoped, and a table-level grant beside a column list WIDENS
//!
//! `GRANT SELECT ON <table>` and `GRANT SELECT (a, b) ON <table>` are not two
//! spellings of a narrowing. `PostgreSQL` takes the union: with both in place
//! `has_column_privilege` answers true for every column, including the ones the
//! column list deliberately omitted. So the emission below grants `SELECT`,
//! `INSERT` and `UPDATE` only in their column-listed form. `DELETE` has no
//! column form at all and is granted at the table, which is sound because a
//! `DELETE` names no column to return.
//!
//! # Which column is withheld
//!
//! A masked field occupies TWO physical columns, and the orientation is the
//! opposite of the obvious one. The field's own column holds the MASK; the
//! sibling [`raw_column_name`](zeroship_migrate_backend::schema::raw_column_name)
//! composes - `__zs_raw__<field>`, the
//! [`RAW_COLUMN_PREFIX`](zeroship_migrate_backend::schema::RAW_COLUMN_PREFIX) -
//! holds the real value, plaintext or ciphertext. That is the column the
//! classification is about, and it is the one withheld from both capability
//! roles' `SELECT`:
//!
//! - **read** is every physical column EXCEPT the `__zs_raw__` siblings, so a
//!   session that projects a masked field gets the mask,
//! - **write** is every physical column INCLUDING them, because the data plane
//!   dual-binds both halves on insert and update
//!   (`zeroship_data_orm::protection::mask_pass::relocate_masked_columns`),
//! - **unmask** is the complement of read - the `__zs_raw__` siblings and
//!   nothing else - and it goes to a role no session carries ambiently.
//!
//! Withholding `SELECT` while granting `UPDATE` on the same column is not an
//! oversight: it also denies a `WHERE` clause over the real value, which is the
//! unaudited binary search `raw_column_for_field` names as the reason the real
//! value has to leave the queryable column.
//!
//! # The real value is reachable, and only from the audited path
//!
//! Withholding the column from the two roles a session narrows to is what makes
//! `zeroship_data_orm::protection::unmask::dispatch_unmask` the only way to the
//! plaintext, but the dispatcher still has to be able to read it. The third
//! grant is what it reads through: the binding role holds membership in
//! `zs_db_<dbs>_unmask` `WITH INHERIT FALSE`
//! (`crate::datastore::cluster::grant_binding_statements`), so the privilege is
//! never ambient and a statement gets it only by assuming the role on purpose.
//! The unmask role is granted the raw columns and NOTHING else - no non-raw
//! column, no `INSERT`, no `UPDATE`, no `DELETE` - so assuming it narrows the
//! session rather than widening it.
//!
//! The partition is taken from the PHYSICAL COLUMN NAME and not from the
//! `zero-migrate:mask` comment sentinel. The sentinel rides the field's own
//! column (`zeroship_migrate_backend::schema::build_mask_sentinel_comments`), so
//! reading it identifies the column to GRANT; the prefix identifies the column to
//! WITHHOLD, which is the direction a mistake must not go. A column the engine
//! never created but whose name carries the prefix is withheld too, which is the
//! same direction.
//!
//! # It converges the whole schema, not the tables one apply touched
//!
//! Every statement states a desired state. A table is revoked to nothing and
//! re-granted from the live catalog, so re-running changes nothing, a
//! reclassified column loses the privilege its earlier shape had, and a database
//! whose earlier apply predates this module acquires its grants on the next one.
//! Deriving the set from the documents an apply carried would instead leave the
//! previous shape's grants standing beside the new one, which is the direction
//! that widens.
//!
//! # Platform tables are out of scope
//!
//! The engine journal and the unmask audit table live in the creator's schema
//! under [`PLATFORM_TABLE_PREFIX`], which a creator collection name is refused
//! from. They are not granted here and not revoked here: the audit table's own
//! grant is `INSERT` and deliberately no `SELECT`
//! ([`crate::provisioning::audit_unmask_capability_grants_sql`]), and a sweep
//! that treated it as a creator table would hand every session the audit trail
//! of every other actor.

use compio_postgres::Client;
use zeroship_core::database_derivation;
use zeroship_core::database_role::{DatabaseCapability, RoleNameTooLong};
use zeroship_core::DatabaseId;
use zeroship_migrate_backend::schema::RAW_COLUMN_PREFIX;

use crate::apply::quote_ident;
use crate::provisioning::exec_retry;

/// The prefix every platform-owned table inside a creator schema carries.
///
/// `zeroship_data_orm::sql::ident` reserves it, which is what makes a creator
/// collection unable to claim one and therefore makes this filter exact rather
/// than a guess. [`crate::provisioning::AUDIT_UNMASK_TABLE`] and the engine's
/// journal tables are the members; `the_audit_table_is_a_platform_table` binds
/// the constant to the one this crate also spells.
pub const PLATFORM_TABLE_PREFIX: &str = "__zeroship_";

/// A cluster-side step of the capability grant emission that did not complete.
#[derive(Debug, thiserror::Error)]
pub enum CapabilityGrantError {
    /// The cluster refused or could not answer.
    #[error("capability column grant: {0}")]
    Query(#[from] compio_postgres::Error),
    /// A derived role name would not fit a `PostgreSQL` identifier.
    #[error(transparent)]
    RoleName(#[from] RoleNameTooLong),
}

/// One creator table and every physical column the catalog holds for it, in
/// `attnum` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatorTable {
    /// The unqualified relation name.
    pub name: String,
    /// Every live column, dropped ones excluded.
    pub columns: Vec<String>,
    /// The columns of the table's `PRIMARY KEY`, in the same order, or empty
    /// when it has none.
    ///
    /// They are not a separate grant set. They ride in [`Self::unmaskable`],
    /// because an unmask read locates ONE row and `PostgreSQL` requires
    /// `SELECT` on every column a `WHERE` clause references, not only on the
    /// ones projected.
    pub key_columns: Vec<String>,
}

impl CreatorTable {
    /// The columns a capability may READ: every column that is not a masked
    /// field's real-value sibling.
    #[must_use]
    pub fn readable(&self) -> Vec<&str> {
        self.columns
            .iter()
            .filter(|column| !column.starts_with(RAW_COLUMN_PREFIX))
            .map(String::as_str)
            .collect()
    }

    /// The columns a readwrite capability may WRITE: every column, the
    /// real-value siblings included.
    #[must_use]
    pub fn writable(&self) -> Vec<&str> {
        self.columns.iter().map(String::as_str).collect()
    }

    /// The columns the UNMASK role may read: the ones [`Self::readable`]
    /// withholds, plus the `PRIMARY KEY` that addresses one row.
    ///
    /// # The key column is not a widening, it is the read
    ///
    /// `zeroship_data_orm::crud::internal::raw_column` compiles
    /// `SELECT <raw> AS _raw FROM <table> WHERE id = $1`, and `PostgreSQL`
    /// checks `SELECT` on every column an expression references rather than on
    /// the projection alone: without the key the statement is refused `42501`
    /// on a column it never returns. Withholding it would not narrow the role,
    /// it would make it unusable and push the read back onto a projection over
    /// the whole table.
    ///
    /// It discloses nothing either capability role is refused - both read the
    /// key already - and the role still cannot reach the mask-bearing column or
    /// any other payload column beside the real value.
    ///
    /// # A table with no masked field yields nothing
    ///
    /// The empty answer is load-bearing rather than an optimisation: the key
    /// alone is a grant with no purpose, and `capability_column_grants_sql`
    /// suppresses the whole statement on an empty list, so an unmasked table
    /// leaves the role holding nothing on it at all.
    ///
    /// # A MASKED table with no primary key cannot be unmasked
    ///
    /// The key comes from `pg_index`, so a table the IR gave no `PRIMARY KEY`
    /// contributes none, and the grant is the raw siblings alone - on which the
    /// row-addressed read is refused `42501`. That is the behaviour
    /// `the_unmask_set_is_the_raw_siblings_and_the_key_that_addresses_a_row`
    /// pins, and it is a refusal rather than a leak, which is the direction a
    /// mistake here must go.
    #[must_use]
    pub fn unmaskable(&self) -> Vec<&str> {
        if !self
            .columns
            .iter()
            .any(|column| column.starts_with(RAW_COLUMN_PREFIX))
        {
            return Vec::new();
        }
        // Filtered out of `columns` rather than concatenated, so the list stays
        // in `attnum` order and a key column that is ALSO a raw sibling appears
        // once.
        self.columns
            .iter()
            .filter(|column| {
                column.starts_with(RAW_COLUMN_PREFIX) || self.key_columns.contains(column)
            })
            .map(String::as_str)
            .collect()
    }
}

/// Every creator table in one schema, its live columns, and which of them the
/// `PRIMARY KEY` covers.
///
/// Scoped by `nspname` in the query rather than filtered afterwards, and by
/// relation kind: a partition child is reached through its top-level parent, so
/// granting on it separately would describe a surface no statement addresses.
///
/// The key flag is a `LEFT JOIN` and not a second query: a column has to be
/// reported whether or not it is in the key, and a table has to be reported
/// whether or not it has one.
const fn creator_table_column_query() -> &'static str {
    "SELECT c.relname AS table_name,
            a.attname AS column_name,
            (key.indexrelid IS NOT NULL) AS is_key
       FROM pg_class AS c
       JOIN pg_namespace AS n ON n.oid = c.relnamespace
       JOIN pg_attribute AS a ON a.attrelid = c.oid
       LEFT JOIN pg_index AS key
              ON key.indrelid = c.oid
             AND key.indisprimary
             AND a.attnum = ANY(key.indkey::int2[])
      WHERE n.nspname = $1
        AND c.relkind IN ('r', 'p')
        AND NOT c.relispartition
        AND left(c.relname, length($2::text)) <> $2::text
        AND a.attnum > 0
        AND NOT a.attisdropped
      ORDER BY c.relname, a.attnum"
}

/// Read one schema's creator tables, their columns and their primary keys.
///
/// # Errors
/// [`CapabilityGrantError::Query`] on any read failure.
pub async fn creator_tables(
    admin: &Client,
    schema: &str,
) -> Result<Vec<CreatorTable>, CapabilityGrantError> {
    let rows = admin
        .query(
            creator_table_column_query(),
            &[&schema, &PLATFORM_TABLE_PREFIX],
        )
        .await?;
    let mut tables: Vec<CreatorTable> = Vec::new();
    for row in &rows {
        let table: String = row.get("table_name");
        let column: String = row.get("column_name");
        let is_key: bool = row.get("is_key");
        match tables.last_mut() {
            Some(last) if last.name == table => {
                if is_key {
                    last.key_columns.push(column.clone());
                }
                last.columns.push(column);
            }
            _ => tables.push(CreatorTable {
                name: table,
                key_columns: if is_key {
                    vec![column.clone()]
                } else {
                    Vec::new()
                },
                columns: vec![column],
            }),
        }
    }
    Ok(tables)
}

/// A parenthesised, quoted column list, or `None` when there is nothing to
/// list.
///
/// `GRANT SELECT () ON ...` is a syntax error, so an empty list has to suppress
/// its whole statement rather than render as an empty one.
fn column_list(columns: &[&str]) -> Option<String> {
    if columns.is_empty() {
        return None;
    }
    Some(
        columns
            .iter()
            .map(|column| quote_ident(column))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// The statements that converge one schema's capability grants.
///
/// Empty when the schema holds no creator table, which is the state of a
/// database whose only migration created nothing.
///
/// # Every statement re-runs
///
/// `REVOKE ALL` clears the table's column privileges as well as its table-level
/// ones, so the re-grant that follows is the whole of the desired state rather
/// than a delta over whatever the last apply left. A repeated `GRANT` of the
/// same column list replaces its ACL entry rather than adding one, so nothing
/// accumulates either way.
#[must_use]
pub fn capability_column_grants_sql(
    schema: &str,
    tables: &[CreatorTable],
    readwrite: &str,
    readonly: &str,
    unmask: &str,
) -> String {
    let schema_q = quote_ident(schema);
    let readwrite_q = quote_ident(readwrite);
    let readonly_q = quote_ident(readonly);
    let unmask_q = quote_ident(unmask);
    let mut statements: Vec<String> = Vec::new();
    for table in tables {
        let table_q = format!("{schema_q}.{}", quote_ident(&table.name));
        statements.push(format!(
            "REVOKE ALL ON {table_q} FROM {readwrite_q}, {readonly_q}, {unmask_q}"
        ));
        let readable = column_list(&table.readable());
        let writable = column_list(&table.writable());
        let unmaskable = column_list(&table.unmaskable());
        let mut readwrite_privileges: Vec<String> = Vec::new();
        if let Some(readable) = readable.as_deref() {
            readwrite_privileges.push(format!("SELECT ({readable})"));
        }
        if let Some(writable) = writable.as_deref() {
            readwrite_privileges.push(format!("INSERT ({writable})"));
            readwrite_privileges.push(format!("UPDATE ({writable})"));
        }
        if !readwrite_privileges.is_empty() {
            statements.push(format!(
                "GRANT {} ON {table_q} TO {readwrite_q}",
                readwrite_privileges.join(", ")
            ));
        }
        // A DELETE names no column, so the row it removes discloses nothing the
        // column lists above withheld. PostgreSQL has no column form for it.
        statements.push(format!("GRANT DELETE ON {table_q} TO {readwrite_q}"));
        if let Some(readable) = readable.as_deref() {
            statements.push(format!(
                "GRANT SELECT ({readable}) ON {table_q} TO {readonly_q}"
            ));
        }
        // The complement, and only the complement. A table with no masked field
        // emits nothing here, so the unmask role holds no privilege at all on
        // it - `column_list` suppresses its own statement rather than rendering
        // an empty list.
        if let Some(unmaskable) = unmaskable.as_deref() {
            statements.push(format!(
                "GRANT SELECT ({unmaskable}) ON {table_q} TO {unmask_q}"
            ));
        }
    }
    statements.join(";\n")
}

/// Converge one database's capability grants over every creator table it holds.
///
/// Runs on the privileged migration connection, which reaches the tables as an
/// inheriting member of the schema's owning migrator role - the membership
/// `datastore::cluster::converge_database` established. The whole emission is
/// one multi-statement `batch_execute`, which `PostgreSQL` runs as a single
/// implicit transaction, so a failure part way through leaves every table's ACL
/// exactly as it was and the next apply converges it.
///
/// # Errors
/// [`CapabilityGrantError::RoleName`] on a name `PostgreSQL` would truncate,
/// [`CapabilityGrantError::Query`] on any read or DDL failure.
pub async fn grant_capability_columns(
    admin: &Client,
    database: &DatabaseId,
) -> Result<(), CapabilityGrantError> {
    let schema = database_derivation::schema_name(database);
    let readwrite =
        database_derivation::capability_role_name(database, DatabaseCapability::ReadWrite)?;
    let readonly =
        database_derivation::capability_role_name(database, DatabaseCapability::ReadOnly)?;
    let unmask = database_derivation::unmask_role_name(database)?;
    let tables = creator_tables(admin, &schema).await?;
    let sql = capability_column_grants_sql(&schema, &tables, &readwrite, &readonly, &unmask);
    if sql.is_empty() {
        return Ok(());
    }
    exec_retry(admin, &sql).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provisioning::AUDIT_UNMASK_TABLE;

    /// A table whose FIRST column is its primary key, which is the shape the
    /// `id TEXT PRIMARY KEY` contract gives every creator collection.
    fn table(name: &str, columns: &[&str]) -> CreatorTable {
        CreatorTable {
            name: name.to_owned(),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            key_columns: columns
                .first()
                .map(|c| (*c).to_owned())
                .into_iter()
                .collect(),
        }
    }

    /// A table with no primary key at all, for the arm that has to distinguish
    /// "the key rides along" from "every column does".
    fn keyless_table(name: &str, columns: &[&str]) -> CreatorTable {
        CreatorTable {
            name: name.to_owned(),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            key_columns: Vec::new(),
        }
    }

    /// The table this crate excludes by prefix is one the prefix actually
    /// covers. Two constants that drifted apart would leave the audit table
    /// swept as a creator table and its `INSERT`-only grant replaced by a
    /// `SELECT` over every actor's audit trail.
    #[test]
    fn the_audit_table_is_a_platform_table() {
        assert!(
            AUDIT_UNMASK_TABLE.starts_with(PLATFORM_TABLE_PREFIX),
            "{AUDIT_UNMASK_TABLE} must be excluded by {PLATFORM_TABLE_PREFIX}"
        );
    }

    /// The real-value sibling is withheld from the read set and kept in the
    /// write set, and every other column is in both.
    #[test]
    fn a_masked_fields_real_value_column_is_writable_and_never_readable() {
        let raw = format!("{RAW_COLUMN_PREFIX}ssn");
        let subject = table("people", &["id", "ssn", raw.as_str(), "nickname"]);
        assert_eq!(
            subject.readable(),
            vec!["id", "ssn", "nickname"],
            "the mask-bearing column stays readable and the real value does not"
        );
        assert_eq!(
            subject.writable(),
            vec!["id", "ssn", raw.as_str(), "nickname"],
            "the write pass dual-binds both halves, so both are writable"
        );
        assert!(
            subject.readable().len() < subject.writable().len(),
            "the control: a partition that withheld nothing would satisfy both \
             assertions above if the fixture carried no raw column"
        );
        assert_eq!(
            subject.unmaskable(),
            vec!["id", raw.as_str()],
            "the unmask set is the column the read set withheld plus the key \
             that addresses one row, and nothing else"
        );
    }

    /// The unmask set is every raw sibling plus the key, and no payload column
    /// beside them.
    ///
    /// The key is in the set because the read is `WHERE id = $1` and
    /// `PostgreSQL` checks `SELECT` on a column a predicate references. The arm
    /// that makes that a narrowing rather than a licence is the KEYLESS table
    /// beside it: on the same columns with no primary key the set is the raw
    /// siblings alone, so the key is riding in because it IS the key and not
    /// because the filter has gone slack.
    #[test]
    fn the_unmask_set_is_the_raw_siblings_and_the_key_that_addresses_a_row() {
        let raw = format!("{RAW_COLUMN_PREFIX}ssn");
        let other = format!("{RAW_COLUMN_PREFIX}dob");
        let columns = ["id", "ssn", raw.as_str(), "dob", other.as_str(), "nickname"];

        let subject = table("people", &columns);
        assert_eq!(
            subject.unmaskable(),
            vec!["id", raw.as_str(), other.as_str()],
            "every raw sibling and the key, in attnum order"
        );
        for withheld in ["ssn", "dob", "nickname"] {
            assert!(
                !subject.unmaskable().contains(&withheld),
                "the unmask role must not reach the payload column {withheld}"
            );
        }

        let keyless = keyless_table("people", &columns);
        assert_eq!(
            keyless.unmaskable(),
            vec![raw.as_str(), other.as_str()],
            "with no primary key the set is the raw siblings alone: the key rides \
             in because it is the key"
        );
    }

    /// Read and unmask cover every writable column between them, overlapping in
    /// the key alone.
    ///
    /// Stated over the whole table because that is the property the design
    /// rests on: a column in neither would be unreachable, and a payload column
    /// in both would hand a narrowed session the plaintext. Neither shows up in
    /// an assertion that only asks whether the raw column is in the unmask list.
    #[test]
    fn read_and_unmask_cover_every_column_and_overlap_only_in_the_key() {
        let raw = format!("{RAW_COLUMN_PREFIX}ssn");
        let subject = table("people", &["id", "ssn", raw.as_str(), "nickname"]);

        let mut covered = subject.readable();
        covered.extend(subject.unmaskable());
        covered.sort_unstable();
        covered.dedup();
        let mut writable = subject.writable();
        writable.sort_unstable();
        assert_eq!(covered, writable, "no column may fall out of both sets");
        assert!(
            !subject.readable().is_empty() && !subject.unmaskable().is_empty(),
            "the control: two empty sets would satisfy the equality above if the \
             write set were empty too"
        );

        let shared: Vec<&str> = subject
            .readable()
            .into_iter()
            .filter(|column| subject.unmaskable().contains(column))
            .collect();
        assert_eq!(
            shared, subject.key_columns,
            "the key is the only column both sets hold"
        );
    }

    /// A table with no masked field has the same read and write sets and an
    /// EMPTY unmask set, which is the control for the case above: without it, a
    /// `readable` that returned everything would pass here and fail nothing,
    /// and an `unmaskable` that returned everything would too.
    #[test]
    fn a_table_with_no_masked_field_reads_and_writes_the_same_columns() {
        let subject = table("notes", &["id", "title", "body"]);
        assert_eq!(subject.readable(), subject.writable());
        assert_eq!(subject.readable().len(), 3);
        assert!(
            subject.unmaskable().is_empty(),
            "a table with no masked field gives the unmask role nothing: {:?}",
            subject.unmaskable()
        );
    }

    /// The emission never grants a bare table-level `SELECT`, `INSERT` or
    /// `UPDATE`, because one beside a column list widens the column list
    /// instead of narrowing it.
    #[test]
    fn no_read_or_write_privilege_is_granted_at_the_table() {
        let raw = format!("{RAW_COLUMN_PREFIX}ssn");
        let sql = capability_column_grants_sql(
            "db_x",
            &[table("people", &["id", raw.as_str()])],
            "rw",
            "ro",
            "un",
        );
        for widening in ["SELECT ON", "INSERT ON", "UPDATE ON"] {
            assert!(
                !sql.contains(widening),
                "a table-level `{widening}` would restore the withheld column: {sql}"
            );
        }
        assert!(
            sql.contains("GRANT DELETE ON"),
            "DELETE has no column form and must stay at the table: {sql}"
        );
    }

    /// Every column list a privilege KEYWORD introduces, as it was rendered.
    ///
    /// The `_rw` grant carries `SELECT`, `INSERT` and `UPDATE` in one statement,
    /// so a check that looked at the whole line would see the write list's
    /// columns inside the read arm and vice versa. Splitting on the keyword is
    /// what makes "no SELECT names the real value" a statement about the read
    /// list rather than about the statement it happens to share.
    fn lists_for(sql: &str, privilege: &str) -> Vec<String> {
        let needle = format!("{privilege} (");
        sql.match_indices(&needle)
            .map(|(at, _)| {
                let rest = &sql[at + needle.len()..];
                let close = rest
                    .find(')')
                    .unwrap_or_else(|| panic!("an unterminated {privilege} list in {sql}"));
                rest[..close].to_owned()
            })
            .collect()
    }

    /// Every column list one ROLE is granted, as it was rendered.
    ///
    /// `lists_for` splits by privilege keyword, which is what the arms about the
    /// read and write lists need. The unmask arms ask a different question -
    /// which role a list was granted TO - and the answer is not derivable from
    /// the keyword, because the unmask grant and the readonly grant are both
    /// `GRANT SELECT (...)`.
    fn select_lists_granted_to(sql: &str, role: &str) -> Vec<String> {
        // Every statement but the last carries the `;` the join inserted.
        let recipient = format!(" TO \"{role}\"");
        sql.lines()
            .map(|line| line.strip_suffix(';').unwrap_or(line))
            .filter(|line| line.ends_with(&recipient))
            .filter_map(|line| {
                // The readwrite statement carries SELECT, INSERT and UPDATE in
                // one GRANT, so the list has to be cut out of the middle of the
                // line rather than off its front.
                let opened = line.find("SELECT (")? + "SELECT (".len();
                let rest = &line[opened..];
                let close = rest.find(')')?;
                Some(rest[..close].to_owned())
            })
            .collect()
    }

    /// The real-value column appears in the write lists and in no capability
    /// role's read list, and it is what the unmask role is granted.
    #[test]
    fn the_generated_statements_withhold_the_real_value_column_from_every_select() {
        let raw = format!("{RAW_COLUMN_PREFIX}ssn");
        let sql = capability_column_grants_sql(
            "db_x",
            &[table("people", &["id", "ssn", raw.as_str()])],
            "zs_db_x_rw",
            "zs_db_x_ro",
            "zs_db_x_unmask",
        );
        let reads = lists_for(&sql, "SELECT");
        assert_eq!(
            reads.len(),
            3,
            "one read list for each capability, plus the unmask role's: {sql}"
        );
        for capability in ["zs_db_x_rw", "zs_db_x_ro"] {
            assert_eq!(
                select_lists_granted_to(&sql, capability),
                vec!["\"id\", \"ssn\"".to_owned()],
                "{capability}'s read list names the mask-bearing column and not \
                 the real value: {sql}"
            );
        }
        assert_eq!(
            select_lists_granted_to(&sql, "zs_db_x_unmask"),
            vec![format!("\"id\", \"{raw}\"")],
            "the unmask role is granted the real-value column and the key that \
             addresses one row, and no other column: {sql}"
        );
        let writes = lists_for(&sql, "INSERT");
        writes
            .iter()
            .chain(&lists_for(&sql, "UPDATE"))
            .for_each(|write| {
                assert_eq!(
                    write,
                    &format!("\"id\", \"ssn\", \"{raw}\""),
                    "a write list carries the real-value column"
                );
            });
        assert_eq!(
            writes.len(),
            1,
            "only the readwrite capability writes: {sql}"
        );
    }

    /// The readonly role gets reads and nothing else.
    #[test]
    fn the_readonly_capability_is_granted_no_write_verb() {
        let sql = capability_column_grants_sql(
            "db_x",
            &[table("notes", &["id", "title"])],
            "rw",
            "ro",
            "un",
        );
        for line in sql.lines().filter(|line| line.contains("TO \"ro\"")) {
            assert!(
                line.contains("GRANT SELECT (") || line.starts_with("REVOKE ALL"),
                "the readonly capability holds only column-listed SELECT: {line}"
            );
        }
        assert!(
            sql.lines()
                .any(|line| line.contains("GRANT SELECT (") && line.contains("TO \"ro\"")),
            "the arm must not pass over an empty set: {sql}"
        );
    }

    /// The unmask role is granted a `SELECT` and no other verb, and on a table
    /// with no masked field it is granted nothing beyond the revoke.
    ///
    /// The second half is the control the first needs: a table whose only
    /// statement for this role is `REVOKE ALL` would satisfy "no write verb"
    /// for the wrong reason, so the masked fixture beside it has to show the
    /// grant actually being emitted.
    #[test]
    fn the_unmask_role_is_granted_one_select_and_no_other_verb() {
        let raw = format!("{RAW_COLUMN_PREFIX}ssn");
        let masked = capability_column_grants_sql(
            "db_x",
            &[table("people", &["id", "ssn", raw.as_str()])],
            "rw",
            "ro",
            "un",
        );
        let granted: Vec<&str> = masked
            .lines()
            .filter(|line| line.contains("TO \"un\"") && !line.starts_with("REVOKE ALL"))
            .collect();
        assert_eq!(
            granted,
            vec![format!(
                "GRANT SELECT (\"id\", \"{raw}\") ON \"db_x\".\"people\" TO \"un\""
            )],
            "the unmask role's whole grant is one column-listed SELECT: {masked}"
        );

        // THE CONTROL, differing in one variable: the same emission over a
        // table with no masked field.
        let unmasked = capability_column_grants_sql(
            "db_x",
            &[table("notes", &["id", "title"])],
            "rw",
            "ro",
            "un",
        );
        assert!(
            unmasked
                .lines()
                .filter(|line| line.contains("TO \"un\""))
                .all(|line| line.starts_with("REVOKE ALL")),
            "a table with no masked field must leave the unmask role holding \
             nothing on it: {unmasked}"
        );
        assert!(
            unmasked.contains("REVOKE ALL ON \"db_x\".\"notes\" FROM \"rw\", \"ro\", \"un\""),
            "the revoke must still name the unmask role, or a reclassification \
             that removed a masked field would leave its grant standing: {unmasked}"
        );
    }

    /// Every table is revoked before it is re-granted, which is what makes a
    /// re-run converge rather than accumulate.
    #[test]
    fn every_table_is_revoked_before_it_is_granted() {
        let tables = [table("a", &["id"]), table("b", &["id"])];
        let sql = capability_column_grants_sql("db_x", &tables, "rw", "ro", "un");
        for name in ["a", "b"] {
            let revoke = sql
                .find(&format!("REVOKE ALL ON \"db_x\".\"{name}\""))
                .unwrap_or_else(|| panic!("`{name}` must be revoked: {sql}"));
            let grant = sql
                .find(&format!("GRANT DELETE ON \"db_x\".\"{name}\""))
                .unwrap_or_else(|| panic!("`{name}` must be granted: {sql}"));
            assert!(revoke < grant, "the revoke must precede the grant: {sql}");
        }
    }

    /// A schema with no creator table emits nothing, so the executor sends no
    /// statement at all.
    #[test]
    fn a_schema_with_no_creator_table_emits_no_statement() {
        assert!(capability_column_grants_sql("db_x", &[], "rw", "ro", "un").is_empty());
    }

    /// Identifiers reach the statement through `quote_ident`, which doubles an
    /// embedded quote. Table and column names come from the catalog, and a
    /// creator names both.
    #[test]
    fn catalog_identifiers_are_quoted_not_interpolated_raw() {
        let sql = capability_column_grants_sql(
            "db_x",
            &[table("t\"; DROP SCHEMA public; --", &["c\"x"])],
            "rw",
            "ro",
            "un",
        );
        assert!(
            sql.contains(r#""t""; DROP SCHEMA public; --""#),
            "the table name must survive as ONE doubled-quote identifier: {sql}"
        );
        assert!(
            sql.contains(r#""c""x""#),
            "the column name must survive as ONE doubled-quote identifier: {sql}"
        );
        assert!(
            !sql.contains("; DROP SCHEMA public; --\"\n"),
            "no statement boundary may be reachable from a catalog name: {sql}"
        );
    }
}

/// The grant set measured against a live server rather than against the text of
/// the statements that ask for it.
///
/// The unit tests above read the generator's output. Whether `PostgreSQL` then
/// answers `permission denied` for the withheld column, and whether a second
/// emission changes anything, are the server's questions.
#[cfg(test)]
mod live_capability_column_grants {
    use super::*;
    use uuid::Uuid;

    async fn admin_client() -> Client {
        crate::test_database::connect().await
    }

    fn is_insufficient_privilege(err: &compio_postgres::Error) -> bool {
        err.code() == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    }

    /// A throwaway schema, its owning role, the two capability roles and the
    /// unmask role, in the shape `datastore::cluster::converge_database` leaves
    /// behind: the schema owned by the migrator, the other three holding
    /// `USAGE` and nothing else.
    struct Converged {
        schema: String,
        migrator: String,
        readwrite: String,
        readonly: String,
        unmask: String,
    }

    impl Converged {
        async fn create(admin: &Client) -> Self {
            let unique = Uuid::new_v4().simple().to_string();
            let converged = Self {
                schema: format!("capgrant_{unique}"),
                migrator: format!("capgrant_{unique}_mig"),
                readwrite: format!("capgrant_{unique}_rw"),
                readonly: format!("capgrant_{unique}_ro"),
                unmask: format!("capgrant_{unique}_unmask"),
            };
            let schema_q = quote_ident(&converged.schema);
            let migrator_q = quote_ident(&converged.migrator);
            let readwrite_q = quote_ident(&converged.readwrite);
            let readonly_q = quote_ident(&converged.readonly);
            let unmask_q = quote_ident(&converged.unmask);
            admin
                .batch_execute(&format!(
                    "CREATE ROLE {migrator_q} NOLOGIN INHERIT;
                     CREATE ROLE {readwrite_q} NOLOGIN INHERIT;
                     CREATE ROLE {readonly_q} NOLOGIN INHERIT;
                     CREATE ROLE {unmask_q} NOLOGIN INHERIT;
                     CREATE SCHEMA {schema_q} AUTHORIZATION {migrator_q};
                     GRANT USAGE ON SCHEMA {schema_q} \
                       TO {readwrite_q}, {readonly_q}, {unmask_q};"
                ))
                .await
                .expect("converge the fixture schema and its four roles");
            converged
        }

        /// Create one table as the migrator, exactly as an apply's DDL does.
        async fn create_table(&self, admin: &Client, table: &str, body: &str) {
            admin
                .batch_execute(&format!(
                    "SET ROLE {}; CREATE TABLE {}.{} ({body}); RESET ROLE",
                    quote_ident(&self.migrator),
                    quote_ident(&self.schema),
                    quote_ident(table),
                ))
                .await
                .expect("create the fixture table as the schema's owner");
        }

        /// Run `sql` narrowed to one capability role, and leave the session
        /// clean either way.
        async fn under(
            &self,
            admin: &Client,
            role: &str,
            sql: &str,
        ) -> Result<(), compio_postgres::Error> {
            admin
                .batch_execute(&format!("SET ROLE {}", quote_ident(role)))
                .await
                .expect("narrow to the capability role");
            let result = admin.batch_execute(sql).await;
            admin
                .batch_execute("RESET ROLE")
                .await
                .expect("widen back after the probe");
            result
        }

        async fn may(
            &self,
            admin: &Client,
            role: &str,
            table: &str,
            column: &str,
            privilege: &str,
        ) -> bool {
            admin
                .query_one(
                    "SELECT has_column_privilege($1, format('%I.%I', $2::text, $3::text), $4, $5) \
                     AS granted",
                    &[&role, &self.schema, &table, &column, &privilege],
                )
                .await
                .expect("ask PostgreSQL for the column privilege")
                .get("granted")
        }

        async fn teardown(&self, admin: &Client) {
            let _ = admin
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE",
                    quote_ident(&self.schema)
                ))
                .await;
            for role in [
                &self.readwrite,
                &self.readonly,
                &self.unmask,
                &self.migrator,
            ] {
                let q = quote_ident(role);
                let _ = admin
                    .batch_execute(&format!("DROP OWNED BY {q} CASCADE"))
                    .await;
                let _ = admin
                    .batch_execute(&format!("DROP ROLE IF EXISTS {q}"))
                    .await;
            }
        }
    }

    /// The emission is what a converged database's capability roles then hold,
    /// asked of `PostgreSQL` and exercised as statements.
    #[compio::test]
    async fn the_emission_grants_the_mask_and_withholds_the_real_value() {
        let admin = admin_client().await;
        let fixture = Converged::create(&admin).await;
        let raw = format!("{RAW_COLUMN_PREFIX}ssn");
        fixture
            .create_table(
                &admin,
                "people",
                &format!(
                    "id text PRIMARY KEY, ssn text, {} bytea, nickname text",
                    quote_ident(&raw)
                ),
            )
            .await;
        // THE CONTROL, taken before the emission: the capability roles reach
        // nothing, so every assertion below is about what this emission did.
        assert!(
            !fixture
                .may(&admin, &fixture.readwrite, "people", "id", "SELECT")
                .await,
            "a converged database must hold USAGE and no column privilege"
        );

        let tables = creator_tables(&admin, &fixture.schema)
            .await
            .expect("read the fixture's creator tables");
        assert_eq!(
            tables,
            vec![CreatorTable {
                name: "people".to_owned(),
                columns: vec![
                    "id".to_owned(),
                    "ssn".to_owned(),
                    raw.clone(),
                    "nickname".to_owned(),
                ],
                // Read from the live catalog's own `pg_index`, not declared by
                // the fixture: the DDL above says `id text PRIMARY KEY` and
                // this is the server reporting it back.
                key_columns: vec!["id".to_owned()],
            }],
            "the catalog read must return the table, its columns in attnum order \
             and the primary key it actually has"
        );
        let sql = capability_column_grants_sql(
            &fixture.schema,
            &tables,
            &fixture.readwrite,
            &fixture.readonly,
            &fixture.unmask,
        );
        exec_retry(&admin, &sql)
            .await
            .expect("the emission applies");

        for column in ["id", "ssn", "nickname"] {
            assert!(
                fixture
                    .may(&admin, &fixture.readwrite, "people", column, "SELECT")
                    .await,
                "the readwrite capability must read {column}"
            );
            assert!(
                fixture
                    .may(&admin, &fixture.readonly, "people", column, "SELECT")
                    .await,
                "the readonly capability must read {column}"
            );
        }
        assert!(
            !fixture
                .may(&admin, &fixture.readwrite, "people", &raw, "SELECT")
                .await,
            "the real-value column must be unreadable by the readwrite capability"
        );
        assert!(
            !fixture
                .may(&admin, &fixture.readonly, "people", &raw, "SELECT")
                .await,
            "the real-value column must be unreadable by the readonly capability"
        );
        for privilege in ["INSERT", "UPDATE"] {
            assert!(
                fixture
                    .may(&admin, &fixture.readwrite, "people", &raw, privilege)
                    .await,
                "the write pass dual-binds the real value, so it needs {privilege}"
            );
        }

        let schema_q = quote_ident(&fixture.schema);
        fixture
            .under(
                &admin,
                &fixture.readwrite,
                &format!(
                    "INSERT INTO {schema_q}.\"people\" (id, ssn, {}, nickname) \
                     VALUES ('p1', '***', '\\x01', 'nick')",
                    quote_ident(&raw)
                ),
            )
            .await
            .expect("a readwrite session writes both halves of a masked field");
        fixture
            .under(
                &admin,
                &fixture.readwrite,
                &format!("SELECT id, ssn, nickname FROM {schema_q}.\"people\""),
            )
            .await
            .expect("a readwrite session reads the mask");
        let denied = fixture
            .under(
                &admin,
                &fixture.readwrite,
                &format!("SELECT {} FROM {schema_q}.\"people\"", quote_ident(&raw)),
            )
            .await
            .expect_err("the real value must not be selectable");
        assert!(
            is_insufficient_privilege(&denied),
            "expected 42501 on the withheld column, got {denied}"
        );
        let starred = fixture
            .under(
                &admin,
                &fixture.readwrite,
                &format!("SELECT * FROM {schema_q}.\"people\""),
            )
            .await
            .expect_err("a star projection reaches the withheld column");
        assert!(
            is_insufficient_privilege(&starred),
            "expected 42501 on SELECT *, got {starred}"
        );

        // THE NEGATIVE ARM, differing from the permitted case in one variable:
        // the readonly capability, on the same statement.
        let readonly_write = fixture
            .under(
                &admin,
                &fixture.readonly,
                &format!("INSERT INTO {schema_q}.\"people\" (id, nickname) VALUES ('p2', 'no')"),
            )
            .await
            .expect_err("the readonly capability must not write");
        assert!(
            is_insufficient_privilege(&readonly_write),
            "expected 42501 on a readonly write, got {readonly_write}"
        );
        let readonly_delete = fixture
            .under(
                &admin,
                &fixture.readonly,
                &format!("DELETE FROM {schema_q}.\"people\""),
            )
            .await
            .expect_err("the readonly capability must not delete");
        assert!(
            is_insufficient_privilege(&readonly_delete),
            "expected 42501 on a readonly delete, got {readonly_delete}"
        );

        // THE UNMASK ROLE, on the same table and the same emission. It is the
        // complement of the two above: the one column they are refused is the
        // one column it holds.
        assert!(
            fixture
                .may(&admin, &fixture.unmask, "people", &raw, "SELECT")
                .await,
            "the unmask role must read the real value, or the audited dispatcher \
             has no path to it"
        );
        // The read the dispatcher actually issues: ONE row, addressed by its
        // primary key. `PostgreSQL` checks `SELECT` on a column a predicate
        // references and not only on the projection, so a grant covering the
        // real value alone would be refused here on a column it never returns.
        fixture
            .under(
                &admin,
                &fixture.unmask,
                &format!(
                    "SELECT {} FROM {schema_q}.\"people\" WHERE id = 'p1'",
                    quote_ident(&raw)
                ),
            )
            .await
            .expect("the unmask role reads one row's real value");
        // The payload columns beside it stay refused, which is what makes the
        // key's presence a narrowing rather than a licence.
        for column in ["ssn", "nickname"] {
            assert!(
                !fixture
                    .may(&admin, &fixture.unmask, "people", column, "SELECT")
                    .await,
                "the unmask role must not read {column}: assuming it narrows the \
                 session, it does not widen it"
            );
            let denied_column = fixture
                .under(
                    &admin,
                    &fixture.unmask,
                    &format!("SELECT {} FROM {schema_q}.\"people\"", quote_ident(column)),
                )
                .await
                .expect_err("the unmask role must be refused a column it was not granted");
            assert!(
                is_insufficient_privilege(&denied_column),
                "expected 42501 on {column} under the unmask role, got {denied_column}"
            );
        }
        let starred_unmask = fixture
            .under(
                &admin,
                &fixture.unmask,
                &format!("SELECT * FROM {schema_q}.\"people\""),
            )
            .await
            .expect_err("a star projection under the unmask role reaches the mask");
        assert!(
            is_insufficient_privilege(&starred_unmask),
            "expected 42501 on SELECT * under the unmask role, got {starred_unmask}"
        );
        // `has_column_privilege` answers only for the privileges that HAVE a
        // column form, so the two that do are asked of the catalog as well as
        // exercised, and `DELETE` - which names no column - is exercised alone.
        for privilege in ["INSERT", "UPDATE"] {
            assert!(
                !fixture
                    .may(&admin, &fixture.unmask, "people", &raw, privilege)
                    .await,
                "the unmask role must hold no {privilege} on the real value"
            );
        }
        for (privilege, statement) in [
            (
                "INSERT",
                format!("INSERT INTO {schema_q}.\"people\" (id) VALUES ('p3')"),
            ),
            (
                "UPDATE",
                format!(
                    "UPDATE {schema_q}.\"people\" SET {} = '\\x02'",
                    quote_ident(&raw)
                ),
            ),
            ("DELETE", format!("DELETE FROM {schema_q}.\"people\"")),
        ] {
            let refused = fixture
                .under(&admin, &fixture.unmask, &statement)
                .await
                .expect_err("the unmask role must not write");
            assert!(
                is_insufficient_privilege(&refused),
                "expected 42501 on a {privilege} under the unmask role, got {refused}"
            );
        }

        fixture.teardown(&admin).await;
    }

    /// Every live column of one table and the ACL `PostgreSQL` records for it.
    async fn column_acls(
        admin: &Client,
        schema: &str,
        table: &str,
    ) -> Vec<(String, Option<String>)> {
        admin
            .query(
                "SELECT a.attname AS name, a.attacl::text AS acl \
                   FROM pg_attribute a \
                   JOIN pg_class c ON c.oid = a.attrelid \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = $1 AND c.relname = $2 \
                    AND a.attnum > 0 AND NOT a.attisdropped \
                  ORDER BY a.attnum",
                &[&schema, &table],
            )
            .await
            .expect("read the column ACLs")
            .iter()
            .map(|row| {
                (
                    row.get::<_, String>("name"),
                    row.get::<_, Option<String>>("acl"),
                )
            })
            .collect()
    }

    /// One convergence pass over the fixture, through the same two functions
    /// [`grant_capability_columns`] composes.
    async fn emit(admin: &Client, fixture: &Converged) {
        let tables = creator_tables(admin, &fixture.schema)
            .await
            .expect("read the fixture's creator tables");
        assert!(
            !tables.is_empty(),
            "the emission must have input to converge"
        );
        let sql = capability_column_grants_sql(
            &fixture.schema,
            &tables,
            &fixture.readwrite,
            &fixture.readonly,
            &fixture.unmask,
        );
        exec_retry(admin, &sql).await.expect("the emission applies");
    }

    /// A second emission over the same schema changes nothing, and a column
    /// that BECOMES a masked field's real value between two applies loses the
    /// read privilege its earlier shape had.
    #[compio::test]
    async fn a_second_emission_converges_rather_than_accumulating() {
        let admin = admin_client().await;
        let fixture = Converged::create(&admin).await;
        fixture
            .create_table(&admin, "notes", "id text PRIMARY KEY, title text")
            .await;

        emit(&admin, &fixture).await;
        let first = column_acls(&admin, &fixture.schema, "notes").await;
        assert!(
            first.iter().all(|(_, acl)| acl.is_some()),
            "every column must carry an ACL after the first emission: {first:?}"
        );
        emit(&admin, &fixture).await;
        assert_eq!(
            column_acls(&admin, &fixture.schema, "notes").await,
            first,
            "a second emission must not change or accumulate a single ACL entry"
        );
        // THE BEFORE for the reclassification below: this table has no masked
        // field yet, so the unmask role holds nothing on any of its columns.
        for column in ["id", "title"] {
            assert!(
                !fixture
                    .may(&admin, &fixture.unmask, "notes", column, "SELECT")
                    .await,
                "a table with no masked field must leave the unmask role holding \
                 nothing on {column}"
            );
        }

        // A later migration ALTERS the table twice over: it adds an ordinary
        // column, and it turns `title` into a masked field - the mask stays in
        // `title` and the real value moves to the raw sibling. The emission has
        // to grant the ordinary one, withhold the raw one, and leave `title`
        // readable. THE ORDINARY COLUMN IS THE CONTROL: without it, an emission
        // that stopped covering added columns altogether would satisfy the
        // withholding arm below for the wrong reason.
        let raw = format!("{RAW_COLUMN_PREFIX}title");
        admin
            .batch_execute(&format!(
                "SET ROLE {}; \
                 ALTER TABLE {schema}.\"notes\" ADD COLUMN \"body\" text; \
                 ALTER TABLE {schema}.\"notes\" ADD COLUMN {} text; \
                 RESET ROLE",
                quote_ident(&fixture.migrator),
                quote_ident(&raw),
                schema = quote_ident(&fixture.schema),
            ))
            .await
            .expect("alter the table as the schema's owner");
        assert!(
            !fixture
                .may(&admin, &fixture.readwrite, "notes", "body", "SELECT")
                .await,
            "an added column carries no privilege until the emission runs again"
        );
        emit(&admin, &fixture).await;
        assert!(
            fixture
                .may(&admin, &fixture.readwrite, "notes", "body", "SELECT")
                .await,
            "a column an ALTER added must be reachable after the next apply"
        );
        assert!(
            fixture
                .may(&admin, &fixture.readwrite, "notes", "title", "SELECT")
                .await,
            "the mask-bearing column stays readable"
        );
        assert!(
            !fixture
                .may(&admin, &fixture.readwrite, "notes", &raw, "SELECT")
                .await,
            "a column added by a later migration is withheld too"
        );
        assert!(
            fixture
                .may(&admin, &fixture.readwrite, "notes", &raw, "INSERT")
                .await,
            "and it is still writable, or no write could store the real value"
        );
        // The reclassification moves the privilege rather than deleting it: the
        // column the capability roles just lost is the one the unmask role now
        // holds, and it held nothing on this table before the ALTER.
        assert!(
            fixture
                .may(&admin, &fixture.unmask, "notes", &raw, "SELECT")
                .await,
            "a column that BECAME a real value must reach the unmask role on the \
             next apply"
        );
        assert!(
            !fixture
                .may(&admin, &fixture.unmask, "notes", "title", "SELECT")
                .await,
            "the control: the unmask role gains only the raw sibling, not the \
             mask-bearing column beside it"
        );

        // A dropped column leaves no privilege behind for a re-added one.
        admin
            .batch_execute(&format!(
                "SET ROLE {}; ALTER TABLE {}.\"notes\" DROP COLUMN {}; RESET ROLE",
                quote_ident(&fixture.migrator),
                quote_ident(&fixture.schema),
                quote_ident(&raw),
            ))
            .await
            .expect("drop the real-value column as the schema's owner");
        emit(&admin, &fixture).await;
        let after_drop = column_acls(&admin, &fixture.schema, "notes").await;
        assert!(
            after_drop.iter().all(|(name, _)| name != &raw),
            "the dropped column must be gone from the live catalog: {after_drop:?}"
        );

        fixture.teardown(&admin).await;
    }

    /// The platform's own tables inside a creator schema are neither granted
    /// nor revoked here.
    #[compio::test]
    async fn a_platform_table_is_not_swept_as_a_creator_table() {
        let admin = admin_client().await;
        let fixture = Converged::create(&admin).await;
        fixture
            .create_table(&admin, "notes", "id text PRIMARY KEY")
            .await;
        fixture
            .create_table(
                &admin,
                crate::provisioning::AUDIT_UNMASK_TABLE,
                "id bigint PRIMARY KEY, collection text",
            )
            .await;
        // The audit table's own grant, as `grant_audit_unmask_to_capabilities`
        // leaves it: INSERT and deliberately no SELECT.
        admin
            .batch_execute(&format!(
                "GRANT INSERT ON {}.{} TO {}, {}",
                quote_ident(&fixture.schema),
                quote_ident(crate::provisioning::AUDIT_UNMASK_TABLE),
                quote_ident(&fixture.readwrite),
                quote_ident(&fixture.readonly),
            ))
            .await
            .expect("seed the audit table's own grant");

        let tables = creator_tables(&admin, &fixture.schema)
            .await
            .expect("read the fixture's creator tables");
        assert_eq!(
            tables.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["notes"],
            "the platform table must not be in the creator set"
        );
        let sql = capability_column_grants_sql(
            &fixture.schema,
            &tables,
            &fixture.readwrite,
            &fixture.readonly,
            &fixture.unmask,
        );
        exec_retry(&admin, &sql)
            .await
            .expect("the emission applies");

        assert!(
            fixture
                .may(
                    &admin,
                    &fixture.readwrite,
                    crate::provisioning::AUDIT_UNMASK_TABLE,
                    "id",
                    "INSERT"
                )
                .await,
            "the audit table's own INSERT grant must survive the emission"
        );
        assert!(
            !fixture
                .may(
                    &admin,
                    &fixture.readwrite,
                    crate::provisioning::AUDIT_UNMASK_TABLE,
                    "collection",
                    "SELECT"
                )
                .await,
            "the emission must not hand a session the audit trail to read back"
        );

        fixture.teardown(&admin).await;
    }
}
