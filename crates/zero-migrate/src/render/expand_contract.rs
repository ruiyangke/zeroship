//! The `ExpandContractAuthor` — zero-downtime **online column RENAME** via
//! trigger dual-write (the zero-downtime expand-contract pattern).
//!
//! A column rename cannot be done as a single `ALTER TABLE … RENAME COLUMN`
//! without breaking every running deploy that still reads/writes the old name:
//! the rename is atomic, but the *fleet* is not — old code and new code run
//! concurrently across a rolling deploy. The expand-contract pattern makes the
//! two shapes **coexist** so neither generation of code ever sees a missing
//! column:
//!
//! ```text
//! EXPAND  (deploy N, lands BEFORE code switches to the new name)
//!   E1  ADD COLUMN <to> <ty>            -- nullable, transactional, additive
//!   E2  CREATE FUNCTION + TRIGGER        -- BEFORE INSERT/UPDATE dual-write
//!       (mirror <from> ⇄ <to>)             depends_on [E1]
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
//! - **E1 is nullable + transactional.** A bare `ADD COLUMN … NOT NULL` over a
//!   populated table rewrites the whole table under `ACCESS EXCLUSIVE`; the
//!   online author MUST NOT emit it (and MUST NOT emit a bare `SET NOT NULL`
//!   either — see [`ExpandContractAuthor`] for the `CHECK … NOT VALID` →
//!   `VALIDATE` lint). This stops at the nullable column + dual-write + backfill;
//!   tightening to `NOT NULL` is a separate authored step.
//! - **E2 is `SECURITY INVOKER` (the plpgsql default), NOT `SECURITY DEFINER`.**
//!   A `DEFINER` trigger would run with the *function owner's* (the migrator's)
//!   privileges for every app write — an escalation primitive, and guard-denied
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
//!   forbids paging on the column it mutates (see [`crate::apply::backend::postgres::backfill`]). E3
//!   depends on E2 so the trigger is live before the backfill runs — otherwise a
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
//! `Expand` checksums — exactly like [`crate::plan::author`]'s index-name determinism.

use crate::model::backfill::BackfillSpec;
use crate::model::migration::{Checksum, Migration, MigrationFlags, MigrationId, OnlinePhase};

/// A high-level online-migration intent the [`ExpandContractAuthor`] expands
/// into an ordered, phased [`Migration`] sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnlineIntent {
    /// Rename column `from` → `to` (of type `ty`) on `table`, online, via the
    /// canonical expand-contract dual-write sequence.
    RenameColumn {
        /// The table the column lives on (bare; project-schema-qualified on emit).
        table: String,
        /// The existing column name.
        from: String,
        /// The new column name.
        to: String,
        /// The Postgres type of the column (emitted verbatim for the new column).
        ty: String,
    },
}

/// A failure to author an online expand-contract sequence.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExpandContractError {
    /// A request field was empty or invalid (empty table/column name, `from`
    /// equal to `to`, empty type).
    #[error("invalid online intent: {0}")]
    Invalid(String),
}

/// The full ordered output of [`ExpandContractAuthor::author`] — the expand and
/// contract migrations for one online intent, with the `depends_on` chain wired.
///
/// The expand migrations ([`expand`](Self::expand)) and contract migrations
/// ([`contract`](Self::contract)) are exposed separately so a caller (the
/// control plane) can bundle the expand into deploy N and the contract into a
/// later deploy N+1 — the cross-deploy partition the engine gate enforces. The
/// flat [`all`](Self::all) view is the input to [`plan`](crate::engine::MigrationEngine::plan).
#[derive(Debug, Clone)]
pub struct ExpandContractPlan {
    /// The stable logical identity of the owning authored plan. IR lowering
    /// stamps this after the ordered plan is assembled; declarative callers that
    /// do not have an outer plan identity leave it `None` and retain the legacy
    /// first-expand-step fallback.
    pub plan_version: Option<MigrationId>,
    /// E1, E2, E3 in order (add column, dual-write trigger, backfill marker).
    pub expand: Vec<Migration>,
    /// C1, C2 in order (drop trigger/function, drop old column).
    pub contract: Vec<Migration>,
    /// The structured backfill spec for E3, to be driven by
    /// [`run_backfill`](crate::apply::backend::postgres::backfill::run_backfill) during orchestration.
    pub backfill: BackfillSpec,
    /// The version of the E2 trigger migration — the dependency every contract
    /// step and the gate keys on as "the expand". Carried out so the
    /// orchestrator / gate need not re-derive it.
    pub trigger_version: MigrationId,
    /// The neutral [`OnlineIntent`] this plan was authored from. Carried so the
    /// generic declarative apply path hands the **intent** (not the PG-DDL plan)
    /// to the [`OnlineSchemaChange`](crate::apply::backend::OnlineSchemaChange) seam — the Postgres impl ignores it and
    /// runs the pre-authored [`expand`](Self::expand) steps verbatim, while a
    /// future engine lowers the intent to its own native online DDL.
    pub intent: OnlineIntent,
}

