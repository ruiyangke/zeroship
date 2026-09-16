//! The `ExpandContractAuthor` - zero-downtime **online column RENAME** via
//! trigger dual-write (the zero-downtime expand-contract pattern).
//!
//! A column rename cannot be done as a single `ALTER TABLE ... RENAME COLUMN`
//! without breaking every running deploy that still reads/writes the old name:
//! the rename is atomic, but the *fleet* is not - old code and new code run
//! concurrently across a rolling deploy. The expand-contract pattern makes the
//! two shapes **coexist** so neither generation of code ever sees a missing
//! column:
//!
//! ```text
//! EXPAND  (deploy N, lands BEFORE code switches to the new name)
//!   E1  ADD COLUMN <to> <ty>            -- nullable, transactional, additive
//!   E2  CREATE FUNCTION + TRIGGER        -- BEFORE INSERT/UPDATE dual-write
//!       (mirror <from> and <to> both ways)  depends_on [E1]
//!   E3  BACKFILL <to> := <from>          -- cursor on the PRIMARY KEY
//!       WHERE <to> IS NULL                 depends_on [E2]
//!
//! CONTRACT (deploy N+1, lands AFTER code stops using <from>; gated on EXPAND)
//!   C1  DROP TRIGGER + DROP FUNCTION     -- requires_approval, depends_on [E2]
//!   C2  DROP COLUMN <from>               -- destructive, requires_approval,
//!                                           depends_on [E1, E3, C1]
//! ```
//!
//! # Why each piece is shaped the way it is
//!
//! - **E1 is nullable + transactional.** A bare `ADD COLUMN ... NOT NULL` over a
//!   populated table rewrites the whole table under `ACCESS EXCLUSIVE`; the
//!   online author MUST NOT emit it (and MUST NOT emit a bare `SET NOT NULL`
//!   either - see [`ExpandContractAuthor`] for the `CHECK ... NOT VALID` ->
//!   `VALIDATE` lint). This stops at the nullable column + dual-write + backfill;
//!   tightening to `NOT NULL` is a separate authored step.
//! - **E2 is `SECURITY INVOKER` (the plpgsql default), NOT `SECURITY DEFINER`.**
//!   A `DEFINER` trigger would run with the *function owner's* (the migrator's)
//!   privileges for every app write - an escalation primitive, and guard-denied
//!   anyway. `INVOKER` runs the trigger body with the **writing app role's**
//!   privileges; the dual-write is just an in-row `NEW.* :=` assignment in a
//!   `BEFORE` trigger, which needs no privilege beyond writing the row the app
//!   is already writing.
//! - **E2's recursion / write-amplification guard.** A `BEFORE` trigger that
//!   assigns `NEW.*` does **not** re-fire (it mutates the row in flight, it does
//!   not issue a new statement), so there is no infinite recursion by
//!   construction. The `IS DISTINCT FROM` guards exist to avoid *write
//!   amplification*: the trigger only assigns the mirror column when the source
//!   actually changed and the mirror did not, so an UPDATE that touches neither
//!   column (or that already wrote both consistently) is a no-op. A `WHEN`
//!   clause on the trigger short-circuits it entirely when neither column is
//!   distinct across the update.
//! - **E3 backfills on the table's PRIMARY KEY**, never on `<to>` (the column
//!   being populated): the backfill engine requires a UNIQUE/NOT-NULL cursor and
//!   forbids paging on the column it mutates (see
//!   [`OnlineSchemaChange::run_online_backfill`](crate::apply::backend::OnlineSchemaChange::run_online_backfill)). E3
//!   depends on E2 so the trigger is live before the backfill runs - otherwise a
//!   concurrent write between backfill batches could land in `<from>` only and
//!   be lost.
//! - **C1/C2 are gated.** Dropping the trigger and the old column is
//!   `requires_approval` (C2 is also `destructive`). The engine's expand/contract
//!   gate additionally refuses the contract until the matching
//!   expand is net-applied in the journal.
//!
//! All emitted SQL is **project-schema-qualified** and **byte-stable** across
//! re-authoring (the function/trigger/index names are deterministic functions of
//! the table + column names), so re-authoring the same intent yields identical
//! `Expand` checksums - exactly like [`crate::plan::author`]'s index-name determinism.

use crate::model::backfill::BackfillSpec;
use crate::model::migration::{Checksum, Migration, MigrationFlags, MigrationId, OnlinePhase};
use zeroship_migrate_backend::registry::VendorSet;
use zeroship_migrate_ir::dialect::DialectId;

/// The neutral online-migration INTENT this author expands into a phased
/// [`Migration`] sequence.
///
/// MOVED to `zeroship-migrate-backend` and re-exported here. It is what
/// `OnlineSchemaChange::run_online_backfill` is handed - the rename's MEANING rather than
/// this module's PostgreSQL spelling of it - so a vendor crate cannot implement
/// the capability without naming it. It carries four `String`s, so it travelled
/// alone; the [`ExpandContractPlan`] that holds it stayed, because it also holds
/// the authored `Migration` sequence and the `BackfillSpec`.
pub use zeroship_migrate_backend::capability::OnlineIntent;

/// A failure to author an online expand-contract sequence.
///
/// MOVED to `zeroship-migrate-backend` and re-exported here. It is a one-variant
/// `Invalid(String)` and it travelled for one reason: `DeclarativeError::Rename` is
/// `#[from] ExpandContractError`, and `DeclarativeError` had to go with
/// `IrLowerError`. It brought nothing with it.
pub use zeroship_migrate_backend::error::ExpandContractError;

/// The full ordered output of [`ExpandContractAuthor::author`].
///
/// MOVED to `zeroship-migrate-backend` and re-exported here. It is what
/// `PlanStep::OnlineRename` carries through `RenameStep::ExpandContract`, so it had
/// to travel with the lowered-plan vocabulary. Nothing came with it: the authored
/// `Migration`s and the `MigrationId`s are `zeroship-migrate-ir`'s, the `BackfillSpec`
/// and the [`OnlineIntent`] were already in the contract crate. The AUTHOR - every
/// line of PostgreSQL trigger and function DDL below - stayed here.
pub use zeroship_migrate_backend::capability::ExpandContractPlan;

