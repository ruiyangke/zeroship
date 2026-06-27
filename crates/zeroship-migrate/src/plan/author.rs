//! The `MigrationAuthor` seam — where SQL *enters* the pipeline (design §3).
//!
//! A migration's `up`/`down` SQL is produced by a pluggable **author**, then
//! handed to the engine for the SAME `plan` (lint) → `gate` (approval) →
//! [`executor::apply`](crate::executor::apply) (guard + least-priv role)
//! treatment regardless of where it came from. Two authors ship in v1:
//!
//! - [`DeterministicAuthor`] — a BOUNDED set of trivial **additive** ops
//!   ([`AuthorRequest`]: create table, add column, create index). Pure
//!   pattern-matching, no AI, project-schema-qualified output, always safe
//!   (non-destructive). It deliberately does **not** attempt renames, drops, or
//!   type changes — those are the AI author's job (design §3, scenarios 9/10).
//! - [`RawSqlAuthor`] — the **AI-author hook**. The AI/builder generates the
//!   `up`/`down` SQL *externally* (the engine never calls an LLM); this author
//!   wraps that pre-generated SQL as a [`Migration`], deriving the destructive /
//!   approval flags from a guard pass so the complex/expand-contract output
//!   enters the pipeline with the right gating. This is how every non-trivial
//!   migration (scenarios 4/6/9/10/13–16) reaches the executor.
//!
//! Both produce a fully-formed [`Migration`] (`UUIDv7` version, checksum, flags);
//! neither bypasses the guard — [`DeterministicAuthor`] emits provably-safe SQL,
//! and [`RawSqlAuthor`] runs the guard to *flag* (not gate — that is the
//! engine's job in [`crate::engine`]) its output.

use crate::guard::{GuardConfig, GuardError, SqlGuard};
use crate::model::migration::{Checksum, Migration, MigrationFlags, MigrationId};

/// A pluggable source of versioned migrations (design §3).
///
/// The executor is fixed; the *author* is the seam. `DeterministicAuthor`
/// handles the trivial additive set; `RawSqlAuthor` is the hook through which an
/// external AI/builder feeds non-trivial migrations into the same gate.
pub trait MigrationAuthor {
    /// Produce the versioned migration(s) for a request.
    ///
    /// # Errors
    /// [`AuthorError`] if the request is malformed (empty names, no columns) or,
    /// for [`RawSqlAuthor`], if the supplied SQL is unparseable by the guard.
    fn author(&self, req: &AuthorRequest) -> Result<Vec<Migration>, AuthorError>;
}

/// A failure to author a migration.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuthorError {
    /// A structured request field was empty/invalid (e.g. no columns).
    #[error("invalid author request: {0}")]
    Invalid(String),
    /// The supplied raw SQL could not be guarded (unparseable). Only
    /// [`RawSqlAuthor`] returns this; it surfaces the guard's parse rejection at
    /// authoring time rather than deferring it to plan.
    #[error("raw SQL rejected by guard: {0}")]
    Guard(#[from] GuardError),
}

/// A column in a [`AuthorRequest::CreateTable`] / [`AuthorRequest::AddColumn`].
///
/// The `ty` is emitted verbatim as the Postgres type (`text`, `bigint`,
/// `timestamptz`, …); the deterministic author does not validate it — an invalid
/// type fails loudly at apply, and the guard already confines the *statement
/// kind*, which is what matters for safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// The column name (emitted as a quoted identifier).
    pub name: String,
    /// The Postgres type, verbatim (`text`, `bigint`, `jsonb`, …).
    pub ty: String,
    /// `true` ⇒ the column is nullable (no `NOT NULL`). Additive-safe.
    pub nullable: bool,
}