impl ExpandContractPlan {
    /// All migrations (expand then contract) in apply order — the input to
    /// [`MigrationEngine::plan`](crate::engine::MigrationEngine::plan).
    #[must_use]
    pub fn all(&self) -> Vec<Migration> {
        self.expand.iter().chain(&self.contract).cloned().collect()
    }
}

/// Quote a Postgres identifier (double embedded quotes, wrap in `"`). Mirrors
/// [`crate::plan::author`]'s quoting so output is injection-safe.
pub(crate) fn quote_ident(ident: &str) -> String {
    crate::render::dml::escape_quote_ident(ident)
}

/// Validate a bare SQL identifier: non-empty, starts with a letter/underscore,
/// and contains only `[A-Za-z0-9_]`. Mirrors [`crate::apply::backend::postgres::backfill`]'s `validate_ident`
/// so `table`/`from`/`to` are safe-by-construction at the AUTHOR boundary — not
/// only safe-by-quoting downstream. Rejects schema-qualified names
/// (`control.users`), quote-injection (`t"; DROP …`), whitespace, punctuation.
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
/// parentheses, so we reject a `ty` that has either — closing
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
pub(crate) fn qualified(schema: &str, object: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(object))
}

/// Sub-step indices for the online-rename sequence — folded into the
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
/// every fact that identifies the logical rename — `schema`, `owner`, `table`,
/// `from`, `to`, `ty`. Length-prefixing each field makes the encoding injective
/// (so `("a","bc")` and `("ab","c")` never collide). NOTHING per-run is folded
/// (no time, no random), so re-lowering the identical IR envelope reproduces the
/// SAME seed → the SAME E1..C2 ids. A semantically different rename (different
/// `to`/`ty`) produces a different seed → fresh ids.
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

/// The Postgres identifier length limit (`NAMEDATALEN - 1`), in bytes. Mirrors
/// [`crate::plan::author`]'s constant: an over-long name is silently truncated
/// server-side, desyncing the name we emit in `up`/`down`. We cap it ourselves.
const PG_MAX_IDENT_BYTES: usize = 63;

/// Deterministically derive the dual-write function name for a rename, capped to
/// Postgres's 63-byte identifier limit. Stable across re-authoring (so the
/// `down` and the orchestrator target the same object), with a hash suffix to
/// disambiguate over-long natural names — mirroring [`crate::plan::author`]'s
/// `index_name` discipline.
pub(crate) fn dual_write_fn_name(table: &str, from: &str, to: &str) -> String {
    capped_name(&format!("zsdw_{table}_{from}_{to}_fn"))
}

/// Deterministically derive the dual-write trigger name (see
/// [`dual_write_fn_name`]).
pub(crate) fn dual_write_trg_name(table: &str, from: &str, to: &str) -> String {
    capped_name(&format!("zsdw_{table}_{from}_{to}_trg"))
}

