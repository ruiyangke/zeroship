//! The two render-time ERROR types the backend contract carries across the crate
//! boundary.
//!
//! They live here rather than in the engine for one measured reason: they are the
//! `Err` halves of [`crate::renderer::DmlRenderer`]'s signatures, so a vendor crate
//! cannot implement the trait without naming them. Leaving them in the engine is
//! exactly the cycle `docs/proposals/pluggable-backends.md` step 4 hits - the engine
//! would name the vendors for its registry while the vendors name the engine for
//! their error type, and Cargo refuses.
//!
//! MEASURED, not assumed. With these two enums, [`crate::step::BindValue`] and the
//! `renderer` / `dml` / `vendor` modules moved out, the transitive core-module
//! closure of the three DML vendor modules collapses from **54 modules / 113,216
//! lines** to **7 modules / 7,733 lines**. `IrLowerError` alone was the bridge: it
//! sat in the 16,868-line `render::lower`, which reaches `engine`, `apply::*`,
//! `model::validate` and `render::fold` - effectively all of the engine.
//!
//! Neither enum needed anything from the engine to come with it. Every
//! `DeclarativeError` payload is a `String`, a `&'static str` or a `Vec<String>`;
//! `IrLowerError`'s only non-scalar payloads are the other three render errors
//! ([`DeclarativeError`], [`crate::vendor::VendorError`], [`crate::dml::DmlError`])
//! and `zero_migrate_ir::validate::AuthoringError`, which was already in the leaf
//! wire crate.
//!
//! Both are re-exported from their historical engine paths
//! (`zero_migrate::render::lower::IrLowerError`,
//! `zero_migrate::render::declarative::DeclarativeError`), so every existing caller
//! and every `match` arm resolves unchanged.

use zero_migrate_ir::dialect::DialectId;

/// A failure to author an online expand-contract sequence.
///
/// One variant, one `String`. It is here only because `DeclarativeError::Rename` is
/// `#[from] ExpandContractError` and `DeclarativeError` had to move; the authoring
/// machinery it names stays in `zero_migrate::render::expand_contract`, which
/// re-exports this type at its historical path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandContractError {
    /// A request field was empty or invalid (empty table/column name, `from`
    /// equal to `to`, empty type).
    #[error("invalid online intent: {0}")]
    Invalid(String),
}