/// A structured request for the [`DeterministicAuthor`] — the BOUNDED set of
/// trivial **additive** operations it can emit without AI (design §3, scenarios
/// 1/2/5).
///
/// Every variant is non-destructive by construction; there is deliberately no
/// `DropTable` / `RenameColumn` / `ChangeType` variant — those are destructive
/// or ambiguous and belong to the AI author (fed via [`RawSqlAuthor`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorRequest {
    /// `CREATE TABLE <schema>.<name> (<columns…>)` — scenario 1.
    CreateTable {
        /// The new table's name (project-schema-qualified on emit).
        name: String,
        /// The table's columns (at least one required).
        columns: Vec<Column>,
    },
    /// `ALTER TABLE <schema>.<table> ADD COLUMN <column>` — scenario 2.
    ///
    /// Additive-safe only when `nullable` (an additive NOT-NULL with no default
    /// over existing rows is scenario 4 — the AI author's job).
    AddColumn {
        /// The table to alter (project-schema-qualified on emit).
        table: String,
        /// The column to add.
        column: Column,
    },
    /// `CREATE INDEX [CONCURRENTLY] IF NOT EXISTS … ON <schema>.<table> (…)` —
    /// scenarios 5/42.
    CreateIndex {
        /// The table to index (project-schema-qualified on emit).
        table: String,
        /// The columns the index covers (at least one required).
        columns: Vec<String>,
        /// `true` ⇒ `CREATE INDEX CONCURRENTLY` (non-transactional online
        /// build); the emitted SQL uses `IF NOT EXISTS` so the executor's
        /// non-txn idempotency rule is satisfied.
        concurrently: bool,
    },
}

/// Quote a Postgres identifier (double embedded quotes, wrap in `"`). Routes
/// through the ONE crate-shared engine seam ([`crate::dml::quote_ident_checked`])
/// so author output is byte-identical to (and uniformly self-defending with) the
/// executor/role/journal quoting — fail-closed on an empty / NUL identifier
/// (which `"`-doubling cannot neutralise) rather than silently emitting it.
fn quote_ident(ident: &str) -> Result<String, AuthorError> {
    crate::render::dml::quote_ident_checked(ident).map_err(|e| {
        AuthorError::Invalid(format!("unquotable identifier ({}): {:?}", e.reason, e.value))
    })
}

/// #150 test seam: route a probe through the local `quote_ident` so the shared
/// helper's uniform render can be asserted across all engine seams.
#[cfg(test)]
pub(crate) fn quote_ident_for_test(ident: &str) -> Result<String, AuthorError> {
    quote_ident(ident)
}

/// Render `<schema>.<object>`, both parts quoted.
fn qualified(schema: &str, object: &str) -> Result<String, AuthorError> {
    Ok(format!("{}.{}", quote_ident(schema)?, quote_ident(object)?))
}

/// Render one column definition for a `CREATE TABLE` / `ADD COLUMN` clause.
fn column_def(c: &Column) -> Result<String, AuthorError> {
    let null = if c.nullable { "" } else { " NOT NULL" };
    // `ty` is emitted verbatim — a Postgres type, not an identifier.
    Ok(format!("{} {}{}", quote_ident(&c.name)?, c.ty, null))
}

/// The Postgres identifier length limit (`NAMEDATALEN - 1`), in bytes. An index
/// name longer than this is silently truncated *by the server* on `CREATE`, which
/// would break `IF NOT EXISTS` / `DROP INDEX` round-tripping (the name we emit in
/// the `down` would not match the truncated name on disk). So we truncate
/// deterministically *ourselves* and keep the result ≤ this many bytes.
pub(crate) const PG_MAX_IDENT_BYTES: usize = 63;