/// Quote an identifier through the explicitly selected registered backend.
pub(crate) fn quote_ident(vendors: VendorSet, ident: &str, dialect: &DialectId) -> String {
    crate::render::dml::escape_quote_ident_for_dialect(vendors, ident, dialect)
}

/// Validate a bare SQL identifier: non-empty, starts with a letter/underscore,
/// and contains only `[A-Za-z0-9_]`. Mirrors the `validate_ident` in the
/// module-private `crate::zeroship_migrate_postgres::backend::backfill_sql` (named in plain
/// text because a private module is not a linkable doc target)
/// so `table`/`from`/`to` are safe-by-construction at the AUTHOR boundary - not
/// only safe-by-quoting downstream. Rejects schema-qualified names
/// (`control.users`), quote-injection (`t"; DROP ...`), whitespace, punctuation.
///
/// # Errors
/// [`ExpandContractError::Invalid`] when `value` is not a bare identifier.
fn validate_ident(what: &str, value: &str) -> Result<(), ExpandContractError> {
    let mut chars = value.chars();
    let ok_first = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
    let ok_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if value.is_empty() || !ok_first || !ok_rest {
        return Err(ExpandContractError::Invalid(format!(
            "{what} is not a valid bare identifier: '{value}'"
        )));
    }
    Ok(())
}

/// Validate a Postgres type name spliced verbatim into `ADD COLUMN <to> <ty>`.
/// The author defends in depth (the downstream guard is the second line): a real
/// Postgres type never contains a statement separator `;` and always has balanced
/// parentheses, so we reject a `ty` that has either - closing
/// `text; CREATE TABLE control.evil(...)` and truncated `numeric(10` at the
/// author boundary while still accepting `numeric(10,2)`, `varchar(255)`, etc.
///
/// # Errors
/// [`ExpandContractError::Invalid`] when `ty` contains `;` or unbalanced parens.
fn validate_type(ty: &str) -> Result<(), ExpandContractError> {
    if ty.contains(';') {
        return Err(ExpandContractError::Invalid(format!(
            "column type contains a statement separator ';': '{ty}'"
        )));
    }
    let mut depth: i32 = 0;
    for c in ty.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return Err(ExpandContractError::Invalid(format!(
                        "column type has unbalanced parentheses: '{ty}'"
                    )));
                }
            }
            _ => {}
        }
    }
    if depth != 0 {
        return Err(ExpandContractError::Invalid(format!(
            "column type has unbalanced parentheses: '{ty}'"
        )));
    }
    Ok(())
}

/// Render `<schema>.<object>`, both parts quoted.
pub(crate) fn qualified(
    vendors: VendorSet,
    schema: &str,
    object: &str,
    dialect: &DialectId,
) -> String {
    format!(
        "{}.{}",
        quote_ident(vendors, schema, dialect),
        quote_ident(vendors, object, dialect)
    )
}

/// Sub-step indices for the online-rename sequence - folded into the
/// rename's stable seed so each of E1..C2 derives a DISTINCT, reproducible id.
/// These are the `step_index` half of `step_id = derive(rename_seed, step_index)`.
const EC_STEP_E1: u8 = 1;
const EC_STEP_E2: u8 = 2;
const EC_STEP_E3: u8 = 3;
const EC_STEP_C1: u8 = 4;
const EC_STEP_C2: u8 = 5;
const EC_STEP_ABORT_C1: u8 = 6;
const EC_STEP_ABORT_C2: u8 = 7;

/// Derive the journal identity of one resolver-authored abort step.
///
/// The pending version is the durable identity of an online rename, while the
/// ordinal distinguishes the ordered cleanup statements. Keeping this in one
/// helper lets execution and status recognize the same resolver-owned entries.
pub(crate) fn resolve_pending_abort_version(pending_version: &str, ordinal: usize) -> MigrationId {
    let mut seed = pending_version.as_bytes().to_vec();
    seed.extend_from_slice(&(ordinal as u64).to_be_bytes());
    MigrationId::derive("resolve_pending_abort", &seed)
}

/// Derive the journal identity for the atomic roll-forward resolver step.
pub(crate) fn resolve_pending_apply_atomic_version(pending_version: &str) -> MigrationId {
    MigrationId::derive("resolve_pending_apply_atomic", pending_version.as_bytes())
}

/// Derive the journal identity for the atomic abort resolver step.
pub(crate) fn resolve_pending_abort_atomic_version(pending_version: &str) -> MigrationId {
    MigrationId::derive("resolve_pending_abort_atomic", pending_version.as_bytes())
}

/// Build the rename's STABLE identity seed: a length-prefixed image of
/// every fact that identifies the logical rename - `schema`, `owner`, `table`,
/// `from`, `to`, `ty`. Length-prefixing each field makes the encoding injective
/// (so `("a","bc")` and `("ab","c")` never collide). NOTHING per-run is folded
/// (no time, no random), so re-lowering the identical IR envelope reproduces the
/// SAME seed -> the SAME E1..C2 ids. A semantically different rename (different
/// `to`/`ty`) produces a different seed -> fresh ids.
fn rename_id_seed(
    schema: &str,
    owner: &str,
    table: &str,
    from: &str,
    to: &str,
    ty: &str,
) -> Vec<u8> {
    let mut seed = Vec::new();
    for field in [schema, owner, table, from, to, ty] {
        seed.extend_from_slice(&(field.len() as u64).to_be_bytes());
        seed.extend_from_slice(field.as_bytes());
    }
    seed
}

// The three dual-write derivations moved down to the backend contract, beside the
// `OnlineIntent` they are derived FROM. The PostgreSQL executor re-derives all three
// - the trigger identity it is allowed to mirror beneath, and the function body it
// proves the live trigger against - and a vendor crate cannot reach into the engine.
// These doors keep the engine's own call sites and its byte budget unchanged: the cap
// is still read once, from the registered backend that imposes it.
pub(crate) fn dual_write_fn_name(vendors: VendorSet, table: &str, from: &str, to: &str) -> String {
    zeroship_migrate_backend::capability::dual_write_fn_name(
        table,
        from,
        to,
        crate::render::backends::generated_ident_max_bytes(vendors),
    )
}