/// A failure to diff a declarative desired schema against the live one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeclarativeError {
    /// A descriptor name/type was not a safe bare identifier / type at the
    /// author boundary (mirrors `crate::render::expand_contract`'s `validate_ident` /
    /// `validate_type`). Nothing is generated.
    #[error("invalid descriptor: {0}")]
    Invalid(String),
    /// The diff requires an op the differ does not generate: an in-place INDEX or
    /// FOREIGN KEY redefinition (any same-name index whose observable shape moved -
    /// uniqueness, columns, key elements, access method, predicate, INCLUDE, storage
    /// parameters, ONLY or comment - or a re-pointed FK target; each one a
    /// DROP+CREATE, which the differ does not synthesize because the DROP is
    /// destructive and wants an author's decision).
    /// Surfaced explicitly - never silently skipped. (Type/nullability changes are
    /// handled as gated/ungated ALTERs; destructive DROPs are gated
    /// migrations - neither uses this error.)
    #[error("the declarative differ does not generate this change: {0}; author it as an explicit migration instead")]
    UnsupportedInV1(String),
    /// An existing MySQL column needs a type or nullability change, which this
    /// differ renders as PostgreSQL `ALTER COLUMN` DDL that MySQL cannot execute.
    ///
    /// MySQL is not missing the capability - `MODIFY COLUMN` does this work - but
    /// it requires the COMPLETE column specification restated and silently drops
    /// every facet omitted. Emitting it from a diff that knows only the changed
    /// facet would discard the column's default, charset and comment, so the diff
    /// refuses and says which column it stopped on.
    #[error(
        "cannot change column {table}.{column} ({change}) on MySQL: the differ \
         renders alter-column DDL in PostgreSQL syntax, and MySQL's MODIFY COLUMN \
         needs the whole column definition restated (omitting any part of it \
         silently drops that part). Author the change as an explicit migration \
         with the SQL you want."
    )]
    MysqlAlterColumnUnsupported {
        /// The table holding the column the diff stopped on.
        table: String,
        /// The column whose change cannot be rendered.
        column: String,
        /// Which facet moved, so the message names the change and not just the site.
        change: &'static str,
    },
    /// A rename hint asked this differ to rename a MySQL column, which it authors as
    /// the PostgreSQL expand-contract sequence MySQL cannot drive.
    ///
    /// The rename author is dialect-blind: only SQLite `continue`s past it (its
    /// renames route through the 12-step rebuild), so PostgreSQL AND MySQL both
    /// pushed an `ExpandContractPlan` into the plan's `renames`. The engine then maps
    /// every one of them to `RenameStep::ExpandContract` unconditionally, and the
    /// MySQL backend exposes no `OnlineSchemaChange` capability to run it - so the
    /// deploy died mid-apply on an internal "routing bug" message, AFTER the plain
    /// DDL ahead of it had already committed, leaving a schema that was neither the
    /// old shape nor the new one and that no retry could complete.
    ///
    /// **The engine already declared this unsupported everywhere else**: the
    /// disposition table records `renameColumn | base | mysql: Unsupported`,
    /// `docs/dialects.md` prints `No`, and the IR lane's `lower_ir_rename` answers
    /// `Err(UnsupportedInV1)` at plan time. Only the differ dissented, and it was the
    /// one path that reached a live server.
    ///
    /// Note the DIRECTION, because the previous MySQL disagreement in this codebase
    /// ran the other way: for `setColumnType` the table said *portable* while the
    /// engine refused - the table was right and the engine under-delivered. This one
    /// is the mirror image. The table said *unsupported* and the engine
    /// OVER-delivered, planning an apply that could not work. Same class of
    /// table-vs-engine disagreement, opposite sign; check which way the next one
    /// points before assuming the table is the thing that needs correcting.
    #[error(
        "cannot rename column {table}.{from} to {to} on MySQL: the differ authors a \
         rename as the PostgreSQL expand-contract sequence (add shadow column, \
         dual-write trigger, backfill, deferred drop) and the MySQL backend has no \
         online schema-change capability to drive it. Author the rename as an \
         explicit migration with the SQL you want."
    )]
    MysqlRenameColumnUnsupported {
        /// The table holding the column the diff stopped on.
        table: String,
        /// The column the hint renames away from.
        from: String,
        /// The column the hint renames to.
        to: String,
    },
    /// A declared IDENTITY column's type moved off the three types PostgreSQL lets
    /// an identity column have.
    ///
    /// The differ's half of
    /// `IrLowerError::IdentityColumnTypeUnsupported`,
    /// refused for the same measured reason: the server answers `identity column
    /// type must be smallint, integer, or bigint` and rejects the `ALTER` outright,
    /// so a plan carrying it dies partway through applying. Widening and narrowing
    /// WITHIN the three stay legal and are not refused here.
    #[error(
        "cannot change column {table}.{column} to {to_type}: it is an IDENTITY column \
         and PostgreSQL confines one to smallInt, int or bigInt (`identity column type \
         must be smallint, integer, or bigint`). The ALTER is refused by the server, so \
         the migration would fail partway through applying. Declare it within those \
         three, or drop the identity property first."
    )]
    IdentityColumnTypeUnsupported {
        /// The table holding the identity column.
        table: String,
        /// The identity column whose declared type moved.
        column: String,
        /// The rendered spelling of the type the declaration asked for.
        to_type: String,
    },
    /// A declared field used a DSL type token the differ does not map. This
    /// covers both out-of-scope parameterised/extension types
    /// (`vector`/`geoPoint`/`encrypted`) AND typos / wrong spellings
    /// (`bigint`, `uuid`, `int4`, `serial`, …). It is rejected at the author
    /// boundary BEFORE any SQL is emitted, rather than silently degrading to a
    /// `text` column (the creator declared X, would have got `text`, with
    /// permanent divergence from what plugin-db materialises).
    #[error(
        "unsupported field type '{ty}' (not mapped by the declarative differ; \
         vector/geoPoint/encrypted are out of scope). Supported: string, number, boolean, date, calendarDate, \
         json, object, array, union, ref, bytes, actor, id, int, smallInt, bigInt, \
         float, real, inet"
    )]
    UnsupportedType {
        /// The unrecognised / out-of-scope DSL type token.
        ty: String,
    },
    /// Two or more apps declared the same table with DIFFERENT shapes. One table
    /// has exactly one owner; an identical re-declaration is
    /// idempotent (merged) but a conflicting one is a hard deploy error — never a
    /// silent last-writer-wins merge (this refines the blanket `DuplicateTable`).
    ///
    /// `apps` carries EVERY app that declared this table (sorted, deduplicated),
    /// not just the first-detected pair. This makes the report **deterministic
    /// regardless of descriptor order** even with 3+ declarers: the merge no
    /// longer reports `order_pair(slot_owner, latecomer)` on the first mismatch
    /// (whose `slot_owner` flapped with input order when two identical twins
    /// raced for the slot — 1b). The full sorted declarer set is the same for
    /// every permutation of the same descriptors.
    #[error(
        "conflicting declaration of table '{table}': apps {apps:?} declare it with \
         differing shapes (a table has exactly one owner; identical re-declaration \
         is idempotent, a conflicting one is a deploy error)"
    )]
    ConflictingDeclaration {
        /// The table declared with conflicting shapes.
        table: String,
        /// EVERY app that declared this table, sorted ascending and deduplicated.
        /// Order-independent: the same set for any permutation of the descriptors.
        apps: Vec<String>,
    },
    /// The deploying app tried to make a structural change (CREATE/ALTER/DROP) to
    /// a table it does NOT own (ownership enforcement). The
    /// declaring app owns a table's migrations; a non-owner may USE the table's
    /// rows freely but may NOT migrate its structure. (An IDENTICAL re-declaration
    /// by a non-owner produces no diff op and never trips this — only an actual
    /// structural change to a non-owned table is refused.)
    #[error(
        "app '{deploying_app}' may not migrate table '{table}' (owned by \
         '{owner}'): a non-owner may use a table but not alter its structure"
    )]
    NotTableOwner {
        /// The table the deploying app tried to change.
        table: String,
        /// The app that owns the table's migrations.
        owner: String,
        /// The app attempting the structural change.
        deploying_app: String,
    },
    /// The diff would emit a `DROP TABLE` for a live table absent from the union,
    /// but the differ **cannot confirm** the deploying app owns it: the caller's
    /// `live_ownership` map carries NO entry for that live table (2b). Rather than
    /// author a destructive drop of a table whose ownership it cannot verify, the
    /// differ **fails closed** — refusing the drop. This is the defence against a
    /// PARTIAL-union deploy (a caller that passed only one app's descriptors, so
    /// every OTHER app's live table looks "absent from desired"): the omitted
    /// tenants' tables are refused, never mass-dropped under the deploying app's
    /// authority. The fix is to supply the COMPLETE project union AND a
    /// `live_ownership` entry for every live table (see the `plan_declarative`
    /// caller contract).
    #[error(
        "refusing to drop live table '{table}': its ownership is unknown to this \
         diff (no live_ownership entry). The differ fails closed rather than author \
         a destructive drop it cannot confirm belongs to the deploying app — pass \
         the complete project union plus a live_ownership entry for every live table"
    )]
    DropOfUnownedTable {
        /// The live table whose ownership the caller did not supply.
        table: String,
    },
    /// A live table absent from `desired` whose stored `CREATE` is a
    /// `CREATE VIRTUAL TABLE`. The drop pass refuses it instead of authoring a
    /// `DROP TABLE`.
    ///
    /// A virtual table is not an ordinary table: it is the visible half of a
    /// module's storage. `fts5` and `vec0` both keep their real payload in
    /// auto-created SHADOW tables (`<v>_data`, `<v>_idx`, …), and dropping the
    /// vtable CASCADES those away — so a diff that "tidies up an undeclared table"
    /// silently destroys a search or vector index that may be expensive or
    /// impossible to rebuild.
    ///
    /// This engine never AUTHORS a virtual table, so one found live was created by
    /// something else — a data-plane runtime that manages its own indexes, or an
    /// engine version that still emitted them. Either way it is not the schema
    /// differ's to remove.
    ///
    /// Keyed on `CREATE VIRTUAL TABLE`, never on a module name or a table-name
    /// suffix, so a module nobody here has thought of is covered on the same terms.
    /// Fires AHEAD of the ownership check: the ownership guard only catches a
    /// caller that cannot confirm ownership, and a caller that maps every live
    /// table to the deploying app would otherwise sail straight through it.
    ///
    /// **This is forward infrastructure, not only a safety net.** Full-text search
    /// is intended to return as something COMPOSED from primitives rather than a
    /// builtin, and a composed feature that expands to `CREATE VIRTUAL TABLE` needs
    /// drift to tolerate virtual tables GENERALLY — which is why this is keyed on
    /// the DDL shape instead of the `fts5` special case it replaced. Refusing to
    /// drop them is the first half; teaching drift to RECOGNISE a composed vtable as
    /// a declared object is the second half, and does not exist yet. See
    /// `docs/proposals/fts-macro.md`.
    #[error(
        "refusing to drop live table '{table}': it is a VIRTUAL TABLE (module \
         '{module}'), not an ordinary table. Dropping it would cascade away the \
         module's backing shadow tables and destroy the index they hold. This \
         engine never authors virtual tables, so this one was created outside it — \
         drop it deliberately with whatever component created it, or declare it in \
         the project union, rather than letting a schema diff remove it"
    )]
    DropOfVirtualTable {
        /// The live virtual table the drop pass refused.
        table: String,
        /// The module from its `USING` clause (`fts5`, `vec0`, …).
        module: String,
    },
    /// A `ref` field declared a cross-app FK whose **target table is not in the
    /// union schema** (cross-app FK). A cross-app FK may reference a
    /// table owned by another app, but that table must exist in the project's
    /// union (declared by SOME member app); an FK to a table no app declares is a
    /// clear error surfaced here rather than failing as bad SQL at apply.
    #[error(
        "table '{table}' declares a foreign key to '{target}', which no app in the \
         project declares (a cross-app FK target must exist in the union schema)"
    )]
    CrossAppFkTargetMissing {
        /// The table declaring the dangling FK.
        table: String,
        /// The FK target table that is absent from the union.
        target: String,
    },
    /// A `RenameHint` did not match an actual drop+add pair: the `from`
    /// column is not present in live as a dropped column, OR the `to` column is
    /// not present in desired as an added column, on the named table. The hint is
    /// the creator's signed statement of intent, so an un-matchable hint is a hard
    /// error — never silently ignored (a silently-dropped hint would fall back to
    /// an unintended gated-drop + additive-add, losing the column's data).
    #[error(
        "rename hint {table}.{from} → {to} does not match a drop+add pair \
         (from must be a live-only column and to a desired-only column on {table})"
    )]
    RenameHintUnmatched {
        /// The table the hint named.
        table: String,
        /// The `from` column the hint named (expected: live-only).
        from: String,
        /// The `to` column the hint named (expected: desired-only).
        to: String,
    },
    /// A `RenameHint` matched a drop+add pair whose **types differ**: the
    /// live `from` column and the desired `to` column do not share a `data_type`.
    /// A pure online rename (expand-contract dual-write) requires type identity —
    /// a simultaneous rename + type change is two distinct intents and is refused
    /// rather than silently mirrored across incompatible types (which the
    /// dual-write `NEW.<to> := NEW.<from>` assignment could corrupt or reject).
    #[error(
        "rename hint {table}.{from} → {to} matched, but the types differ \
         ({from_type} → {to_type}); a rename requires type identity (rename + \
         type change is two separate intents)"
    )]
    RenameHintTypeMismatch {
        /// The table the hint named.
        table: String,
        /// The `from` column.
        from: String,
        /// The `to` column.
        to: String,
        /// The live `from` column's data type.
        from_type: String,
        /// The desired `to` column's data type.
        to_type: String,
    },
    /// a SQLite catalog-sourced
    /// renameColumn whose POST-rename descriptor `to` field declares a
    /// **data-transforming facet** (encryption / mask / `default` / `enum` / `check`
    /// range) the rebuild cannot certify was already present on the live `from`
    /// column. The rebuild renders the new table's CREATE from the descriptor `to`
    /// def while value-copying the live `from` bytes VERBATIM (no transform). A
    /// rename preserves facets by contract, so a descriptor that simultaneously
    /// CHANGES a facet on the renamed column would apply the new facet's shape to
    /// un-transformed old bytes (e.g. rebuild an `encrypted` column over plaintext,
    /// or stamp an `enum`/`check` the old values may violate). The live catalog read
    /// does NOT recover SDK-level facets for the `from` column, so the rebuild cannot
    /// prove preservation — it FAILS CLOSED rather than silently rebuild under a
    /// changed facet. (The pre-rename-descriptor path keeps the `from` facets and is
    /// unaffected; only the post-rename catalog path hits this.) Rename + facet change
    /// is two intents: do the rename, then a separate facet-change deploy.
    #[error(
        "renameColumn {table}.{from} → {to}: the post-rename descriptor declares a \
         data-transforming facet ({facet}) on the renamed column, but the live `{from}` \
         column's facets cannot be recovered from the catalog to certify it was already \
         present — refusing to rebuild under a changed facet over verbatim-copied bytes \
         (do the rename and the facet change as separate deploys)"
    )]
    RenameHintFacetMismatch {
        /// The table the rename named.
        table: String,
        /// The `from` column.
        from: String,
        /// The `to` column.
        to: String,
        /// Which data-transforming facet the descriptor `to` def declared
        /// (`encrypted` / `mask` / `default` / `enum` / `check`).
        facet: &'static str,
    },
    /// Two `RenameHint`s on the same table shared a `from` (e.g. `[a→c, a→d]`)
    /// or a `to` (e.g. `[a→c, b→c]`) column. Each hint resolves INDEPENDENTLY, so
    /// a shared endpoint produces two colliding expand-contract sequences: a
    /// duplicated `ADD COLUMN <to>` (the second fails `already exists`), divergent
    /// dual-write triggers, or a double `DROP COLUMN <from>`. The cross-hint
    /// validation pass rejects it before any SQL is authored. `side` is
    /// `"from"` or `"to"` — which endpoint was duplicated.
    #[error(
        "duplicate rename hint endpoint: column {table}.{column} appears as the \
         {side} of more than one hint; a column may be renamed at most once per \
         deploy"
    )]
    DuplicateRenameHint {
        /// The table the colliding hints named.
        table: String,
        /// The column that appeared more than once on the same side.
        column: String,
        /// Which endpoint collided: `"from"` or `"to"`.
        side: &'static str,
    },
    /// A `RenameHint`'s `to` equals another hint's `from` on the same table
    /// (e.g. `[a→b, b→c]`) — a rename CHAIN. Chains are not supported: the engine
    /// resolves each hint against the single live/desired snapshot pair, where the
    /// intermediate name (`b`) cannot be simultaneously a live-only drop and a
    /// desired-only add. Reject it EXPLICITLY rather than leave it to surface
    /// incidentally as an [`DeclarativeError::RenameHintUnmatched`].
    #[error(
        "rename hint chain on {table}: column {column} is both the target of one \
         hint and the source of another; chained renames are unsupported (resolve \
         them as separate deploys)"
    )]
    RenameHintChained {
        /// The table the chained hints named.
        table: String,
        /// The intermediate column that is both a `to` and a `from`.
        column: String,
    },
    /// A `RenameHint` had `from == to` — a no-op rename of a column to its own
    /// name. This is rejected with a PRECISE error rather than the misleading
    /// [`DeclarativeError::RenameHintUnmatched`] it would otherwise produce (the
    /// identical name is neither live-only nor desired-only).
    #[error(
        "no-op rename hint on {table}: from and to are the same column ({column}); \
         a rename must change the column name"
    )]
    RenameHintNoop {
        /// The table the hint named.
        table: String,
        /// The identical `from`/`to` column name.
        column: String,
    },
    /// Authoring the expand-contract rename sequence for a matched `RenameHint`
    /// failed (e.g. an identifier that passed the declarative author boundary was
    /// rejected by the stricter expand-contract author boundary). Surfaced rather
    /// than swallowed.
    #[error("failed to author rename expand-contract sequence: {0}")]
    Rename(#[from] ExpandContractError),
    /// A `ref` field's target was a schema-qualified `<otherApp>.<table>` — a
    /// CROSS-APP foreign key, forbidden fail-closed (the runtime plugin enforces
    /// the same rule). Every FK must stay inside the
    /// project schema; a reference to another member app's table is the BARE
    /// collection name (the union puts every app's tables in one schema), never a
    /// dot-qualified one. Caught at the author/plan boundary BEFORE any SQL is
    /// rendered, so a cross-schema escape can never reach DDL.
    #[error(
        "table '{table}' declares a cross-app foreign key to '{target}' (app \
         '{other_app}'): a foreign key may not cross an app/schema boundary — \
         reference a table in this project by its bare name"
    )]
    CrossAppFkForbidden {
        /// The table declaring the forbidden cross-app FK.
        table: String,
        /// The schema-qualified `<otherApp>.<table>` target.
        target: String,
        /// The `<otherApp>` schema prefix that crossed the boundary.
        other_app: String,
    },
    /// the Confined SQLite path needs an FK inlined at CREATE TABLE
    /// (SQLite has no `ALTER TABLE … ADD CONSTRAINT FOREIGN KEY`), but the FK's
    /// target table is neither already live nor created earlier in THIS batch — so
    /// it cannot be inlined and SQLite cannot add it later without a full table
    /// rebuild (the 12-step rebuild). Surfaced as a
    /// clear typed error rather than emitting an invalid `ALTER ADD CONSTRAINT`
    /// (which the SQLite authorizer/engine would reject anyway) or silently
    /// dropping the FK.
    #[error(
        "SQLite cannot defer the foreign key on table '{table}' → '{target}': SQLite \
         has no ALTER TABLE ADD CONSTRAINT, and the target is not live nor created \
         earlier in this batch (a table rebuild is required)"
    )]
    SqliteDeferredFkUnsupported {
        /// The table whose FK could not be inlined.
        table: String,
        /// The FK's target table (not yet available to inline against).
        target: String,
    },
    /// **Reserved fail-closed guard.** A Confined-SQLite existing-table change
    /// that the 12-step rebuild genuinely cannot express. The rebuild
    /// DOES handle the previously-deferred ops — a column TYPE change, a nullability
    /// change (either direction), a column RENAME, an ADD/DROP CONSTRAINT, and an
    /// in-place FK redefinition — so those now flow through
    /// `DeclarativePlan::rebuilds` instead of surfacing here. This variant remains
    /// as the fail-closed boundary for any future existing-table op the rebuild
    /// author cannot yet emit: the engine refuses to emit dangling Postgres DDL on
    /// the SQLite path, surfacing a clear typed error rather than a silent pass.
    #[error(
        "SQLite cannot perform the existing-table change on '{table}' natively \
         ({op}): it has no rebuild expression. The engine refuses to emit dangling \
         Postgres DDL on the SQLite path. Author a compensating migration."
    )]
    SqliteRebuildRequired {
        /// The existing table the rebuild-needing change targets.
        table: String,
        /// The specific operation that has no rebuild expression (human-readable).
        op: String,
    },
    /// The diff would CREATE a table that a `safety.require_rls` obligation covers.
    ///
    /// The obligation is a final-state one: every table a migration creates and
    /// leaves present must end RLS-enabled. The IR path can discharge it, because an
    /// author can write a `setRls` op next to the create. The declarative path
    /// cannot: the desired model is a
    /// `SchemaSnapshot`, which records RLS
    /// only as `RoleSnapshot.bypass_rls` and carries nothing per table, so no diff of
    /// it can author the `ENABLE ROW LEVEL SECURITY` the obligation asks for.
    ///
    /// So the create is refused rather than planned. Auto-appending the enable
    /// instead would invent a final state the author never declared, and planning it
    /// anyway would put an unprotected table in a schema the charter obligates - the
    /// gap this variant closes.
    ///
    /// Only the CREATEs the obligation actually covers are refused: an alter-only
    /// diff, a no-op diff, and a create into a schema the obligation does not name
    /// all keep planning.
    #[error(
        "refusing to create {tables:?} in schema '{schema}': a safety.require_rls \
         obligation covers these tables, and a declarative diff carries no RLS \
         transition that could satisfy it - the desired model records no per-table \
         RLS, so nothing here can render ENABLE ROW LEVEL SECURITY. Teaching the \
         declarative model, the introspector and the differ about RLS is a separate \
         feature and is not implied by this refusal. Either author these tables \
         through the IR migration path, where a setRls op can accompany the create, \
         or narrow the obligation's scope so it does not cover them"
    )]
    RequireRlsUnsatisfiable {
        /// The schema the refused tables would have been created in.
        schema: String,
        /// The covered tables this diff would create, sorted ascending.
        tables: Vec<String>,
    },
}
/// A failure lowering an IR op to SQL.
#[derive(Debug, thiserror::Error)]
pub enum IrLowerError {
    /// The op's fields could not be modelled as a snapshot (e.g. an unknown type
    /// token, an unsafe ref target). Carries the shared builder's error.
    #[error(transparent)]
    Snapshot(#[from] DeclarativeError),
    /// An op `IrAuthor::lower` does not yet compile (this Lower phase covers the
    /// DDL ops; DML / online-intent ops compile elsewhere). Carries the op tag.
    #[error("IrAuthor::lower does not yet compile op {0:?} (DDL ops only)")]
    UnsupportedOp(&'static str),
    /// Two `renameColumn` ops targeted one table in one migration on SQLite, which
    /// reconciles a rename by rebuilding the table from its verbatim stored
    /// `CREATE TABLE` text. The second rebuild would still carry the first one's
    /// pre-rename text, and the engine cannot rewrite that text without the lossy
    /// SQL rewrite the rebuild exists to avoid. Refused before anything runs, rather
    /// than failing mid-apply against an intermediate table. Carries the table.
    #[error(
        "table {0:?} is renamed twice in one migration, which SQLite cannot apply: \
         each rename rebuilds the table from its stored CREATE TABLE text, and the \
         second rebuild would still be built from the first one's. Split the renames \
         across separate migrations."
    )]
    SqliteRepeatRenameTarget(String),
    /// An alter-column op reached the renderer with MySQL as the target. The
    /// renderers behind these ops emit PostgreSQL `ALTER COLUMN` syntax on every
    /// dialect, which MySQL cannot execute. MySQL's own spelling, `MODIFY COLUMN`,
    /// requires the COMPLETE column specification restated and silently discards
    /// every facet left out, and the op carries only the facet being changed - so
    /// rendering it would drop the column's default, nullability, charset and
    /// comment rather than fail. Refuse instead. Carries the authored op name.
    #[error(
        "{0} is not supported on MySQL: the engine renders alter-column DDL in \
         PostgreSQL syntax, and MySQL's MODIFY COLUMN needs the whole column \
         definition restated (omitting any part of it silently drops that part). \
         Author the change as an explicit migration with the SQL you want."
    )]
    MysqlAlterColumnUnsupported(&'static str),
    /// A `setColumnType` on an IDENTITY column named a target PostgreSQL will not
    /// let an identity column have.
    ///
    /// MEASURED on PostgreSQL 18.4 through the engine's own emitted SQL: the
    /// server answers `identity column type must be smallint, integer, or bigint`
    /// and refuses the `ALTER` outright. The op cleared `validate` AND `preview`
    /// before this refusal existed, so the operator met the verdict mid-deploy,
    /// with the migration's earlier statements already applied.
    ///
    /// The permitted set is exactly the server's three, and exactly them: a DOMAIN
    /// over `integer` is refused by PostgreSQL too, measured, so it is refused
    /// here. Widening and narrowing WITHIN the set stay legal — `int → bigint` and
    /// `int → smallint` both apply with `attidentity` intact — because a refusal
    /// broader than the server's would deny a migration the database honours.
    #[error(
        "setColumnType on {table:?}.{column:?} names {to_type}, but that column is an \
         IDENTITY column and PostgreSQL confines one to smallInt, int or bigInt \
         (`identity column type must be smallint, integer, or bigint`). The ALTER is \
         refused by the server, so the migration would fail partway through applying. \
         Retype it within those three, or drop the identity property first."
    )]
    IdentityColumnTypeUnsupported {
        /// Target table.
        table: String,
        /// The identity column the op names.
        column: String,
        /// The rendered spelling of the type the op asked for.
        to_type: String,
    },
    /// An `alterSequence` that carries no option, so there is no action to render.
    ///
    /// `ALTER SEQUENCE <name>` with an empty action list is not a statement in
    /// PostgreSQL's grammar. MEASURED on PostgreSQL 18.4 against the engine's own
    /// emitted text: `ALTER SEQUENCE "s"` answers `syntax error at end of input`.
    ///
    /// Refused here rather than lowered to nothing. A no-op would still take a
    /// journal row and move the drift anchor while changing no sequence, so the
    /// history would record an alter that never happened - and the likeliest cause
    /// of an option-less alter is an author who meant to set something. This is the
    /// same posture the renderer already takes for the other empty payloads it
    /// meets (`exclusion constraint needs at least one element`, `malformed insert
    /// into "t": no rows`, `trigger events`): a payload that renders no SQL is
    /// malformed, not trivially satisfied.
    ///
    /// PRESENCE is the test, not the inner value. `restart: null` is a bare
    /// `RESTART`, `minValue: null` is `NO MINVALUE` and `ownedBy: null` is
    /// `OWNED BY NONE` - each is a present option carrying a null payload, and each
    /// is a real action.
    #[error(
        "alterSequence {name:?} names no action, so it renders `ALTER SEQUENCE` with \
         nothing after the name - which PostgreSQL rejects outright (`syntax error at \
         end of input`) and the whole migration dies partway through applying. Give \
         the alter at least one of increment, restart, minValue, maxValue, cache, \
         cycle or ownedBy, or drop the operation."
    )]
    AlterSequenceHasNoAction {
        /// The sequence the op names.
        name: String,
    },
    /// A `createTrigger` with `INSTEAD OF` timing whose target is a live TABLE.
    ///
    /// `INSTEAD OF` exists to make a VIEW writable, and both dialects that have it
    /// refuse it on a table in their own words. MEASURED through the engine's own
    /// emitted SQL:
    ///
    /// ```text
    ///   sqlite      cannot create INSTEAD OF trigger on table: t
    ///   postgres    "t" is a table  (DETAIL: Tables cannot have INSTEAD OF triggers.)
    /// ```
    ///
    /// So this is NOT a dialect capability and NOT a `dialect-support.toml` cell:
    /// the op IS supported on both, on a view. It is a structural fact about the op,
    /// which is why the refusal is dialect-neutral and lives here rather than in the
    /// capability tables. Before it existed, the op cleared validate, cleared lower
    /// and met the operator mid-apply, with the migration's earlier statements
    /// already committed.
    ///
    /// NOT SQLITE-ONLY. `createTrigger/bodyInsteadOf` is the only dialect-corpus row
    /// carrying `INSTEAD OF`, and it is unsupported on PostgreSQL because PostgreSQL
    /// has no trigger BODIES - so the live conformance sweep saw the defect on SQLite
    /// alone. But `create_trigger_variant` routes every `executeFunction` action to
    /// the `executeFunction` variant whatever the timing, so a PostgreSQL
    /// `INSTEAD OF ... EXECUTE FUNCTION` on a table selects a supported cell and dies
    /// the same way. Both are gated here.
    ///
    /// The test is the POSITIVE fact that the target is a KNOWN LIVE TABLE, never the
    /// absence of a view: catalog introspection fills that set from
    /// `relkind IN ('r','p')` on PostgreSQL and `type='table'` on SQLite, so a view is
    /// never in it, and a `createTable` earlier in the same envelope adds itself to
    /// it. A target the engine has never seen is left alone, the same fail-open
    /// posture [`Self::DmlValidate`]'s resolved-`ColRef` rule documents.
    #[error(
        "createTrigger {trigger:?} is INSTEAD OF, but {table:?} is a table. INSTEAD OF \
         triggers may only be used on views - SQLite answers `cannot create INSTEAD OF \
         trigger on table: {table}` and PostgreSQL answers `\"{table}\" is a table \
         (Tables cannot have INSTEAD OF triggers)`, so the migration would fail partway \
         through applying. Target a view, or use BEFORE/AFTER timing on the table."
    )]
    InsteadOfTriggerTargetIsATable {
        /// The trigger the op names.
        trigger: String,
        /// The live table the op targets.
        table: String,
    },
    /// A `createIndex` whose name already exists LIVE with a DIFFERENT shape.
    ///
    /// The emitters render `CREATE INDEX IF NOT EXISTS` whether or not the author
    /// asked, so the server SKIPS such a statement and reports success while
    /// keeping the live index. Measured: with `ix` live as a non-unique index on
    /// `(v)`, `CREATE UNIQUE INDEX IF NOT EXISTS "ix" … ("w")` succeeds with a
    /// NOTICE and leaves the old index — the author gets neither the uniqueness
    /// they asked for nor an error.
    ///
    /// Refused here rather than in the guard probe: the unguarded probe is
    /// `ownership_only` deliberately, so a same-table re-run stays the
    /// `IF NOT EXISTS` no-op crash recovery replays. Carries the rendered detail.
    #[error("{0}")]
    CreateIndexShapeConflict(String),
    /// A PG/MySQL create-time FK was correctly withheld from a forward
    /// reference, but no matching target-table CREATE appeared later in the
    /// selected artifact leg. Emitting the ALTER anyway would only fail later at
    /// apply time and could leave a partially applied schema, so lower refuses.
    #[error(
        "createTable {source_table:?} foreign key {constraint_name:?} references +         non-live target {target_table:?}, but that target was never created later +         in the selected artifact leg"
    )]
    DeferredForeignKeyTargetNotCreated {
        source_table: String,
        target_table: String,
        constraint_name: String,
    },
    /// A repeatable artifact contained a step that cannot honor run-on-change
    /// semantics. Repeatables are replace-style DDL migrations; silently routing a
    /// DML, backfill, or online-rename step through its once-only executor would
    /// make a changed artifact drift or skip instead of re-applying.
    #[error(
        "repeatable IR artifacts support replace-style DDL only; found {0}. Split the artifact or remove flags.repeatable"
    )]
    RepeatableStepUnsupported(&'static str),
    /// Authored plan metadata reached a step state machine that cannot execute
    /// that metadata faithfully. Refuse it at lower instead of including it in
    /// the checksum while silently ignoring it during apply.
    #[error("IR field {0} is not supported by this executable plan shape")]
    PlanMetadataUnsupported(&'static str),
    /// A column references a named enum/domain that has not been registered by an
    /// earlier `createEnum` / `createDomain` op in this IR stream.
    #[error("UNSUPPORTED {{ kind: {kind:?}, reason: \"unreachable use-site\", name: {name:?} }}")]
    NamedTypeMissing {
        /// `"enum"` or `"domain"`.
        kind: &'static str,
        /// Referenced type name.
        name: String,
    },
    /// A named enum/domain reference appears in a context this renderer cannot
    /// inline/materialize soundly.
    #[error("UNSUPPORTED {{ kind: {kind:?}, reason: {reason:?}, name: {name:?} }}")]
    NamedTypeUnsupported {
        /// `"enum"` or `"domain"`.
        kind: &'static str,
        /// Referenced type name.
        name: String,
        /// Why it cannot be rendered.
        reason: &'static str,
    },
    /// A SQLite operation that requires a table rebuild but lacks the complete
    /// live table snapshot (or is a non-FK constraint shape this IR path does not
    /// rebuild). Named FK add/drop changes do lower to the structured 12-step
    /// rebuild when full live structure is available.
    /// The message states the two reasons as alternatives because they ARE
    /// alternatives, and only one of them holds on any given refusal. The
    /// capability route reaches this without inspecting the snapshot at all, so a
    /// caller who supplied a complete one used to be told it was missing and went
    /// looking for introspection data it already had.
    #[error(
        "IrAuthor::lower of SQLite op {0:?} needs the 12-step table rebuild, which this \
         path cannot emit: either the op shape is one it does not rebuild, or the live \
         table snapshot is incomplete. Refusing rather than emitting a partial rebuild"
    )]
    SqliteRebuildOnly(&'static str),
    /// a guarded op whose shape cannot produce a verifiable
    /// `GuardProbe`. Lowering REFUSES fail-closed
    /// rather than stamping a probe that could not verify the declared shape.
    /// Carries the op tag.
    #[error(
        "IrAuthor::lower cannot build an existence-guard probe for op {0:?} \
         (the declared shape is not catalog-verifiable); refused fail-closed"
    )]
    GuardProbeUnbuildable(&'static str),
    /// a SQLite-targeted op whose EFFECTIVE schema is a
    /// NON-`main` schema (i.e. neither the bound project schema nor the implicit
    /// `main` target). The SQLite emitter renders UNqualified `main` DDL and carries
    /// no schema — so honoring a `schema:'reporting'` qualifier would require an
    /// explicit `ATTACH 'reporting.db' AS reporting`, which the engine does NOT
    /// auto-perform. Rather than SILENTLY dropping the qualifier (rendering the op
    /// into `main` — a silent-WRONG-target), lowering FAILS CLOSED here: a non-main
    /// schema qualifier on the SQLite leg requires an explicit ATTACH the author must
    /// arrange, never an implicit re-pin to `main`. Carries the offending schema.
    /// (Confined/Platform on SQLite are unaffected: `eff == project == main`.)
    #[error(
        "IrAuthor::lower targets SQLite with a non-main schema qualifier {0:?} — the \
         SQLite leg renders unqualified `main` DDL and performs NO auto-ATTACH; a \
         non-main schema requires an explicit `ATTACH … AS {0}` the author must \
         arrange. Refusing to silently render into `main` (a wrong-target drop)."
    )]
    SqliteSchemaUnsupported(String),
    /// the connection `default_schema`
    /// resolved an op's EFFECTIVE schema to a schema the author's
    /// confinement `scope` does NOT permit. The friendly op-level
    /// cross-schema VALIDATE gate inspects ONLY the op's own qualifier, never this
    /// connection default; so a foreign `default_schema` would otherwise render every
    /// guard-less op (one that omits its own qualifier) into the foreign schema while
    /// the validate gate stays silent. Lowering FAILS CLOSED here: a `default_schema`
    /// outside the active scope is refused, not rendered. A bare `IrAuthor::lower`
    /// confines against the Confined `Single(project_schema)`, so a creator-path author
    /// refuses a foreign default even without the upstream load gate;
    /// `IrAuthor::lower_guarded` confines against the charter's `schema.cross_schema`
    /// grant. Carries the offending schema.
    #[error(
        "IrAuthor::lower resolved a connection default_schema to {0:?}, which the \
         author's schema-confinement scope does not permit — the op-level cross-schema \
         gate never inspects the connection default, so a foreign default is refused \
         fail-closed here rather than rendered into {0:?}. Bind a default within scope, \
         or grant schema.cross_schema on {0:?} and route through the guarded lower."
    )]
    DefaultSchemaOutOfScope(String),
    /// an op carrying an
    /// EXPLICIT `schema()` qualifier that the active confinement
    /// scope does NOT permit. The friendly op-level cross-schema
    /// VALIDATE gate (`zero_migrate_ir::validate::validate_ir_scoped`) already refuses this
    /// fail-closed on every PRODUCTION path (`load_and_lower[_guarded]` →
    /// `load_ir_document` → `validate_ir_scoped` gates the explicit qualifier before
    /// lower). But the public `lower`/`lower_steps`
    /// entries do NOT re-run validation — they assume the IR was pre-validated by the
    /// load gate. A future INTERNAL caller invoking bare `lower()` with an op carrying
    /// an explicit FOREIGN `schema()` would otherwise render into that foreign schema,
    /// since the only lower-time scope check covered the `default_schema` case. This
    /// arm makes `lower()` self-defending regardless of whether validate ran: an
    /// explicit out-of-scope qualifier is refused fail-closed at lower, matching the
    /// SQLite/`default_schema` checks beside it. Carries the offending schema. (Not
    /// creator-reachable — the load gate already refuses it; this is the latent-footgun
    /// backstop for internal callers.)
    #[error(
        "IrAuthor::lower of an op explicitly qualified with schema {0:?}, which the \
         author's schema-confinement scope does not permit — the public lower entries \
         do not re-run the cross-schema VALIDATE gate, so an out-of-scope explicit \
         qualifier is refused fail-closed here rather than rendered into {0:?}. Route \
         through the load gate (which validates), or grant schema.cross_schema on \
         {0:?} in the charter the guarded lower composes."
    )]
    LowerCrossSchema(String),
    /// a SQLite `renameColumn` whose table's full live structure is not
    /// in `LiveSchema::table_snapshots` / `LiveSchema::sqlite_schemas`. SQLite
    /// has no native online rename, so the rename is reconciled by the 12-step
    /// table REBUILD, which needs the WHOLE live table shape (every column + the
    /// live SDK schema `Value`) to author the post-rename CREATE + value-copy. The
    /// PG leg never needs this (it lowers to expand-contract from `{from,to,ty}`).
    /// Carries the table name. Fail-closed: never emit a rebuild from a partial
    /// view of the table.
    #[error(
        "IrAuthor::lower of a SQLite renameColumn on table {0:?} needs the table's \
         full live structure (LiveSchema::table_snapshots + sqlite_schemas) to \
         author the 12-step rebuild; it is absent — refusing to emit a rebuild from \
         a partial view"
    )]
    SqliteRenameNeedsLiveTable(String),
    /// the cross-subsystem `OnlineIntent` bridge or the SQLite rebuild
    /// planner rejected a `renameColumn` lowering (an empty/identical name, an
    /// un-resolvable rename hint, an emitter shape mismatch). Carries the
    /// underlying error text. Distinct from [`Self::Snapshot`] because it crosses
    /// into the expand-contract author / the differ, not the shared snapshot
    /// builder.
    #[error("IrAuthor::lower of renameColumn failed: {0}")]
    RenameLower(String),
    /// **VENDOR** — a vendor (`zero-migrate`) op was lowered against a
    /// SQLite target. Every vendor primitive (roles/grants/RLS/policies/triggers/
    /// functions/extensions/schemas/`raw`) is `dialect_scope = PgOnly` and has no
    /// SQLite analogue — refused fail-closed at lower (the
    /// validate gate already refuses it at load on a SQLite target). Carries the op
    /// kind tag.
    #[error(
        "IrAuthor::lower of vendor op {0:?} is Postgres-only — the zero-migrate \
         vendor primitives have no SQLite analogue (PgOnly); a SQLite deploy of them is \
         refused fail-closed"
    )]
    VendorPgOnly(&'static str),
    /// A vendor op reached lower without the
    /// capability validated by the load gate. Lower refuses it before rendering so
    /// direct `lower`/`lower_guarded` callers cannot rely on the SQL guard's
    /// deny-list coverage for benign-looking vendor SQL.
    #[error(
        "IrAuthor::lower of vendor op {op:?} requires capability {capability:?}, \
         but the active vendor capability set does not grant it; refusing before \
         rendering"
    )]
    VendorCapabilityDenied {
        /// The op kind tag.
        op: &'static str,
        /// The capability the op requires.
        capability: zero_migrate_ir::capability::VendorCapability,
    },
    /// **VENDOR** — rendering a vendor op to its Postgres DDL failed (an invalid
    /// identifier, an unrenderable policy/trigger predicate, an empty privilege/role
    /// list). Carries the underlying [`crate::vendor::VendorError`].
    #[error(transparent)]
    Vendor(#[from] crate::vendor::VendorError),
    /// A trigger action or facet is unsupported on the target dialect. Triggers are
    /// cross-dialect core, so these are per-facet/action refusals rather than the
    /// old whole-construct vendor gate.
    #[error("IrAuthor::lower of trigger facet/action {kind:?} is unsupported on {dialect}")]
    TriggerUnsupported {
        /// Stable unsupported-kind token (`triggerBody`, `executeFunction`, …).
        kind: &'static str,
        /// The target dialect that cannot render the facet/action.
        dialect: DialectId,
    },
    /// A view facet is unsupported on the target dialect. Plain structured views
    /// are cross-dialect core; materialized views are PostgreSQL-only.
    #[error("IrAuthor::lower of view facet {kind:?} is unsupported on {dialect}")]
    ViewUnsupported {
        /// Stable unsupported-kind token (`materializedView`, …).
        kind: &'static str,
        /// The target dialect that cannot render the facet.
        dialect: DialectId,
    },
    /// Standalone sequences are PostgreSQL-only; SQLite/MySQL auto-increment is
    /// not a general sequence object and is never used as an emulation.
    #[error("UNSUPPORTED {{ kind: {kind:?}, dialect: {dialect} }}")]
    SequenceUnsupported {
        /// Stable unsupported-kind token.
        kind: &'static str,
        /// The target dialect.
        dialect: DialectId,
    },
    /// Exclusion constraints are PostgreSQL-only.
    #[error("UNSUPPORTED {{ kind: {kind:?}, dialect: {dialect} }}")]
    ExclusionConstraintUnsupported {
        /// Stable unsupported-kind token.
        kind: &'static str,
        /// The target dialect.
        dialect: DialectId,
    },
    /// A column facet is unsupported on the target dialect. Generated/identity
    /// columns are cross-dialect core with per-facet refusals (for example,
    /// SQLite non-PK identity or Postgres virtual generated columns).
    #[error("IrAuthor::lower of column facet {kind:?} is unsupported on {dialect}: {reason:?}")]
    ColumnUnsupported {
        /// Stable unsupported-kind token (`virtualColumn`, `identity`, …).
        kind: &'static str,
        /// The target dialect that cannot render the facet.
        dialect: DialectId,
        /// Optional precise reason.
        reason: Option<&'static str>,
    },
    /// a `renameColumn` whose IR-carried `ColType` does not match the
    /// LIVE `from` column's actual `data_type`. A pure online rename mirrors values
    /// across the two columns (PG dual-write `NEW.<to> := NEW.<from>`; the SQLite
    /// rebuild copies the column across) and CANNOT also change the type — a
    /// simultaneous rename + retype is two distinct intents. The IR path is the
    /// higher-risk AI/creator-authored surface, so it must NOT silently trust an
    /// IR-carried type that disagrees with the live column: a wrong `ty` (e.g.
    /// `Int` over a live `text` column) would otherwise author a mismatched
    /// `ADD COLUMN` + a cross-type dual-write copy with no rejection. This is the
    /// IR-path mirror of the declarative differ's
    /// `crate::render::declarative::DeclarativeError::RenameHintTypeMismatch` — enforced
    /// IDENTICALLY on BOTH dialects (the single authoritative type source is the
    /// LIVE column, reconciled against the IR `ty`; neither leg silently uses one
    /// over the other). Carries the table, the column, and the two `data_type`s.
    #[error(
        "IrAuthor::lower of renameColumn {table:?}.{from:?} → {to:?}: the IR-carried \
         type ({ir_type}) does not match the live `{from}` column's type \
         ({live_type}); a rename requires type identity (rename + type change is two \
         separate intents) — refusing to author a cross-type dual-write/rebuild"
    )]
    RenameTypeMismatch {
        /// The table the rename targets.
        table: String,
        /// The `from` column (the live column whose type is authoritative).
        from: String,
        /// The `to` column.
        to: String,
        /// The `information_schema` `data_type` the IR-carried `ColType` derives.
        ir_type: String,
        /// The live `from` column's actual `information_schema` `data_type`.
        live_type: String,
    },
    /// a `renameColumn` whose LIVE `from` column structure is absent from
    /// `LiveSchema::table_snapshots`, so the authoritative IR-vs-live type
    /// reconciliation (see [`Self::RenameTypeMismatch`]) cannot run. A rename must
    /// NEVER lower from an IR-carried type alone — the live column type is the
    /// authority on BOTH dialects — so an absent live `from` column fails closed
    /// rather than trusting the IR `ty`. Carries the table + column.
    #[error(
        "IrAuthor::lower of renameColumn on {0:?}.{1:?} needs the live `{1}` column's \
         type (LiveSchema::table_snapshots) to reconcile the IR-carried type against \
         the live column; it is absent — refusing to lower a rename from an IR type \
         alone"
    )]
    RenameNeedsLiveColumn(String, String),
    /// the structural expression validator (`crate::model::validate`)
    /// rejected an embedded closed-AST node of a DML op (`update`/`del`/`backfill`
    /// `set`/`where`/`filter`) BEFORE assembly: an out-of-policy node, an
    /// out-of-envelope synth, a non-portable cast. Boxed (the `AuthoringError`
    /// payload is large). The structured payload reaches the author through
    /// the boxed error's `Display`.
    #[error("IrAuthor::lower of a DML op: {0}")]
    DmlValidate(Box<zero_migrate_ir::validate::AuthoringError>),
    /// a selected backend's key-storage policy refused a live catalog column.
    /// [`zero_migrate_ir::ir::IndexElement::Column`] carries no backend-specific
    /// prefix or storage modifier, so the authoring error supplies that backend's
    /// exact reason and remedy.
    ///
    /// The OFFLINE backend storage gate already refuses this when
    /// the column is declared in the SAME migration as the key. This is the half
    /// it cannot see: a column an EARLIER ordered migration created, or one of an
    /// unmanaged table. Validation reads only the migration in front of it, so
    /// the live catalog the apply path has already introspected is the only
    /// witness — which is why the refusal lives here and not there.
    ///
    /// Boxed because the `AuthoringError` payload is large.
    #[error("{0}")]
    KeyStorage(Box<zero_migrate_ir::validate::AuthoringError>),
    /// the creator-DML assembler (`crate::render::dml`) rejected a DML op: a
    /// malformed identifier, an empty/ragged insert, or a MySQL `onConflict`
    /// shape whose authored target cannot be retained safely.
    /// All are hard errors. A DML op is never silently dropped or misapplied.
    #[error("IrAuthor::lower of a DML op: {0}")]
    DmlAssemble(#[from] crate::dml::DmlError),
    /// A resumable backfill reached live-schema planning without an exact,
    /// non-null primary/unique cursor tuple whose scalar and comparison
    /// semantics the selected executor can preserve.
    #[error(
        "planner refused resumable backfill on {schema}.{table} with cursorColumns {columns:?}: {reason}. \
         Use an explicit path: a one-shot update under a maintenance window, a \
         target-specific rebuild or temporary surrogate, or creation of a stable \
         unique cursor in an earlier migration. zero-migrate never pages on an \
         unstable row locator."
    )]
    BackfillCursorUnavailable {
        /// Effective target schema.
        schema: String,
        /// Target table.
        table: String,
        /// Authored ordered cursor tuple.
        columns: Vec<String>,
        /// The failed live proof.
        reason: String,
    },
}