/// Cap an arbitrary generated identifier to ≤ [`PG_MAX_IDENT_BYTES`],
/// deterministically: when `natural` fits, return it verbatim; when it would
/// overflow, keep a readable prefix and append a short hash of the *full* name so
/// distinct long inputs still map to distinct, stable names.
///
/// This is the single source of truth for the cap discipline both the
/// deterministic author ([`index_name`]) and the declarative author
/// (`declarative::unique_index_name`) use, so an over-long generated index name
/// can never desync `up`/`down`/on-disk (which would cause CREATE/DROP churn —
/// the emitted full name ≠ the server-truncated live name on a re-diff). Mirrors
/// the sanitize/truncate discipline of [`crate::role::migrator_role_name`].
pub(crate) fn cap_ident_name(natural: &str) -> String {
    use sha2::{Digest, Sha256};
    if natural.len() <= PG_MAX_IDENT_BYTES {
        return natural.to_string();
    }
    // Overflow: deterministic 10-hex-char hash of the full natural name, plus a
    // truncated readable prefix. `<prefix>_<10 hex>` stays ≤ 63 bytes.
    let digest = Sha256::digest(natural.as_bytes());
    let suffix = hex::encode(&digest[..5]); // 10 hex chars
    // Reserve room for the `_<suffix>` (1 + 10 = 11 bytes). Truncate the readable
    // part on a char boundary so we never split a multi-byte UTF-8 sequence
    // (identifiers are ASCII in practice, but be safe).
    let budget = PG_MAX_IDENT_BYTES - (1 + suffix.len());
    let mut prefix = String::with_capacity(budget);
    for ch in natural.chars() {
        if prefix.len() + ch.len_utf8() > budget {
            break;
        }
        prefix.push(ch);
    }
    format!("{prefix}_{suffix}")
}

/// Derive an index name for a deterministic `CREATE INDEX` so `IF NOT EXISTS`
/// has a stable target (idempotent across re-authors of the same shape). The
/// natural `idx_<table>_<cols>` form is capped to ≤63 bytes by
/// [`cap_ident_name`].
fn index_name(table: &str, columns: &[String]) -> String {
    cap_ident_name(&format!("idx_{}_{}", table, columns.join("_")))
}

/// The deterministic, no-AI author for trivial additive ops (design §3).
///
/// Emits provably-safe, project-schema-qualified SQL with correct
/// [`MigrationFlags`]. Pattern-based only — it never attempts renames, drops, or
/// type changes (the AI author's remit).
#[derive(Debug, Clone)]
pub struct DeterministicAuthor {
    /// The project schema every emitted statement is qualified into.
    project_schema: String,
    /// The declaring app (`app_…`) recorded on the migration (per-table
    /// ownership, design §4).
    owner_app: String,
}

impl DeterministicAuthor {
    /// Construct a deterministic author bound to a project schema + owner app.
    #[must_use]
    pub fn new(project_schema: impl Into<String>, owner_app: impl Into<String>) -> Self {
        Self {
            project_schema: project_schema.into(),
            owner_app: owner_app.into(),
        }
    }