/// Deterministically derive the dual-write trigger name (see [`dual_write_fn_name`]).
pub(crate) fn dual_write_trg_name(vendors: VendorSet, table: &str, from: &str, to: &str) -> String {
    zeroship_migrate_backend::capability::dual_write_trg_name(
        table,
        from,
        to,
        crate::render::backends::generated_ident_max_bytes(vendors),
    )
}

/// The deterministic, no-AI author for the canonical online column-rename
/// expand-contract sequence.
///
/// Like [`crate::plan::author::DeterministicAuthor`], it emits provably-shaped,
/// project-schema-qualified SQL with correct [`MigrationFlags`] - but for the
/// *multi-deploy phased* online pattern, not the trivial additive set. The SQL
/// is byte-stable across re-authoring so the `Expand` checksums are reproducible.
///
/// # The `SET NOT NULL` lint
///
/// The author **never** emits a bare `ALTER TABLE ... ALTER COLUMN ... SET NOT NULL`
/// on a populated table: that takes an `ACCESS EXCLUSIVE` lock and full-scans to
/// validate, blocking writes. This rename leaves `<to>` nullable; a caller that
/// wants to tighten it to `NOT NULL` online authors the
/// `ADD CONSTRAINT ... CHECK (<to> IS NOT NULL) NOT VALID` -> `VALIDATE CONSTRAINT`
/// pair (a separate intent, not part of the rename). This author's output
/// therefore contains no `SET NOT NULL` by construction - the lint is "we don't
/// emit the dangerous form", enforced by tests (no `SET NOT NULL` substring).
#[derive(Debug, Clone)]
pub struct ExpandContractAuthor {
    /// The project schema every emitted statement is qualified into.
    project_schema: String,
    /// The declaring app (`app_...`) recorded on each migration.
    owner_app: String,
    /// The registered backend whose identifier spelling this author uses.
    dialect: DialectId,
    /// The backends this build ships, carried rather than reached for.
    vendors: VendorSet,
}

impl ExpandContractAuthor {
    /// Construct an author bound to a project schema, owner app, and backend identity.
    #[must_use]
    pub fn new(
        vendors: VendorSet,
        project_schema: impl Into<String>,
        owner_app: impl Into<String>,
        dialect: DialectId,
    ) -> Self {
        Self {
            project_schema: project_schema.into(),
            owner_app: owner_app.into(),
            dialect,
            vendors,
        }
    }

    /// Author the ordered, phased migration sequence for an [`OnlineIntent`].
    ///
    /// # Errors
    /// [`ExpandContractError::Invalid`] for an empty table/column name, a `from`
    /// equal to `to`, or an empty type.
    pub fn author(&self, intent: &OnlineIntent) -> Result<ExpandContractPlan, ExpandContractError> {
        match intent {
            OnlineIntent::RenameColumn {
                table,
                from,
                to,
                ty,
            } => self.author_rename(table, from, to, ty),
        }
    }

    /// Author the safe rollback of an outstanding rename expansion.
    ///
    /// The two approval-gated steps first remove the dual-write trigger and
    /// function, then drop the destination column. The source column remains
    /// untouched, returning the table to its pre-rename shape. These steps use
    /// identities distinct from the forward contract so an aborted rename can
    /// never make the destructive source-column drop appear completed.
    ///
    /// # Errors
    /// The same intent validation errors as [`Self::author`].
    pub fn author_abort(
        &self,
        intent: &OnlineIntent,
    ) -> Result<Vec<Migration>, ExpandContractError> {
        // Reuse the canonical author for validation and for the exact dual-write
        // cleanup SQL. This guarantees abort targets the objects expand created.
        let plan = self.author(intent)?;
        let OnlineIntent::RenameColumn {
            table,
            from,
            to,
            ty,
        } = intent;
        let id_seed = rename_id_seed(&self.project_schema, &self.owner_app, table, from, to, ty);
        let canonical_cleanup = &plan.contract[0];
        let cleanup = self.make(
            &format!("abort_drop_dual_write_{table}_{from}_{to}"),
            canonical_cleanup.up.clone(),
            canonical_cleanup.down.clone(),
            MigrationFlags {
                online: true,
                phase: Some(OnlinePhase::Contract),
                requires_approval: true,
                ..MigrationFlags::default()
            },
            vec![plan.trigger_version.clone()],
            &id_seed,
            EC_STEP_ABORT_C1,
        );
        let drop_destination = self.make(
            &format!("abort_drop_column_{table}_{to}"),
            format!(
                "ALTER TABLE {} DROP COLUMN {}",
                qualified(self.vendors, &self.project_schema, table, &self.dialect),
                quote_ident(self.vendors, to, &self.dialect)
            ),
            None,
            MigrationFlags {
                online: true,
                phase: Some(OnlinePhase::Contract),
                destructive: true,
                requires_approval: true,
                ..MigrationFlags::default()
            },
            vec![cleanup.version.clone()],
            &id_seed,
            EC_STEP_ABORT_C2,
        );
        Ok(vec![cleanup, drop_destination])
    }