/// Cap a natural name to ≤63 bytes deterministically: verbatim when it fits,
/// else a readable prefix + a 10-hex-char hash of the full natural name (so
/// distinct long inputs stay distinct). Identical algorithm to
/// [`crate::plan::author`]'s `index_name`, factored for the function/trigger names.
fn capped_name(natural: &str) -> String {
    use sha2::{Digest, Sha256};
    if natural.len() <= PG_MAX_IDENT_BYTES {
        return natural.to_string();
    }
    let digest = Sha256::digest(natural.as_bytes());
    let suffix = hex::encode(&digest[..5]); // 10 hex chars
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

/// The deterministic, no-AI author for the canonical online column-rename
/// expand-contract sequence.
///
/// Like [`crate::plan::author::DeterministicAuthor`], it emits provably-shaped,
/// project-schema-qualified SQL with correct [`MigrationFlags`] — but for the
/// *multi-deploy phased* online pattern, not the trivial additive set. The SQL
/// is byte-stable across re-authoring so the `Expand` checksums are reproducible.
///
/// # The `SET NOT NULL` lint
///
/// The author **never** emits a bare `ALTER TABLE … ALTER COLUMN … SET NOT NULL`
/// on a populated table: that takes an `ACCESS EXCLUSIVE` lock and full-scans to
/// validate, blocking writes. This rename leaves `<to>` nullable; a caller that
/// wants to tighten it to `NOT NULL` online authors the
/// `ADD CONSTRAINT … CHECK (<to> IS NOT NULL) NOT VALID` → `VALIDATE CONSTRAINT`
/// pair (a separate intent, not part of the rename). This author's output
/// therefore contains no `SET NOT NULL` by construction — the lint is "we don't
/// emit the dangerous form", enforced by tests (no `SET NOT NULL` substring).
#[derive(Debug, Clone)]
pub struct ExpandContractAuthor {
    /// The project schema every emitted statement is qualified into.
    project_schema: String,
    /// The declaring app (`app_…`) recorded on each migration.
    owner_app: String,
}

impl ExpandContractAuthor {
    /// Construct an author bound to a project schema + owner app.
    #[must_use]
    pub fn new(project_schema: impl Into<String>, owner_app: impl Into<String>) -> Self {
        Self {
            project_schema: project_schema.into(),
            owner_app: owner_app.into(),
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
                qualified(&self.project_schema, table),
                quote_ident(to)
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
        let tbl_q = qualified(schema, table);
        let from_q = quote_ident(from);
        let to_q = quote_ident(to);
        let fn_name = dual_write_fn_name(table, from, to);
        let trg_name = dual_write_trg_name(table, from, to);
        let fn_q = qualified(schema, &fn_name);
        let trg_q = quote_ident(&trg_name);

        // ---- E1: ADD COLUMN <to> <ty> (nullable, transactional, additive) ----
        let e1_up = format!("ALTER TABLE {tbl_q} ADD COLUMN {to_q} {ty}");
        // Structural rollback BEFORE the backfill runs is allowed:
        // dropping the just-added nullable column is a clean reverse.
        let e1_down = Some(format!("ALTER TABLE {tbl_q} DROP COLUMN {to_q}"));
        // The rename's STABLE identity seed. Every E1..C2 sub-step id is
        // `MigrationId::derive("ec", seed || step_index)`, so a re-lower of the
        // identical IR envelope (the production path re-lowers on EVERY deploy,
        // `deploy_migrate.rs`) reproduces byte-identical ids. The seed folds the
        // schema + owner + table + from + to + ty — every fact that identifies the
        // logical rename — and NOTHING per-run (no time, no random). A changed
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
        let e2_up = build_dual_write_sql(&fn_q, &trg_q, &tbl_q, &from_q, &to_q);
        // Structural rollback of E2 (before backfill) tears down trigger then
        // function — IF EXISTS so it is idempotent / safe if partly applied.
        let e2_down = Some(format!(
            "DROP TRIGGER IF EXISTS {trg_q} ON {tbl_q}; DROP FUNCTION IF EXISTS {fn_q}()"
        ));
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
        // backfill's cursor_columns tuple), NOT on <to> (the column being populated —
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

        // ---- C1: DROP TRIGGER + DROP FUNCTION (gated, depends_on E2) ----
        let c1_up =
            format!("DROP TRIGGER IF EXISTS {trg_q} ON {tbl_q}; DROP FUNCTION IF EXISTS {fn_q}()");
        let c1 = self.make(
            &format!("contract_drop_dual_write_{table}_{from}_{to}"),
            c1_up,
            // Re-creating the dual-write is the reverse (best-effort); the
            // contract is gated + roll-forward-preferred, but a clean down exists.
            Some(build_dual_write_sql(&fn_q, &trg_q, &tbl_q, &from_q, &to_q)),
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
        // MUST be net-applied first — dropping <from> before every pre-existing
        // row's value is mirrored into <to> would lose un-backfilled data; and C1
        // (DROP TRIGGER + DROP FUNCTION) MUST run before C2 — the dual-write
        // trigger references <from>, so dropping the column while the trigger is
        // still live errors / leaves a dangling reference. In a contract-only
        // deploy both C1 and C2 are indegree-0 and would otherwise order only by
        // incidental UUIDv7 version; the explicit edge makes "drop the trigger
        // before the column it reads" a structural guarantee. The dual-write
        // trigger only covers rows written DURING the transition; the backfill
        // covers the rows that predate it. So the destructive drop is gated on the
        // backfill's journaled completion (the backfill step records
        // completion in the journal → the gate reads one timeline).
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
    /// `step_index` is one of the `EC_STEP_*` constants — together they
    /// DETERMINISTICALLY derive the sub-step's `version` via [`MigrationId::derive`]
    /// A re-lower of the identical rename reproduces the SAME id, which is
    /// what the cross-deploy obligation key + idempotent-skip + auto-discharge +
    /// self-EXPAND exemption all depend on. The version is NEVER
    /// `MigrationId::generate()` (random per call), which would re-key the
    /// obligation on every deploy.
    // The id-derivation pair (`id_seed`, `step_index`) pushes this to 8 args; they
    // are one logical unit (the deterministic sub-version derivation) and
    // bundling them into a struct would only relocate the same fields. Matches the
    // crate's `lower_ir_rename` / `sqlite_rename_rebuild` allow pattern.
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
        }
    }
}

/// Render the dual-write function + trigger SQL (shared by E2's `up` and C1's
/// `down`, so they are byte-identical).
///
/// `CREATE OR REPLACE FUNCTION … LANGUAGE plpgsql` (SECURITY INVOKER — the
/// plpgsql default; we deliberately emit NO `SECURITY DEFINER`). The body is
/// **total**: after it runs, `from` and `to` are ALWAYS equal, for every INSERT
/// and UPDATE — no input row is left divergent (a divergent pair would be
/// silently destroyed by the contract's `DROP COLUMN <from>`). Precedence is
/// **`to` wins** (consistent with the contract keeping `to`):
///
/// - on INSERT: if only `from` is set, mirror `from → to`; otherwise (`to` set,
///   both set, or both NULL) copy `to → from`;
/// - on UPDATE: if only `from` changed, mirror `from → to`; otherwise (`to`
///   changed, both changed → to wins, or neither changed → no-op) copy
///   `to → from`.
///
/// The only-`from` arm is `IS DISTINCT FROM`-guarded (NULL-safe). The else arm
/// is the total catch-all; when nothing changed it is a no-op self-copy, so an
/// UPDATE that touches neither column is not amplified.
pub(crate) fn dual_write_function_body(from_q: &str, to_q: &str) -> String {
    format!(
        "\nBEGIN\n\
         \x20   IF TG_OP = 'INSERT' THEN\n\
         \x20       IF NEW.{to_q} IS NULL AND NEW.{from_q} IS NOT NULL THEN\n\
         \x20           NEW.{to_q} := NEW.{from_q};   -- only from set\n\
         \x20       ELSE\n\
         \x20           NEW.{from_q} := NEW.{to_q};   -- to set / both set (to wins) / both null (no-op)\n\
         \x20       END IF;\n\
         \x20   ELSE\n\
         \x20       -- UPDATE: TOTAL, to wins. Only-from-changed mirrors from→to;\n\
         \x20       -- to-changed / both-changed / neither-changed all resolve to→from.\n\
         \x20       IF NEW.{from_q} IS DISTINCT FROM OLD.{from_q}\n\
         \x20          AND NEW.{to_q} IS NOT DISTINCT FROM OLD.{to_q} THEN\n\
         \x20           NEW.{to_q} := NEW.{from_q};   -- only from changed\n\
         \x20       ELSE\n\
         \x20           NEW.{from_q} := NEW.{to_q};   -- to changed / both changed (to wins) / neither (no-op)\n\
         \x20       END IF;\n\
         \x20   END IF;\n\
         \x20   RETURN NEW;\n\
         END;\n"
    )
}

fn build_dual_write_sql(fn_q: &str, trg_q: &str, tbl_q: &str, from_q: &str, to_q: &str) -> String {
    // The function body. `$zsdw$` dollar-quote so embedded SQL needs no escaping.
    // BEGIN … RETURN NEW: a BEFORE trigger mutates NEW in place (never re-issues
    // a write → no recursion).
    //
    // BOTH arms are TOTAL — they ALWAYS leave `from` == `to`, for every INSERT
    // and every UPDATE, no input row left divergent. This is the coexistence
    // model's central data-integrity invariant: a divergent pair would be
    // silently destroyed by the contract's `DROP COLUMN <from>`. When a single
    // statement changes BOTH columns (to different values), the old guarded form
    // matched NEITHER branch and let the pair diverge; the totalized form below
    // closes that hole.
    //
    // Precedence: **`to` (the new column) WINS** — consistent with the end state
    // (the contract keeps `to`). The `from`-only arm is the single exception: it
    // is the one case where `from` is the source of truth (the app wrote only the
    // legacy name), so `from → to`. Every other shape resolves to `to`.
    //
    // INSERT (OLD is undefined): if ONLY `from` is set, mirror `from → to`;
    // otherwise (`to` set, both set, or both NULL) the else arm copies
    // `to → from` (to-wins; both-NULL is a no-op self-copy).
    //
    // UPDATE: if ONLY `from` changed, mirror `from → to`; otherwise (`to`
    // changed, BOTH changed → to wins, or NEITHER changed → no-op self-copy) the
    // else arm copies `to → from`.
    let body = dual_write_function_body(from_q, to_q);
    let func = format!(
        "CREATE OR REPLACE FUNCTION {fn_q}() RETURNS trigger AS $zsdw${body}$zsdw$ LANGUAGE plpgsql"
    );
    // The trigger. BEFORE INSERT OR UPDATE, FOR EACH ROW. We attach a single
    // trigger for both events and keep no WHEN clause: Postgres forbids OLD in a
    // WHEN for the INSERT event, and the body is already total + self-no-op (a
    // no-op UPDATE falls into the to→from else arm, which writes the same value
    // back — no amplification). The function body is the authoritative guard.
    // (A separate UPDATE-only trigger with a WHEN (OLD.* IS DISTINCT FROM NEW.*)
    // is a possible v2 optimization; v1's body-level total form is correct and
    // simpler.)
    let trigger = format!(
        "CREATE TRIGGER {trg_q} BEFORE INSERT OR UPDATE ON {tbl_q}\n\
         FOR EACH ROW EXECUTE FUNCTION {fn_q}()"
    );
    format!("{func};\n{trigger}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author() -> ExpandContractAuthor {
        ExpandContractAuthor::new("proj_acme", "app_acme")
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
        // C2 depends on E1 (the column add it reverses), E3 (the backfill —
        // dropping <from> before the backfill mirrors pre-existing rows loses
        // data), AND C1 (the trigger drop MUST run before the column it reads —
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
        // identical sub-step ids + checksums: the E1..C2
        // versions are now DETERMINISTICALLY derived from the rename's stable seed
        // (schema+owner+table+from+to+ty) plus the step index, not minted fresh
        // per run. Because the checksum folds `depends_on` and `depends_on` now
        // holds deterministic sibling ids, the FULL checksum is stable too — the
        // pre-fix "dependency-free only" carve-out is gone. This is the property a
        // re-lower of the identical IR envelope on every deploy relies on.
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
        // the downstream guard ever sees it) — safe by construction.
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
        // The fn/trg names embedded in E2's up must be ≤63 bytes and appear in
        // both E2.up (CREATE) and E2.down (DROP) identically.
        let fn_name = dual_write_fn_name(&"t".repeat(40), &"f".repeat(20), &"g".repeat(20));
        let trg_name = dual_write_trg_name(&"t".repeat(40), &"f".repeat(20), &"g".repeat(20));
        assert!(
            fn_name.len() <= PG_MAX_IDENT_BYTES,
            "fn {} bytes",
            fn_name.len()
        );
        assert!(
            trg_name.len() <= PG_MAX_IDENT_BYTES,
            "trg {} bytes",
            trg_name.len()
        );
        assert!(e2.up.contains(&fn_name), "up must use capped fn name");
        assert!(e2.down.as_ref().unwrap().contains(&fn_name));
        assert!(e2.down.as_ref().unwrap().contains(&trg_name));
    }

    // REGRESSION — the executor-layer scope gate in `run_expand_pg` is now
    // UNCONDITIONAL. A direct seam caller with an EMPTY `expand` vec under
    // `ApprovalScope::Versions({})` MUST be refused with `ApprovalNotScoped` (keyed on
    // the E2 `trigger_version`, the resolved scope-version when the expand chain is
    // empty). Pre-fix the gate was `if let Some(v) = expand.first().or_else(|| expand.get(1))`
    // — an empty `expand` yielded `None`, SKIPPED the gate entirely, and fell through to
    // an `Ok(empty)` return: a fail-OPEN a malicious/buggy direct caller could ride.
    // This test FAILS RED pre-fix (the old code returned `Ok`, never the refusal).
    //
    // The refusal fires BEFORE any DDL/backfill, so the connection is never used on this
    // path — but `run_expand_pg` needs a `&Client`, so we open one (skip if :5440 is down).
}