    /// Build a [`Migration`] from already-rendered `up`/`down` SQL + flags.
    fn make(&self, name: &str, up: String, down: Option<String>, flags: MigrationFlags) -> Migration {
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: &up,
            down: down.as_deref(),
            flags: &flags,
            owner_app: &self.owner_app,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Migration {
            version: MigrationId::generate(),
            name: name.to_string(),
            up,
            down,
            checksum,
            flags,
            owner_app: self.owner_app.clone(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        }
    }
}

impl MigrationAuthor for DeterministicAuthor {
    fn author(&self, req: &AuthorRequest) -> Result<Vec<Migration>, AuthorError> {
        let schema = &self.project_schema;
        let m = match req {
            AuthorRequest::CreateTable { name, columns } => {
                if name.is_empty() {
                    return Err(AuthorError::Invalid("table name is empty".into()));
                }
                if columns.is_empty() {
                    return Err(AuthorError::Invalid(format!(
                        "CreateTable '{name}' has no columns"
                    )));
                }
                let cols = columns
                    .iter()
                    .map(column_def)
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ");
                let up = format!("CREATE TABLE {} ({cols})", qualified(schema, name)?);
                // A clean additive create has a precise reverse.
                let down = Some(format!("DROP TABLE {}", qualified(schema, name)?));
                self.make(
                    &format!("create_table_{name}"),
                    up,
                    down,
                    // Additive: transactional, non-destructive, no approval.
                    MigrationFlags::default(),
                )
            }
            AuthorRequest::AddColumn { table, column } => {
                if table.is_empty() {
                    return Err(AuthorError::Invalid("table name is empty".into()));
                }
                if column.name.is_empty() {
                    return Err(AuthorError::Invalid("column name is empty".into()));
                }
                let up = format!(
                    "ALTER TABLE {} ADD COLUMN {}",
                    qualified(schema, table)?,
                    column_def(column)?,
                );
                let down = Some(format!(
                    "ALTER TABLE {} DROP COLUMN {}",
                    qualified(schema, table)?,
                    quote_ident(&column.name)?,
                ));
                self.make(
                    &format!("add_column_{table}_{}", column.name),
                    up,
                    down,
                    MigrationFlags::default(),
                )
            }
            AuthorRequest::CreateIndex {
                table,
                columns,
                concurrently,
            } => {
                if table.is_empty() {
                    return Err(AuthorError::Invalid("table name is empty".into()));
                }
                if columns.is_empty() {
                    return Err(AuthorError::Invalid(format!(
                        "CreateIndex on '{table}' has no columns"
                    )));
                }
                let idx = index_name(table, columns);
                let cols = columns
                    .iter()
                    .map(|c| quote_ident(c))
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ");
                // The non-txn idempotency rule (executor) REQUIRES `CREATE INDEX
                // CONCURRENTLY IF NOT EXISTS` so crash-recovery can re-run the
                // `up` verbatim. We always emit `IF NOT EXISTS` for both forms.
                let concurrent_kw = if *concurrently { "CONCURRENTLY " } else { "" };
                let up = format!(
                    "CREATE INDEX {concurrent_kw}IF NOT EXISTS {} ON {} ({cols})",
                    quote_ident(&idx)?,
                    qualified(schema, table)?,
                );
                // A concurrent DROP mirrors a concurrent CREATE; `IF EXISTS`
                // keeps the down idempotent too.
                let drop_concurrent = if *concurrently { "CONCURRENTLY " } else { "" };
                let down = Some(format!(
                    "DROP INDEX {drop_concurrent}IF EXISTS {}",
                    qualified(schema, &idx)?,
                ));
                let flags = MigrationFlags {
                    // CONCURRENTLY cannot run inside a transaction.
                    transactional: !*concurrently,
                    ..MigrationFlags::default()
                };
                self.make(&format!("create_index_{idx}"), up, down, flags)
            }
        };
        Ok(vec![m])
    }
}

/// The AI-author hook — wrap externally-generated `up`/`down` SQL as a
/// [`Migration`] (design §3, the "AI builder" author).
///
/// The AI/builder produces the SQL for non-trivial migrations (renames, type
/// changes, backfills, expand-contract sequences); the engine does **not** call
/// an LLM. This author takes that pre-generated SQL, runs the [`SqlGuard`] over
/// the `up` to derive the [`MigrationFlags`] (destructive ⇒ `requires_approval`;
/// any non-transactional statement ⇒ transactional=false) via
/// [`flags_for`](crate::guard::flags_for), and mints a versioned migration. The
/// output then gets the SAME guard + role treatment in the pipeline — untrusted
/// AI SQL is gated exactly like any other.
///
/// A *denial* (RCE / cross-tenant / file / network) is NOT raised here: the
/// engine's [`plan`](crate::engine::MigrationEngine::plan) records denials so a
/// caller sees every problem at once. Only an unparseable `up` (which `flags_for`
/// cannot classify) surfaces as an [`AuthorError`] at authoring time.
#[derive(Debug, Clone)]
pub struct RawSqlAuthor {
    /// The project schema the guard confines the supplied SQL to.
    project_schema: String,
    /// The declaring app (`app_…`) recorded on the migration.
    owner_app: String,
    /// Extensions the supplied SQL is permitted to `CREATE EXTENSION`.
    extension_allowlist: Vec<String>,
}

impl RawSqlAuthor {
    /// Construct a raw-SQL author bound to a project schema + owner app.
    #[must_use]
    pub fn new(project_schema: impl Into<String>, owner_app: impl Into<String>) -> Self {
        Self {
            project_schema: project_schema.into(),
            owner_app: owner_app.into(),
            extension_allowlist: Vec::new(),
        }
    }