    // A linear sequence builder: validate, then emit E1/E2/E3/C1/C2 in order.
    // Kept as one readable top-to-bottom function (the phased sequence reads best
    // contiguously); the pedantic line-count lint is allowed for exactly that.
    #[allow(clippy::too_many_lines)]
    fn author_rename(
        &self,
        table: &str,
        from: &str,
        to: &str,
        ty: &str,
    ) -> Result<ExpandContractPlan, ExpandContractError> {
        if table.is_empty() {
            return Err(ExpandContractError::Invalid("table name is empty".into()));
        }
        if from.is_empty() || to.is_empty() {
            return Err(ExpandContractError::Invalid("column name is empty".into()));
        }
        if from == to {
            return Err(ExpandContractError::Invalid(format!(
                "rename from and to are identical ('{from}')"
            )));
        }
        if ty.trim().is_empty() {
            return Err(ExpandContractError::Invalid("column type is empty".into()));
        }
        // Defense-in-depth: bound the spliced inputs at the author boundary, not
        // only at the downstream guard. table/from/to must be bare identifiers;
        // ty (spliced verbatim) must carry no statement separator / unbalanced
        // parens.
        validate_ident("table", table)?;
        validate_ident("from", from)?;
        validate_ident("to", to)?;
        validate_type(ty)?;

        let schema = &self.project_schema;
        let tbl_q = qualified(self.vendors, schema, table, &self.dialect);
        let from_q = quote_ident(self.vendors, from, &self.dialect);
        let to_q = quote_ident(self.vendors, to, &self.dialect);
        let fn_name = dual_write_fn_name(self.vendors, table, from, to);
        let trg_name = dual_write_trg_name(self.vendors, table, from, to);
        let fn_q = qualified(self.vendors, schema, &fn_name, &self.dialect);
        let trg_q = quote_ident(self.vendors, &trg_name, &self.dialect);

        // ---- E1: ADD COLUMN <to> <ty> (nullable, transactional, additive) ----
        let e1_up = format!("ALTER TABLE {tbl_q} ADD COLUMN {to_q} {ty}");
        // Structural rollback BEFORE the backfill runs is allowed:
        // dropping the just-added nullable column is a clean reverse.
        let e1_down = Some(format!("ALTER TABLE {tbl_q} DROP COLUMN {to_q}"));
        // The rename's STABLE identity seed. Every E1..C2 sub-step id is
        // `MigrationId::derive("ec", seed || step_index)`, so a re-lower of the
        // identical IR envelope (the production path re-lowers on EVERY deploy,
        // `deploy_migrate.rs`) reproduces byte-identical ids. The seed folds the
        // schema + owner + table + from + to + ty - every fact that identifies the
        // logical rename - and NOTHING per-run (no time, no random). A changed
        // rename (different to/ty) gets fresh ids; the same rename always maps to
        // the same obligation key (the E2 id), idempotent-skip key, contract ids,
        // and self-EXPAND exemption key. This is the determinism the cross-deploy
        // interlock leans on.
        let id_seed = rename_id_seed(schema, &self.owner_app, table, from, to, ty);
        let e1 = self.make(
            &format!("expand_add_column_{table}_{to}"),
            e1_up,
            e1_down,
            MigrationFlags {
                online: true,
                phase: Some(OnlinePhase::Expand),
                ..MigrationFlags::default()
            },
            Vec::new(),
            &id_seed,
            EC_STEP_E1,
        );

        // ---- E2: dual-write function + trigger (SECURITY INVOKER plpgsql) ----
        //
        // BEFORE INSERT OR UPDATE. The body is TOTAL: after it runs <from> and
        // <to> are ALWAYS equal (to wins), so no write can leave them divergent
        // for the contract's DROP COLUMN <from> to destroy. A BEFORE trigger
        // assigning NEW.* never re-fires, so there is no recursion by
        // construction; the only-from arm's IS DISTINCT FROM is the
        // amplification guard (a no-op UPDATE falls into the self-copy else arm),
        // not a recursion guard.
        let e2_sql = dual_write_sql(
            self.vendors,
            &self.dialect,
            &fn_q,
            &trg_q,
            &tbl_q,
            &from_q,
            &to_q,
        )?;
        let e2_up = e2_sql.install.clone();
        // Structural rollback of E2 (before backfill) tears down trigger then
        // function. Idempotence is the backend's stated contract for `remove`, so
        // this is safe if the install was only partly applied.
        let e2_down = Some(e2_sql.remove.clone());
        let e2 = self.make(
            &format!("expand_dual_write_{table}_{from}_{to}"),
            e2_up,
            e2_down,
            MigrationFlags {
                online: true,
                phase: Some(OnlinePhase::Expand),
                ..MigrationFlags::default()
            },
            vec![e1.version.clone()],
            &id_seed,
            EC_STEP_E2,
        );
        let trigger_version = e2.version.clone();

        // ---- E3: BACKFILL <to> := <from> WHERE <to> IS NULL ----
        //
        // Cursor on the PRIMARY KEY (resolved by the orchestrator / caller as the
        // backfill's cursor_columns tuple), NOT on <to> (the column being populated -
        // backfill.rs forbids paging on the mutated column). The backfill is a
        // data-mutation STEP driven by run_backfill during orchestration (v1.3),
        // not raw `up` SQL; we still mint a journaled marker migration for it so
        // the expand phase has a single recorded completion the gate can read.
        // depends_on [E2]: the trigger must be live before the backfill runs so a
        // concurrent write between batches is never lost to <from> only.
        //
        // The marker's `up` is a no-op SELECT (guard-safe, project-qualified by
        // the pinned search_path); the real work is run_backfill. This keeps E3
        // in the journal/gate timeline without the executor trying to run the
        // batched UPDATE as one statement.
        let backfill = BackfillSpec {
            // The E3 backfill targets the bound project schema (expand-contract
            // is a Confined-profile, single-project online change).
            schema: self.project_schema.clone(),
            table: table.to_string(),
            // The orchestrator/caller supplies the real PK as cursor_columns; we
            // default to "id" (the platform's conventional PK) and document that
            // a caller overrides it for a non-`id` PK.
            cursor_columns: vec!["id".to_string()],
            cursor_stability: crate::model::ir::CursorStability::GuardUpdates,
            cursor_contract: None,
            batch_size: 1000,
            set_clause: format!("{to_q} = {from_q}"),
            per_row: std::collections::BTreeMap::new(),
            filter: Some(format!("{to_q} IS NULL")),
            name: format!("backfill_{table}_{from}_to_{to}"),
        };
        let e3_up = format!(
            "SELECT 1 /* online backfill marker: {} */",
            backfill.backfill_id()
        );
        let e3 = self.make(
            &format!("expand_backfill_{table}_{from}_to_{to}"),
            e3_up,
            // The backfill is roll-FORWARD-only past this point: once
            // data is mirrored, there is no structural down. Explicitly irreversible.
            None,
            MigrationFlags {
                online: true,
                phase: Some(OnlinePhase::Expand),
                ..MigrationFlags::default()
            },
            vec![e2.version.clone()],
            &id_seed,
            EC_STEP_E3,
        );

        // ---- C1: remove the dual-write trigger (gated, depends_on E2) ----
        // Last use of both halves, so this consumes the pair the renderer built.
        let c1 = self.make(
            &format!("contract_drop_dual_write_{table}_{from}_{to}"),
            e2_sql.remove,
            // Re-creating the dual-write is the reverse (best-effort); the
            // contract is gated + roll-forward-preferred, but a clean down exists.
            Some(e2_sql.install),
            MigrationFlags {
                online: true,
                phase: Some(OnlinePhase::Contract),
                requires_approval: true,
                ..MigrationFlags::default()
            },
            vec![e2.version.clone()],
            &id_seed,
            EC_STEP_C1,
        );

        // ---- C2: DROP COLUMN <from> (destructive, gated) ----
        //
        // depends_on [E1, E3, C1]: E1 is the column it reverses; E3 (the backfill)
        // MUST be net-applied first - dropping <from> before every pre-existing
        // row's value is mirrored into <to> would lose un-backfilled data; and C1
        // (DROP TRIGGER + DROP FUNCTION) MUST run before C2 - the dual-write
        // trigger references <from>, so dropping the column while the trigger is
        // still live errors / leaves a dangling reference. In a contract-only
        // deploy both C1 and C2 are indegree-0 and would otherwise order only by
        // incidental UUIDv7 version; the explicit edge makes "drop the trigger
        // before the column it reads" a structural guarantee. The dual-write
        // trigger only covers rows written DURING the transition; the backfill
        // covers the rows that predate it. So the destructive drop is gated on the
        // backfill's journaled completion (the backfill step records
        // completion in the journal -> the gate reads one timeline).
        // Deliberately NO CASCADE. A column another object depends on - a generated
        // column reading it, a view, an EXCLUDE constraint - makes PostgreSQL refuse
        // this drop, and the refusal names the dependent:
        //
        //   ERROR:  cannot drop column qty of table t because other objects depend on it
        //   DETAIL:  column total of table t depends on column qty of table t
        //
        // Measured on PostgreSQL 18.4, along with what the hint would cost: `DROP
        // COLUMN qty CASCADE` reports `drop cascades to column total` and leaves the
        // table without it. Taking the hint would turn a stop into the silent loss of
        // a column nobody asked to drop, inside a step that already carries
        // `destructive: true` for the ONE column the operator did name. So a rename
        // whose old column is read by a generated column stops here rather than
        // completing, and that is the better of the two failures.
        let c2_up = format!("ALTER TABLE {tbl_q} DROP COLUMN {from_q}");
        let c2 = self.make(
            &format!("contract_drop_column_{table}_{from}"),
            c2_up,
            // Dropping a column is irreversible (the data is gone); no true down.
            None,
            MigrationFlags {
                online: true,
                phase: Some(OnlinePhase::Contract),
                destructive: true,
                requires_approval: true,
                ..MigrationFlags::default()
            },
            vec![e1.version.clone(), e3.version.clone(), c1.version.clone()],
            &id_seed,
            EC_STEP_C2,
        );

        Ok(ExpandContractPlan {
            plan_version: None,
            expand: vec![e1, e2, e3],
            contract: vec![c1, c2],
            backfill,
            trigger_version,
            intent: OnlineIntent::RenameColumn {
                table: table.to_string(),
                from: from.to_string(),
                to: to.to_string(),
                ty: ty.to_string(),
            },
        })
    }

    /// Build a [`Migration`] from rendered `up`/`down` SQL + flags + deps.
    ///
    /// `id_seed` is the rename's stable identity image (see [`rename_id_seed`]) and
    /// `step_index` is one of the `EC_STEP_*` constants - together they
    /// DETERMINISTICALLY derive the sub-step's `version` via [`MigrationId::derive`]
    /// A re-lower of the identical rename reproduces the SAME id, which is
    /// what the cross-deploy obligation key + idempotent-skip + auto-discharge +
    /// self-EXPAND exemption all depend on. The version is NEVER
    /// `MigrationId::generate()` (random per call), which would re-key the
    /// obligation on every deploy.
    // The id-derivation pair (`id_seed`, `step_index`) pushes this to 8 args; they
    // are one logical unit (the deterministic sub-version derivation) and
    // bundling them into a struct would only relocate the same fields. Matches the
    // crate's `lower_ir_rename` / column-rename rebuild allow pattern.
    #[allow(clippy::too_many_arguments)]
    fn make(
        &self,
        name: &str,
        up: String,
        down: Option<String>,
        flags: MigrationFlags,
        depends_on: Vec<MigrationId>,
        id_seed: &[u8],
        step_index: u8,
    ) -> Migration {
        let checksum = Checksum::of(&crate::model::migration::ChecksumInput {
            up: &up,
            down: down.as_deref(),
            flags: &flags,
            owner_app: &self.owner_app,
            depends_on: &depends_on,
            supersedes: &[],
            preconditions: &[],
        });
        // Deterministic sub-version: fold the step index into the rename's
        // stable seed so E1..C2 each get a distinct, reproducible id.
        let mut seed = id_seed.to_vec();
        seed.push(step_index);
        Migration {
            version: MigrationId::derive("ec", &seed),
            name: name.to_string(),
            up,
            down,
            checksum,
            flags,
            owner_app: self.owner_app.clone(),
            depends_on,
            supersedes: Vec::new(),
            preconditions: Vec::new(),
            existence_guard: None,
            effect: None,
        }
    }
}