    /// Permit the supplied SQL to `CREATE EXTENSION` the named extensions.
    #[must_use]
    pub fn with_extension_allowlist(mut self, exts: Vec<String>) -> Self {
        self.extension_allowlist = exts;
        self
    }

    /// Wrap pre-generated `up`/`down` SQL as a versioned [`Migration`].
    ///
    /// Runs the guard over `up` to derive destructive / non-transactional /
    /// approval flags. The migration's *gating* still happens in the engine; a
    /// guard **denial** is not raised here (it is surfaced by `plan`). Only an
    /// unparseable `up` errors at this stage.
    ///
    /// # Errors
    /// [`AuthorError::Guard`] if `up` is unparseable (the guard cannot classify
    /// it). A *denial* is deferred to [`plan`](crate::engine::MigrationEngine::plan).
    pub fn wrap(&self, name: &str, up: &str, down: Option<&str>) -> Result<Migration, AuthorError> {
        let guard = SqlGuard::new(
            GuardConfig::confined(self.project_schema.clone())
                .with_extension_allowlist(self.extension_allowlist.clone()),
        );
        // Derive flags from a guard pass when it succeeds; on a *denial* keep
        // conservative defaults but err only if UNPARSEABLE (a denial is the
        // engine plan's to surface, not authoring's). A denied-but-parseable
        // migration is still minted so plan can report the denial precisely.
        let flags = match guard.check(up) {
            Ok(report) => crate::guard::flags_for(&report),
            Err(GuardError::Parse(e)) => return Err(AuthorError::Guard(GuardError::Parse(e))),
            // Denied/cross-schema: parseable but dangerous. Mint with
            // conservative flags; plan() re-runs the guard and records the denial.
            Err(_) => MigrationFlags {
                // Conservative: treat as requiring approval until plan/gate decides.
                requires_approval: true,
                ..MigrationFlags::default()
            },
        };
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up,
            down,
            flags: &flags,
            owner_app: &self.owner_app,
            depends_on: &[],
            supersedes: &[],
            preconditions: &[],
        });
        Ok(Migration {
            version: MigrationId::generate(),
            name: name.to_string(),
            up: up.to_string(),
            down: down.map(str::to_string),
            checksum,
            flags,
            owner_app: self.owner_app.clone(),
            depends_on: Vec::new(),
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det() -> DeterministicAuthor {
        DeterministicAuthor::new("proj_acme", "app_acme")
    }

    fn col(name: &str, ty: &str, nullable: bool) -> Column {
        Column {
            name: name.into(),
            ty: ty.into(),
            nullable,
        }
    }

    #[test]
    fn create_table_emits_qualified_additive_sql() {
        let req = AuthorRequest::CreateTable {
            name: "orders".into(),
            columns: vec![col("id", "bigint", false), col("note", "text", true)],
        };
        let migs = det().author(&req).expect("author");
        assert_eq!(migs.len(), 1);
        let m = &migs[0];
        assert!(m.version.as_str().starts_with("mig_"));
        assert_eq!(m.owner_app, "app_acme");
        // Project-schema-qualified.
        assert!(
            m.up.contains("CREATE TABLE \"proj_acme\".\"orders\""),
            "up = {}",
            m.up
        );
        assert!(m.up.contains("\"id\" bigint NOT NULL"), "up = {}", m.up);
        assert!(m.up.contains("\"note\" text"), "up = {}", m.up);
        assert!(!m.up.contains("\"note\" text NOT NULL"), "nullable col must not be NOT NULL");
        // Additive ⇒ transactional, non-destructive, no approval.
        assert!(m.flags.transactional);
        assert!(!m.flags.destructive);
        assert!(!m.flags.requires_approval);
        // Reverse drops the table.
        assert_eq!(
            m.down.as_deref(),
            Some("DROP TABLE \"proj_acme\".\"orders\"")
        );
        // Checksum matches the emitted migration's whole apply-relevant unit.
        assert_eq!(m.checksum, Checksum::of(&crate::model::migration::ChecksumInput::from_migration(m)));
    }

    #[test]
    fn add_column_emits_alter_table_add_column() {
        let req = AuthorRequest::AddColumn {
            table: "users".into(),
            column: col("nickname", "text", true),
        };
        let m = &det().author(&req).expect("author")[0];
        assert_eq!(
            m.up,
            "ALTER TABLE \"proj_acme\".\"users\" ADD COLUMN \"nickname\" text"
        );
        assert!(m.flags.transactional);
        assert!(!m.flags.destructive);
        assert_eq!(
            m.down.as_deref(),
            Some("ALTER TABLE \"proj_acme\".\"users\" DROP COLUMN \"nickname\"")
        );
    }

    #[test]
    fn create_index_plain_is_transactional() {
        let req = AuthorRequest::CreateIndex {
            table: "users".into(),
            columns: vec!["email".into()],
            concurrently: false,
        };
        let m = &det().author(&req).expect("author")[0];
        assert!(m.up.starts_with("CREATE INDEX IF NOT EXISTS"), "up = {}", m.up);
        assert!(!m.up.contains("CONCURRENTLY"), "plain index must not be concurrent");
        assert!(m.up.contains("ON \"proj_acme\".\"users\" (\"email\")"), "up = {}", m.up);
        assert!(m.flags.transactional);
        assert!(!m.flags.destructive);
    }

    #[test]
    fn create_index_concurrently_is_non_txn_and_if_not_exists() {
        let req = AuthorRequest::CreateIndex {
            table: "users".into(),
            columns: vec!["email".into(), "tenant".into()],
            concurrently: true,
        };
        let m = &det().author(&req).expect("author")[0];
        // CONCURRENTLY ⇒ non-transactional + IF NOT EXISTS (executor's
        // idempotency rule for the two-phase non-txn recovery path).
        assert!(
            m.up.contains("CREATE INDEX CONCURRENTLY IF NOT EXISTS"),
            "up = {}",
            m.up
        );
        assert!(!m.flags.transactional, "CONCURRENTLY must be non-transactional");
        assert!(!m.flags.destructive);
        // Multi-column.
        assert!(m.up.contains("(\"email\", \"tenant\")"), "up = {}", m.up);
        // The down also drops concurrently + IF EXISTS.
        assert!(
            m.down.as_deref().unwrap().contains("DROP INDEX CONCURRENTLY IF EXISTS"),
            "down = {:?}",
            m.down
        );
    }

    #[test]
    fn index_name_stays_within_postgres_63_byte_limit_and_is_deterministic() {
        // A pathologically long table + column set whose natural
        // `idx_<table>_<cols>` would blow past Postgres's 63-byte identifier
        // limit (and so be silently truncated server-side, desyncing up vs down).
        let long_table = "a".repeat(50);
        let long_cols: Vec<String> = (0..5).map(|i| format!("{}{i}", "c".repeat(20))).collect();
        let n1 = index_name(&long_table, &long_cols);
        // Must fit Postgres's NAMEDATALEN-1 limit so the name we emit in `up`
        // matches the on-disk name and the `down`'s DROP INDEX.
        assert!(
            n1.len() <= PG_MAX_IDENT_BYTES,
            "index name {} bytes exceeds {PG_MAX_IDENT_BYTES}",
            n1.len()
        );
        // Deterministic: same inputs → same name (so re-authoring the same shape
        // is idempotent and the `down` can target it).
        assert_eq!(n1, index_name(&long_table, &long_cols));
        // Distinct long shapes → distinct names (the hash suffix disambiguates,
        // unlike a blind truncation that would collide on the shared prefix).
        let mut other_cols = long_cols.clone();
        other_cols.push("d".repeat(20));
        let n2 = index_name(&long_table, &other_cols);
        assert_ne!(n1, n2, "distinct column sets must yield distinct index names");
        assert!(n2.len() <= PG_MAX_IDENT_BYTES);
        // A valid Postgres identifier (starts with a letter, then [a-z0-9_]).
        assert!(n1.starts_with("idx_"), "name should keep a readable prefix: {n1}");
        assert!(
            n1.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "index name must be a bare identifier: {n1}"
        );
    }

    #[test]
    fn index_name_short_case_is_unchanged() {
        // The common (fitting) case is emitted verbatim — no surprise hashing.
        assert_eq!(
            index_name("users", &["email".to_string()]),
            "idx_users_email"
        );
    }

    #[test]
    fn create_index_with_long_names_emits_matching_up_and_down_name() {
        // End-to-end: the author's emitted up/down reference the SAME (capped)
        // index name, so DROP INDEX in the down can actually find it.
        let long_table = "t".repeat(40);
        let req = AuthorRequest::CreateIndex {
            table: long_table.clone(),
            columns: vec!["x".repeat(30), "y".repeat(30)],
            concurrently: false,
        };
        let m = &det().author(&req).expect("author")[0];
        let expected = index_name(&long_table, &["x".repeat(30), "y".repeat(30)]);
        assert!(expected.len() <= PG_MAX_IDENT_BYTES);
        assert!(m.up.contains(&format!("\"{expected}\"")), "up = {}", m.up);
        assert!(
            m.down.as_deref().unwrap().contains(&format!("\"{expected}\"")),
            "down = {:?}",
            m.down
        );
    }

    #[test]
    fn empty_columns_is_rejected() {
        let req = AuthorRequest::CreateTable {
            name: "t".into(),
            columns: vec![],
        };
        assert!(matches!(det().author(&req), Err(AuthorError::Invalid(_))));
    }

    #[test]
    fn raw_sql_author_wraps_safe_additive_sql_with_clean_flags() {
        let author = RawSqlAuthor::new("proj_acme", "app_acme");
        let m = author
            .wrap(
                "seed_settings",
                "INSERT INTO \"proj_acme\".\"settings\" (k, v) VALUES ('theme', 'dark')",
                None,
            )
            .expect("wrap");
        assert!(m.version.as_str().starts_with("mig_"));
        assert!(!m.flags.destructive);
        assert!(!m.flags.requires_approval);
        assert!(m.flags.transactional);
        assert_eq!(m.checksum, Checksum::of(&crate::model::migration::ChecksumInput::from_migration(&m)));
    }

    #[test]
    fn raw_sql_author_flags_destructive_drop_requiring_approval() {
        let author = RawSqlAuthor::new("proj_acme", "app_acme");
        let m = author
            .wrap(
                "drop_legacy",
                "DROP TABLE \"proj_acme\".\"legacy\"",
                None,
            )
            .expect("wrap");
        // A DROP is destructive ⇒ requires approval (flags_for §1.6).
        assert!(m.flags.destructive, "DROP TABLE must be flagged destructive");
        assert!(m.flags.requires_approval);
    }

    #[test]
    fn raw_sql_author_rejects_unparseable_up() {
        let author = RawSqlAuthor::new("proj_acme", "app_acme");
        let err = author
            .wrap("broken", "THIS IS NOT SQL ;;", None)
            .unwrap_err();
        assert!(matches!(err, AuthorError::Guard(GuardError::Parse(_))), "got {err:?}");
    }

    #[test]
    fn raw_sql_author_mints_dangerous_but_parseable_sql_for_plan_to_deny() {
        // A cross-tenant read is parseable but will be DENIED by plan(); the
        // author still mints it (conservative requires_approval) so plan can
        // report the denial precisely rather than failing at authoring.
        let author = RawSqlAuthor::new("proj_acme", "app_acme");
        let m = author
            .wrap("evil", "SELECT * FROM control.users", None)
            .expect("parseable, so wrap succeeds");
        assert!(m.flags.requires_approval, "dangerous SQL is conservatively gated");
    }
}