// The engine asks `SchemaRenderer::dual_write_trigger`, a REQUIRED method, so the three
// backends that do not resolve a rename this way say so at their own definition sites
// rather than being papered over by a fallthrough here. The PostgreSQL implementation
// lives in `zeroship-migrate-postgres/src/dual_write.rs`.
fn dual_write_sql(
    vendors: VendorSet,
    dialect: &DialectId,
    fn_q: &str,
    trg_q: &str,
    tbl_q: &str,
    from_q: &str,
    to_q: &str,
) -> Result<zeroship_migrate_backend::schema::DualWriteTriggerSql, ExpandContractError> {
    crate::render::backends::schema_renderer(vendors, dialect)
        .dual_write_trigger(&zeroship_migrate_backend::schema::DualWriteTriggerSpec {
            function: fn_q,
            trigger: trg_q,
            table: tbl_q,
            from: from_q,
            to: to_q,
        })
        .ok_or_else(|| {
            // Reachable only from a backend that answered `ExpandContract` for
            // `column_rename_strategy` and `None` here - one decision contradicting
            // itself. Fail closed rather than author an expand step with no trigger,
            // which would leave the contract's `DROP COLUMN <from>` destroying writes
            // that never got mirrored.
            ExpandContractError::Invalid(format!(
                "{dialect} resolves a column rename by expand-contract but registers no \
                 dual-write trigger, so the expand step cannot be authored"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author() -> ExpandContractAuthor {
        ExpandContractAuthor::new(
            crate::test_fixtures::VENDORS,
            "proj_acme",
            "app_acme",
            crate::test_fixtures::POSTGRES,
        )
    }

    fn rename() -> OnlineIntent {
        OnlineIntent::RenameColumn {
            table: "users".into(),
            from: "email".into(),
            to: "email_address".into(),
            ty: "text".into(),
        }
    }

    #[test]
    fn emits_expand_then_contract_with_correct_phases() {
        let plan = author().author(&rename()).expect("author");
        assert_eq!(plan.expand.len(), 3, "E1, E2, E3");
        assert_eq!(plan.contract.len(), 2, "C1, C2");
        for m in &plan.expand {
            assert!(m.flags.online, "expand migrations are online");
            assert_eq!(m.flags.phase, Some(OnlinePhase::Expand), "{}", m.name);
        }
        for m in &plan.contract {
            assert!(m.flags.online, "contract migrations are online");
            assert_eq!(m.flags.phase, Some(OnlinePhase::Contract), "{}", m.name);
        }
    }

    #[test]
    fn all_sql_is_project_schema_qualified() {
        let plan = author().author(&rename()).expect("author");
        for m in plan.all() {
            assert!(
                m.up.contains("\"proj_acme\".\"users\"")
                    || m.up.contains("\"proj_acme\".\"zsdw_")
                    || m.up.contains("/* online backfill marker"),
                "up not schema-qualified: {}\n{}",
                m.name,
                m.up
            );
            // No reference to any other schema.
            assert!(!m.up.contains("public."), "{}: {}", m.name, m.up);
        }
    }

    #[test]
    fn e1_adds_nullable_column_transactionally() {
        let plan = author().author(&rename()).expect("author");
        let e1 = &plan.expand[0];
        assert_eq!(
            e1.up,
            "ALTER TABLE \"proj_acme\".\"users\" ADD COLUMN \"email_address\" text"
        );
        assert!(e1.flags.transactional);
        assert!(!e1.flags.destructive);
        assert!(!e1.flags.requires_approval);
        // No NOT NULL on the added column (additive-safe online).
        assert!(!e1.up.contains("NOT NULL"));
    }

    #[test]
    fn e2_trigger_fn_is_plpgsql_invoker_never_security_definer() {
        let plan = author().author(&rename()).expect("author");
        let e2 = &plan.expand[1];
        assert!(e2.up.contains("LANGUAGE plpgsql"), "{}", e2.up);
        assert!(
            !e2.up.to_ascii_uppercase().contains("SECURITY DEFINER"),
            "dual-write fn must NOT be SECURITY DEFINER (escalation): {}",
            e2.up
        );
        assert!(e2.up.contains("BEFORE INSERT OR UPDATE"), "{}", e2.up);
        assert!(e2.up.contains("RETURN NEW"), "{}", e2.up);
    }

    #[test]
    fn e2_has_recursion_amplification_guard() {
        let plan = author().author(&rename()).expect("author");
        let e2 = &plan.expand[1];
        // IS DISTINCT FROM guards prevent write amplification.
        assert!(
            e2.up.contains("IS DISTINCT FROM"),
            "missing distinct-from amplification guard: {}",
            e2.up
        );
        assert!(e2.up.contains("IS NOT DISTINCT FROM"), "{}", e2.up);
    }

    #[test]
    fn never_emits_bare_set_not_null() {
        // The online author must NOT emit a bare SET NOT NULL.
        let plan = author().author(&rename()).expect("author");
        for m in plan.all() {
            assert!(
                !m.up.to_ascii_uppercase().contains("SET NOT NULL"),
                "online author emitted a bare SET NOT NULL in {}: {}",
                m.name,
                m.up
            );
        }
    }

    #[test]
    fn backfill_cursor_is_pk_not_the_backfilled_column() {
        let plan = author().author(&rename()).expect("author");
        // The backfill pages on the PK ("id"), not on the column it populates.
        assert_eq!(plan.backfill.cursor_columns, ["id"]);
        assert_ne!(plan.backfill.cursor_columns, ["email_address"]);
        assert_eq!(plan.backfill.table, "users");
        assert!(plan.backfill.set_clause.contains("\"email_address\""));
        assert!(plan.backfill.set_clause.contains("\"email\""));
        assert_eq!(
            plan.backfill.filter.as_deref(),
            Some("\"email_address\" IS NULL")
        );
    }

    #[test]
    fn depends_on_chain_is_correct_and_acyclic() {
        let plan = author().author(&rename()).expect("author");
        let (e1, e2, e3) = (&plan.expand[0], &plan.expand[1], &plan.expand[2]);
        let (c1, c2) = (&plan.contract[0], &plan.contract[1]);
        // E1 has no deps.
        assert!(e1.depends_on.is_empty());
        // E2 depends on E1.
        assert_eq!(e2.depends_on, vec![e1.version.clone()]);
        // E3 depends on E2 (trigger live before backfill).
        assert_eq!(e3.depends_on, vec![e2.version.clone()]);
        // C1 depends on E2 (the trigger it drops).
        assert_eq!(c1.depends_on, vec![e2.version.clone()]);
        // C2 depends on E1 (the column add it reverses), E3 (the backfill -
        // dropping <from> before the backfill mirrors pre-existing rows loses
        // data), AND C1 (the trigger drop MUST run before the column it reads -
        // a structural guarantee, not incidental UUIDv7 ordering).
        assert_eq!(
            c2.depends_on,
            vec![e1.version.clone(), e3.version.clone(), c1.version.clone()]
        );
        assert!(
            c2.depends_on.contains(&c1.version),
            "C2 (DROP COLUMN) must declare C1 (DROP TRIGGER) as a dependency"
        );
        // trigger_version is E2.
        assert_eq!(plan.trigger_version, e2.version);
    }

    #[test]
    fn contract_migrations_require_approval_and_drop_is_destructive() {
        let plan = author().author(&rename()).expect("author");
        let (c1, c2) = (&plan.contract[0], &plan.contract[1]);
        assert!(c1.flags.requires_approval, "DROP TRIGGER/FUNCTION is gated");
        assert!(c2.flags.requires_approval, "DROP COLUMN is gated");
        assert!(c2.flags.destructive, "DROP COLUMN is destructive");
        // C1 is not "destructive" in the data-loss sense (no rows lost dropping
        // a trigger), but is gated.
        assert!(!c1.flags.destructive);
    }

    #[test]
    fn abort_plan_removes_dual_write_then_the_new_column() {
        let expand = author().author(&rename()).expect("expand author");
        let abort = author().author_abort(&rename()).expect("abort author");

        assert_eq!(
            abort.len(),
            2,
            "trigger cleanup and destination-column drop"
        );
        assert_eq!(
            abort[0].up, expand.contract[0].up,
            "abort must remove the exact dual-write objects created by expand"
        );
        assert_eq!(
            abort[0].depends_on,
            vec![expand.trigger_version],
            "the trigger must have been created before it can be removed"
        );
        assert_eq!(
            abort[1].up,
            "ALTER TABLE \"proj_acme\".\"users\" DROP COLUMN \"email_address\""
        );
        assert_eq!(abort[1].depends_on, vec![abort[0].version.clone()]);
        assert!(abort.iter().all(|step| step.flags.requires_approval));
        assert!(abort[1].flags.destructive);
        assert!(abort
            .iter()
            .all(|step| { step.flags.online && step.flags.phase == Some(OnlinePhase::Contract) }));
    }

    #[test]
    fn abort_plan_is_deterministic_and_does_not_reuse_contract_ids() {
        let first = author()
            .author_abort(&rename())
            .expect("first abort author");
        let second = author()
            .author_abort(&rename())
            .expect("second abort author");
        let contract = author()
            .author(&rename())
            .expect("contract author")
            .contract;

        assert_eq!(first.len(), second.len());
        for (left, right) in first.iter().zip(&second) {
            assert_eq!(left.version, right.version);
            assert_eq!(left.checksum, right.checksum);
            assert_eq!(left.up, right.up);
        }
        let contract_ids = contract
            .iter()
            .map(|step| step.version.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(first
            .iter()
            .all(|step| !contract_ids.contains(step.version.as_str())));
    }

    #[test]
    fn expand_sql_is_byte_stable_across_reauthoring() {
        // Re-authoring the same intent yields identical Expand/Contract SQL AND
        // identical sub-step ids + checksums: the E1..C2 versions are
        // DETERMINISTICALLY derived from the rename's stable seed
        // (schema+owner+table+from+to+ty) plus the step index. The checksum folds
        // `depends_on`, which holds deterministic sibling ids, so the FULL
        // checksum is stable too. This is the property a re-lower of the
        // identical IR envelope on every deploy relies on.
        let p1 = author().author(&rename()).expect("author 1");
        let p2 = author().author(&rename()).expect("author 2");
        assert_eq!(
            p1.trigger_version, p2.trigger_version,
            "E2 obligation key is deterministic"
        );
        for (a, b) in p1.expand.iter().zip(&p2.expand) {
            assert_eq!(
                a.version, b.version,
                "expand sub-step id must be deterministic: {}",
                a.name
            );
            assert_eq!(a.up, b.up, "expand up SQL must be byte-stable: {}", a.name);
            assert_eq!(a.down, b.down, "expand down SQL must be byte-stable");
            assert_eq!(
                a.depends_on, b.depends_on,
                "expand depends_on must be deterministic"
            );
            assert_eq!(
                a.checksum, b.checksum,
                "expand checksum must be stable: {}",
                a.name
            );
        }
        for (a, b) in p1.contract.iter().zip(&p2.contract) {
            assert_eq!(
                a.version, b.version,
                "contract sub-step id must be deterministic: {}",
                a.name
            );
            assert_eq!(
                a.up, b.up,
                "contract up SQL must be byte-stable: {}",
                a.name
            );
            assert_eq!(
                a.depends_on, b.depends_on,
                "contract depends_on must be deterministic"
            );
            assert_eq!(
                a.checksum, b.checksum,
                "contract checksum must be stable: {}",
                a.name
            );
        }
        assert_eq!(p1.backfill.backfill_id(), p2.backfill.backfill_id());
    }

    #[test]
    fn substep_ids_are_deterministic_and_distinct() {
        // Every E1..C2 id is derived (not random), so two
        // authorings of the SAME rename produce byte-identical ids; and the five
        // sub-steps are mutually DISTINCT (the step-index fold keeps E1..C2 apart).
        let p1 = author().author(&rename()).expect("author 1");
        let p2 = author().author(&rename()).expect("author 2");
        // Compare id strings directly across the two authorings.
        for (a, b) in p1.all().iter().zip(p2.all().iter()) {
            assert_eq!(a.version, b.version, "sub-step id is deterministic");
        }
        // All five sub-step ids are distinct.
        let mut seen = std::collections::HashSet::new();
        for m in p1.all() {
            assert!(
                seen.insert(m.version.as_str().to_string()),
                "sub-step ids must be distinct"
            );
        }
        // A DIFFERENT rename (different `to`) gets a different obligation key.
        let other = author()
            .author(&OnlineIntent::RenameColumn {
                table: "users".into(),
                from: "email".into(),
                to: "contact".into(),
                ty: "text".into(),
            })
            .expect("author other");
        assert_ne!(
            p1.trigger_version, other.trigger_version,
            "a semantically different rename gets a fresh obligation key"
        );
    }

    #[test]
    fn rejects_invalid_intents() {
        let a = author();
        assert!(matches!(
            a.author(&OnlineIntent::RenameColumn {
                table: String::new(),
                from: "x".into(),
                to: "y".into(),
                ty: "text".into(),
            }),
            Err(ExpandContractError::Invalid(_))
        ));
        // from == to.
        assert!(matches!(
            a.author(&OnlineIntent::RenameColumn {
                table: "t".into(),
                from: "x".into(),
                to: "x".into(),
                ty: "text".into(),
            }),
            Err(ExpandContractError::Invalid(_))
        ));
        // empty type.
        assert!(matches!(
            a.author(&OnlineIntent::RenameColumn {
                table: "t".into(),
                from: "x".into(),
                to: "y".into(),
                ty: "   ".into(),
            }),
            Err(ExpandContractError::Invalid(_))
        ));
    }

    #[test]
    fn rejects_type_with_injected_statement_separator() {
        // A `ty` carrying a second statement is rejected by the AUTHOR (before
        // the downstream guard ever sees it) - safe by construction.
        let a = author();
        let err = a
            .author(&OnlineIntent::RenameColumn {
                table: "users".into(),
                from: "email".into(),
                to: "email_address".into(),
                ty: "text; CREATE TABLE control.evil(x int)".into(),
            })
            .expect_err("a type with a ';' second statement must be rejected");
        assert!(
            matches!(err, ExpandContractError::Invalid(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn rejects_type_with_unbalanced_parens() {
        let a = author();
        assert!(matches!(
            a.author(&OnlineIntent::RenameColumn {
                table: "users".into(),
                from: "email".into(),
                to: "email_address".into(),
                ty: "numeric(10".into(),
            }),
            Err(ExpandContractError::Invalid(_))
        ));
    }

    #[test]
    fn accepts_legitimate_parameterized_type() {
        // A real parameterized type (balanced parens, no ';') is still accepted.
        let a = author();
        let plan = a
            .author(&OnlineIntent::RenameColumn {
                table: "amounts".into(),
                from: "old".into(),
                to: "new".into(),
                ty: "numeric(10,2)".into(),
            })
            .expect("a balanced parameterized type is valid");
        assert!(plan.expand[0].up.contains("numeric(10,2)"));
    }

    #[test]
    fn rejects_injection_in_table_from_to_identifiers() {
        let a = author();
        // Injection in `table`.
        assert!(matches!(
            a.author(&OnlineIntent::RenameColumn {
                table: "users\"; DROP TABLE control.users; --".into(),
                from: "email".into(),
                to: "email_address".into(),
                ty: "text".into(),
            }),
            Err(ExpandContractError::Invalid(_))
        ));
        // Injection / schema-qualification in `from`.
        assert!(matches!(
            a.author(&OnlineIntent::RenameColumn {
                table: "users".into(),
                from: "control.secret".into(),
                to: "email_address".into(),
                ty: "text".into(),
            }),
            Err(ExpandContractError::Invalid(_))
        ));
        // Injection in `to`.
        assert!(matches!(
            a.author(&OnlineIntent::RenameColumn {
                table: "users".into(),
                from: "email".into(),
                to: "to\"; DROP".into(),
                ty: "text".into(),
            }),
            Err(ExpandContractError::Invalid(_))
        ));
    }

    #[test]
    fn long_names_are_capped_to_63_bytes_and_match_up_down() {
        let intent = OnlineIntent::RenameColumn {
            table: "t".repeat(40),
            from: "f".repeat(20),
            to: "g".repeat(20),
            ty: "text".into(),
        };
        let plan = author().author(&intent).expect("author");
        let e2 = &plan.expand[1];
        // The fn/trg names embedded in E2's up must be <=63 bytes and appear in
        // both E2.up (CREATE) and E2.down (DROP) identically.
        let vendors = crate::test_fixtures::VENDORS;
        let max = crate::render::backends::generated_ident_max_bytes(vendors);
        let fn_name =
            dual_write_fn_name(vendors, &"t".repeat(40), &"f".repeat(20), &"g".repeat(20));
        let trg_name =
            dual_write_trg_name(vendors, &"t".repeat(40), &"f".repeat(20), &"g".repeat(20));
        assert!(fn_name.len() <= max, "fn {} bytes", fn_name.len());
        assert!(trg_name.len() <= max, "trg {} bytes", trg_name.len());
        assert!(e2.up.contains(&fn_name), "up must use capped fn name");
        assert!(e2.down.as_ref().unwrap().contains(&fn_name));
        assert!(e2.down.as_ref().unwrap().contains(&trg_name));
    }

    // The scope gate over the sequence authored here is UNCONDITIONAL, and it does
    // not live in this module. The engine resolves the scope version through
    // `PlanStep::approval_scope_version` - the rename's plan-group version, falling
    // back to the E2 `trigger_version` when the expand chain is empty - and
    // `enforce_online_scope_if_pending` refuses with `ApprovalNotScoped` under
    // `ApprovalScope::Versions({})`. That fallback is the whole point: the gate it
    // replaced keyed on the FIRST entry of the expand chain, so an EMPTY chain
    // yielded no key, skipped the gate entirely, and fell through to an `Ok(empty)`
    // return - a fail-OPEN a direct seam caller could ride. The refusal fires before
    // any DDL or backfill, so it needs no connection and `engine.rs` owns its test.
}
