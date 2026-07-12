//! **PR10 Part B** — executor-side existence-guard catalog probe (§2.7).
//!
//! PR10 (Part A) shipped the existence-guard IR/JS SHAPE in `ir_version 2`
//! (`ExistenceGuard::{IfNotExists,IfExists}` on every DDL op) but DEFERRED guard
//! EXECUTION — every guarded op was REFUSED fail-closed at lower. This module is
//! the execution: a render-time-resolved, dialect-neutral [`GuardProbe`] is stamped
//! onto each lowered [`Migration`](crate::model::migration::Migration); at apply time the
//! executor reads the LIVE catalog under the ALREADY-HELD project advisory lock +
//! the open per-step transaction, and [`decide`] returns a [`GuardVerdict`]:
//!
//! - [`GuardVerdict::RunBare`] — the guard's precondition is met; run the op's `up`.
//! - [`GuardVerdict::SatisfiedNoop`] — the object already has the declared shape
//!   (`ifNotExists`) or is already absent (`ifExists`); SKIP the `up` but STILL
//!   journal the `completed` row so the version lands (a re-deploy sees it
//!   net-applied and skips it via normal pending computation).
//! - [`GuardVerdict::FailDrift`] — the object EXISTS with a shape that DIVERGES
//!   from the declared one (`ifNotExists`) and cannot be proven equal. This is a
//!   HARD error (`ApplyError::ExistenceGuardDrift`), never a silent skip. The
//!   transaction is rolled back; nothing is applied or journaled.
//!
//! # No-TOCTOU
//! The probe is a `&Client` read issued INSIDE the same open transaction that will
//! run the `up`, under the project advisory lock the whole plan already holds. No
//! lock is acquired or released across probe→decide→act, so there is no window for
//! the catalog to change between the verdict and the action.
//!
//! # Fail-closed shape verification (the point of this module)
//! A guard whose precondition CANNOT be fully proven from the catalog fails CLOSED
//! (`FailDrift`), never optimistically `SatisfiedNoop`. The non-obvious cases:
//!
//! - **Constraint `ifNotExists`** — a present same-name constraint is `FailDrift`,
//!   NOT `SatisfiedNoop`, unless its KIND clashes (also `FailDrift`, with a clearer
//!   `kind` field). The live `pg_get_constraintdef` definition cannot be
//!   byte-compared against the IR's un-normalized constraint body, so a present
//!   constraint's definitional equality cannot be PROVEN — and a same-name +
//!   same-kind constraint with a DIFFERENT predicate (a rewritten CHECK / a
//!   different FK target) is a real divergence the PG catalog DOES expose. We refuse
//!   rather than skip. (The realistic `ifNotExists` use — the constraint is ABSENT —
//!   still `RunBare`.)
//! - **Index `ifNotExists` over an expression / partial predicate** — `FailDrift`
//!   naming `expression`: the live index carries a non-empty `pg_get_expr`
//!   predicate the IR `createIndex` (a column-list AST) cannot render to a
//!   byte-comparable form, so equivalence cannot be proven.
//! - **SQLite affinity compare (F1)** — the engine's declared snapshot data_type is
//!   ALWAYS the PG `information_schema` spelling (`field_data_type` maps via the PG
//!   dialect, dialect-agnostically), but a REAL SQLite catalog reports the SQLite
//!   *affinity* (`text`/`integer`/`real`/`numeric`/`blob`). A raw spelling compare
//!   therefore false-drifts on EVERY non-text type on SQLite (a `timestamp with time
//!   zone` / `jsonb` / `uuid`→`text` snapshot vs a `text` live affinity,
//!   `double precision`→`real`, `bytea`→`blob`). The SQLite leg of [`decide`] folds
//!   BOTH the declared and the live data_type through
//!   [`crate::render::declarative::sqlite_canonical_type`] — the SAME affinity fold the
//!   declarative DIFFER uses — so a clean guarded `createTable`/`addColumn` re-run is
//!   idempotent for every type, while a genuine affinity change (string→number, i.e.
//!   `text` vs `real`) still maps to two distinct canonical tokens and IS a
//!   divergence. Several distinct SDK facets collapse to the `text` affinity on SQLite
//!   (`string`/`ref`/`actor`/`id` + `date`/`json` + a string `literal`) and the live
//!   catalog stores only the affinity, so a within-text-affinity facet change (live
//!   `string` vs declared `ref`/`date`) is INVISIBLE — but we do NOT fail closed on
//!   it: an affinity-match is a `SatisfiedNoop`, exactly as the DIFFER treats it (a
//!   documented SQLite divergence, see `docs/reference/sqlite-divergences.md`; on
//!   SQLite a `ref` column adds no FK via `ALTER` — it is physically a plain `text`
//!   column either way, so the blind spot carries no provable physical divergence).
//!   On PG both sides are the `information_schema` spelling and the raw compare is
//!   exact.
//!
//! The expected `data_type`/`nullable`/`unique`/`columns`/`kind` values are built
//! by the SAME shared snapshot builders the lowering arms call
//! (`build_table_snapshot` / `add_column_snapshot` / `create_index_snapshot` / the
//! addConstraint kind), so they are byte-comparable against the introspected
//! [`SchemaSnapshot`](crate::model::snapshot::SchemaSnapshot).

use crate::model::probe::{ExpectColumn, GuardDir, GuardProbe};
use crate::model::snapshot::SchemaSnapshot;
use zero_migrate_schema::query::{renderer as schema_renderer, SqlDialect};

// `GuardProbe::schema()` now lives on the type itself in `zero_migrate_ir::probe`
// (the type moved into the leaf wire-contract crate — an inherent `impl` here would
// be an orphan impl on a foreign type).

/// A single same-name object whose shape DIVERGES from the declared one — the
/// payload of [`GuardVerdict::FailDrift`]. Names + values only, never DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Divergence {
    /// The object the divergence is on, e.g. `table users`, `column users.email`,
    /// `index users_email_idx`, `constraint users_age_chk`.
    pub object: String,
    /// The attribute that diverged: `data_type`, `nullable`, `unique`, `columns`,
    /// `expression`, `kind`, or `definition`.
    pub field: String,
    /// The DECLARED (expected) value for `field`.
    pub expected: String,
    /// The LIVE value for `field`.
    pub actual: String,
}

/// The executor's decision for a guarded op, computed in Rust from the live
/// catalog snapshot — NEVER a SQL-level conditional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    /// Run the op's `up` bare (the guard's precondition is met).
    RunBare,
    /// Skip the `up` but journal the `completed` row (the declared shape is already
    /// present for `ifNotExists`, or the object is already absent for `ifExists`).
    SatisfiedNoop,
    /// The object exists with a divergent / unprovable shape — fail closed.
    FailDrift(Divergence),
}

/// Decide the verdict for `probe` against the LIVE catalog `live`. Pure (no DB
/// access — the caller has already taken the snapshot under the held lock + open
/// txn). See the module docs for the per-variant fail-closed rules.
///
/// `dialect` is the backend the LIVE snapshot was introspected from. It is the
/// caller-known fact (the PG executor passes [`SqlDialect::Postgres`]; the SQLite
/// backend passes [`SqlDialect::Sqlite`]), NOT carried on the wire-serialized probe.
///
/// **F1** — the declared probe `data_type` is ALWAYS the PG `information_schema`
/// spelling (the snapshot builder maps via the PG dialect, dialect-agnostically),
/// but a REAL SQLite catalog reports the SQLite affinity (`text`/`integer`/`real`/
/// `numeric`/`blob`). A raw `expect != live` compare therefore false-drifts on EVERY
/// non-text-affinity type on SQLite (a `timestamp with time zone` snapshot vs a
/// `text` live affinity, etc.). On the SQLite leg we therefore canonicalize BOTH
/// sides through [`crate::render::declarative::sqlite_canonical_type`] — the SAME affinity
/// fold the differ uses (`declarative.rs`) — so a guarded `createTable`/`addColumn`
/// re-run is idempotent for every type, while a real affinity change still diverges.
#[must_use]
pub fn decide(probe: &GuardProbe, live: &SchemaSnapshot, dialect: SqlDialect) -> GuardVerdict {
    match probe {
        GuardProbe::Table { table, direction, expect_columns, .. } => {
            decide_table(table, *direction, expect_columns, live, dialect)
        }
        GuardProbe::Column { table, column, direction, expect, .. } => {
            decide_column(table, column, *direction, expect.as_ref(), live, dialect)
        }
        GuardProbe::Index { table, name, direction, expect, .. } => {
            decide_index(table, name, *direction, expect.as_ref(), live)
        }
        GuardProbe::Constraint { table, name, direction, expect_kind, expect_definition, .. } => {
            decide_constraint(
                table,
                name,
                *direction,
                expect_kind.as_deref(),
                expect_definition.as_deref(),
                live,
            )
        }
        GuardProbe::View { name, direction, .. } => decide_view(name, *direction, live),
        GuardProbe::Sequence { name, direction, .. } => {
            decide_sequence(name, *direction, live)
        }
        GuardProbe::NamedType { name, kind, direction, .. } => {
            decide_named_type(name, kind, *direction, live)
        }
        GuardProbe::ColumnPresence { table, column, .. } => {
            // Always IfExists. Source column must EXIST → RunBare; absent → Noop.
            if column_present(live, table, column) {
                GuardVerdict::RunBare
            } else {
                GuardVerdict::SatisfiedNoop
            }
        }
    }
}

fn decide_named_type(
    name: &str,
    kind: &str,
    direction: GuardDir,
    live: &SchemaSnapshot,
) -> GuardVerdict {
    match live.named_types.get(name) {
        Some(actual) if actual.kind == kind => match direction {
            GuardDir::IfExists => GuardVerdict::RunBare,
            GuardDir::IfNotExists => GuardVerdict::SatisfiedNoop,
        },
        Some(actual) => GuardVerdict::FailDrift(Divergence {
            object: format!("type {name}"),
            field: "kind".to_string(),
            expected: kind.to_string(),
            actual: actual.kind.clone(),
        }),
        None => match direction {
            GuardDir::IfExists => GuardVerdict::SatisfiedNoop,
            GuardDir::IfNotExists => GuardVerdict::RunBare,
        },
    }
}

fn decide_view(name: &str, direction: GuardDir, live: &SchemaSnapshot) -> GuardVerdict {
    let present = live.views.contains_key(name);
    match direction {
        GuardDir::IfExists => {
            // dropView: presence-only.
            if present { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            if present { GuardVerdict::SatisfiedNoop } else { GuardVerdict::RunBare }
        }
    }
}

fn decide_sequence(name: &str, direction: GuardDir, live: &SchemaSnapshot) -> GuardVerdict {
    // Sequence guards intentionally answer only the existence question requested
    // by `ifExists` / `ifNotExists`. A guarded `createSequence` that finds an
    // existing sequence therefore no-ops here; option equality is the structural
    // drift layer's job (`SequenceSnapshot` carries the catalog options and
    // `diff_snapshots` reports any mismatch). Keeping the guard presence-only
    // avoids turning an existence probe into a partial schema validator.
    let present = live.sequences.contains_key(name);
    match direction {
        GuardDir::IfExists => {
            if present { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            if present { GuardVerdict::SatisfiedNoop } else { GuardVerdict::RunBare }
        }
    }
}

fn decide_table(
    table: &str,
    direction: GuardDir,
    expect_columns: &[ExpectColumn],
    live: &SchemaSnapshot,
    dialect: SqlDialect,
) -> GuardVerdict {
    let present = live.tables.contains_key(table);
    match direction {
        GuardDir::IfExists => {
            // dropTable: presence-only.
            if present { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            let Some(t) = live.tables.get(table) else {
                return GuardVerdict::RunBare; // absent → create it.
            };
            // Present: prove EXACT shape equality of the declared columns. A missing
            // declared column, a `(data_type, nullable)` divergence, OR any EXTRA
            // live column not in the declared set → FailDrift (a wider live table is
            // not the declared shape — fail closed).
            for ec in expect_columns {
                match t.columns.iter().find(|c| c.name == ec.name) {
                    None => {
                        return drift(
                            &format!("table {table}"),
                            "data_type",
                            &format!("{}: {}", ec.name, ec.data_type),
                            &format!("{}: <absent>", ec.name),
                        );
                    }
                    Some(live_col) => {
                        if let Some(v) = column_shape_divergence(&ExpectColumnShape {
                            table,
                            column: &ec.name,
                            expect_dtype: &ec.data_type,
                            expect_nullable: ec.nullable,
                            live_dtype: &live_col.data_type,
                            live_nullable: live_col.nullable,
                            dialect,
                        }) {
                            return v;
                        }
                    }
                }
            }
            // Extra live column → FailDrift (the live table is wider than declared).
            for live_col in &t.columns {
                if !expect_columns.iter().any(|ec| ec.name == live_col.name) {
                    return drift(
                        &format!("table {table}"),
                        "columns",
                        "<declared column set>",
                        &format!("extra live column {}", live_col.name),
                    );
                }
            }
            GuardVerdict::SatisfiedNoop
        }
    }
}

fn decide_column(
    table: &str,
    column: &str,
    direction: GuardDir,
    expect: Option<&(String, bool)>,
    live: &SchemaSnapshot,
    dialect: SqlDialect,
) -> GuardVerdict {
    let present = column_present(live, table, column);
    match direction {
        GuardDir::IfExists => {
            // dropColumn: presence-only.
            if present { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            if !present {
                return GuardVerdict::RunBare; // absent → add it.
            }
            // Present: verify (data_type, nullable). `expect` is always Some on the
            // addColumn ifNotExists path; if it is somehow None (a presence-only
            // ifNotExists we never build), fail closed — we cannot prove the shape.
            let Some((dtype, nullable)) = expect else {
                return drift(
                    &format!("column {table}.{column}"),
                    "data_type",
                    "<declared but unverifiable>",
                    "<present>",
                );
            };
            let live_col = live
                .tables
                .get(table)
                .and_then(|t| t.columns.iter().find(|c| c.name == column));
            let Some(live_col) = live_col else {
                // column_present said present but the column vanished — fail closed.
                return drift(
                    &format!("column {table}.{column}"),
                    "data_type",
                    dtype,
                    "<absent>",
                );
            };
            column_shape_divergence(&ExpectColumnShape {
                table,
                column,
                expect_dtype: dtype,
                expect_nullable: *nullable,
                live_dtype: &live_col.data_type,
                live_nullable: live_col.nullable,
                dialect,
            })
            .unwrap_or(GuardVerdict::SatisfiedNoop)
        }
    }
}

/// The declared-vs-live column shape compare inputs, bundled so the comparison
/// seam takes one argument instead of eight positional scalars (the two callers —
/// `decide_table` per declared column, `decide_column` for the stand-alone
/// addColumn — build it inline). `expect_*` is the declared shape; `live_*` is the
/// introspected catalog shape; `dialect` selects the compare (raw on PG, canonical
/// affinity-fold on SQLite).
struct ExpectColumnShape<'a> {
    table: &'a str,
    column: &'a str,
    expect_dtype: &'a str,
    expect_nullable: bool,
    live_dtype: &'a str,
    live_nullable: bool,
    dialect: SqlDialect,
}

/// Compare a declared column shape against the live one. Returns a `FailDrift`
/// verdict on a divergence, or `None` if they match exactly.
///
/// **F1 — dialect-aware data_type compare.** The declared `expect_dtype` is ALWAYS
/// the PG `information_schema` spelling (the snapshot builder maps via the PG
/// dialect regardless of backend — `declarative::field_data_type`). A REAL SQLite
/// catalog reports the SQLite *affinity* (`text`/`integer`/`real`/`numeric`/`blob`),
/// so a raw `expect_dtype != live_dtype` compare false-drifts on EVERY non-text
/// type on SQLite (a `timestamp with time zone` / `jsonb` / `uuid`→`text` snapshot
/// vs a `text` live affinity, `double precision`→`real`, `bytea`→`blob`, …). On the
/// SQLite leg we therefore fold BOTH sides through
/// [`crate::render::declarative::sqlite_canonical_type`] — the SAME affinity fold the
/// declarative differ uses — so a clean guarded re-run is idempotent for every
/// type, while a real affinity change (`text` vs `real`, i.e. string→number) still
/// maps to two DIFFERENT canonical tokens and IS a divergence. On PG both sides are
/// already the `information_schema` spelling and the raw compare is exact.
///
/// **F1 — SQLite verifies the canonical AFFINITY, consistent with the differ.**
/// After the fold, several distinct SDK facets collapse to the `text` affinity on
/// SQLite (`string`/`ref`/`actor`/`id` + `date`/`json` + a string `literal`), and the
/// live catalog stores only the affinity — the un-collapsed SDK facet is NOT
/// recoverable. We do NOT fail closed on that blind spot: an affinity-match is a
/// `SatisfiedNoop`, exactly as the declarative DIFFER treats it (it compares only
/// `sqlite_canonical_type` on SQLite — a within-affinity facet change is a documented
/// SQLite divergence, see `docs/reference/sqlite-divergences.md`). This is what makes
/// a guarded `createTable`/`addColumn ifNotExists` RE-RUN idempotent on SQLite (every
/// table carries text-affinity system columns; a stand-alone `addColumn` of a `ref`
/// over a live `string` is physically a no-op anyway — SQLite cannot add an FK via
/// `ALTER`, so both are plain `text` columns). A GENUINE affinity change
/// (string→number, `text` vs `real`) still maps to two distinct canonical tokens and
/// IS a divergence. On PG both sides are the `information_schema` spelling and the raw
/// compare is exact.
fn column_shape_divergence(shape: &ExpectColumnShape<'_>) -> Option<GuardVerdict> {
    let ExpectColumnShape {
        table,
        column,
        expect_dtype,
        expect_nullable,
        live_dtype,
        live_nullable,
        dialect,
    } = *shape;
    // **F1** — on SQLite, compare the canonical AFFINITY (PG-spelled snapshot folded
    // to the SQLite affinity the emitter would have written, AND the already-SQLite
    // live token folded to the same canonical form). On PG, compare the raw
    // `information_schema` spellings unchanged.
    let renderer = schema_renderer(dialect);
    let dtypes_match = renderer.canonical_type(expect_dtype) == renderer.canonical_type(live_dtype);
    if !dtypes_match {
        return Some(drift(
            &format!("column {table}.{column}"),
            "data_type",
            expect_dtype,
            live_dtype,
        ));
    }
    if expect_nullable != live_nullable {
        return Some(drift(
            &format!("column {table}.{column}"),
            "nullable",
            &expect_nullable.to_string(),
            &live_nullable.to_string(),
        ));
    }
    None
}

fn decide_index(
    table: &str,
    name: &str,
    direction: GuardDir,
    expect: Option<&(bool, Vec<String>)>,
    live: &SchemaSnapshot,
) -> GuardVerdict {
    // Look up the index under the table hint first; for the presence-only
    // `ifExists` (dropIndex) path the table hint may be absent/empty, so fall back
    // to scanning ALL tables for the index name (indexes are unique per schema).
    let live_idx = live
        .tables
        .get(table)
        .and_then(|t| t.indexes.iter().find(|i| i.name == name))
        .or_else(|| live.tables.values().flat_map(|t| &t.indexes).find(|i| i.name == name));
    match direction {
        GuardDir::IfExists => {
            // dropIndex: presence-only on the index name.
            if live_idx.is_some() { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            let Some(live_idx) = live_idx else {
                return GuardVerdict::RunBare; // absent → create it.
            };
            // Present: prove (unique, columns) equality. An expression / partial
            // index fails closed: this probe contract carries only a plain
            // column-list expectation, so expression/predicate equivalence cannot
            // be proven here.
            if live_idx.predicate.is_some()
                || live_idx
                    .elements
                    .iter()
                    .any(|element| matches!(element, crate::model::snapshot::IndexElementSnapshot::Expr(_)))
            {
                return drift(
                    &format!("index {name}"),
                    "elements",
                    "<plain column-list index>",
                    "<expression/partial index — cannot prove equivalence>",
                );
            }
            let Some((unique, columns)) = expect else {
                return drift(
                    &format!("index {name}"),
                    "unique",
                    "<declared but unverifiable>",
                    "<present>",
                );
            };
            if *unique != live_idx.unique {
                return drift(
                    &format!("index {name}"),
                    "unique",
                    &unique.to_string(),
                    &live_idx.unique.to_string(),
                );
            }
            if columns != &live_idx.columns {
                return drift(
                    &format!("index {name}"),
                    "columns",
                    &columns.join(","),
                    &live_idx.columns.join(","),
                );
            }
            GuardVerdict::SatisfiedNoop
        }
    }
}

fn decide_constraint(
    table: &str,
    name: &str,
    direction: GuardDir,
    expect_kind: Option<&str>,
    expect_definition: Option<&str>,
    live: &SchemaSnapshot,
) -> GuardVerdict {
    let live_con = live
        .tables
        .get(table)
        .and_then(|t| t.constraints.iter().find(|c| c.name == name));
    match direction {
        GuardDir::IfExists => {
            // dropConstraint: presence-only on the constraint name.
            if live_con.is_some() { GuardVerdict::RunBare } else { GuardVerdict::SatisfiedNoop }
        }
        GuardDir::IfNotExists => {
            let Some(live_con) = live_con else {
                return GuardVerdict::RunBare; // absent → add it.
            };
            // Present. A kind clash is the clearest divergence.
            if let Some(kind) = expect_kind {
                if kind != live_con.kind {
                    return drift(
                        &format!("constraint {name}"),
                        "kind",
                        kind,
                        &live_con.kind,
                    );
                }
            }
            // **F2 — structural compare when a byte-comparable definition is
            // available.** The `createTable` deferred-FK unit stamps the declared FK
            // body in the EXACT `pg_get_constraintdef` spelling
            // (`declarative::fk_definition_pg`), so a present same-name + same-kind FK
            // CAN be proven equal: a byte-equal live definition is an idempotent
            // `SatisfiedNoop` (a re-run of the guarded `createTable ifNotExists`
            // succeeds instead of hard-FailDrift), and a divergent one (a re-pointed
            // FK target / changed column / ON-DELETE) is a real divergence we still
            // refuse, naming `definition`. The compare normalizes the referenced-table
            // SCHEMA QUALIFIER out of both sides ([`normalize_fk_definition`]): the
            // declared side is always `REFERENCES <schema>.<table>` but
            // `pg_get_constraintdef` OMITS the schema when the referenced table is in
            // the live `search_path` (it is — same project schema, cross-app FKs are
            // rejected at author time), so the qualifier is search_path-sensitive noise.
            // Every MATERIAL divergence (target table, columns, ON DELETE/UPDATE,
            // DEFERRABLE) survives the normalization and is still caught.
            if let Some(decl_def) = expect_definition {
                if normalize_fk_definition(decl_def) == normalize_fk_definition(&live_con.definition) {
                    return GuardVerdict::SatisfiedNoop;
                }
                return drift(
                    &format!("constraint {name}"),
                    "definition",
                    decl_def,
                    if live_con.definition.is_empty() { "<present>" } else { &live_con.definition },
                );
            }
            // **MED finding (fail-closed over a catalog-exposed divergence).** With NO
            // byte-comparable declared definition (the stand-alone `addConstraint
            // ifNotExists` path), a same-name + same-kind constraint is NOT a
            // SatisfiedNoop: the live `pg_get_constraintdef` definition cannot be
            // byte-compared against the IR's un-normalized constraint body, so we
            // cannot PROVE the predicate (a rewritten CHECK, a different FK target /
            // column / ON-DELETE) is equal. We refuse rather than skip. The realistic
            // `ifNotExists` use (the constraint is ABSENT) still RunBare above.
            drift(
                &format!("constraint {name}"),
                "definition",
                "<declared constraint — cannot prove equal to live pg_get_constraintdef>",
                if live_con.definition.is_empty() { "<present>" } else { &live_con.definition },
            )
        }
    }
}

/// Normalize a FOREIGN KEY `pg_get_constraintdef` body for a search_path-invariant
/// structural compare (F2): strip the referenced-table SCHEMA QUALIFIER that appears
/// right after `REFERENCES `. The declared side (`fk_definition_pg`) ALWAYS qualifies
/// the target (`REFERENCES "schema".people(id)` or `REFERENCES schema.people(id)`),
/// but `pg_get_constraintdef` OMITS the schema when the referenced table is in the
/// live `search_path` (`REFERENCES people(id)`). The qualifier is the ONLY
/// search_path-sensitive token; collapsing it makes the byte-compare stable while
/// every material divergence (the referenced TABLE name, the column lists, the ON
/// DELETE/UPDATE actions, the DEFERRABLE clause) is preserved.
///
/// A cross-schema FK is rejected at author time (`reject_cross_app_ref`), so the
/// referenced table is always in the project schema — there is no case where dropping
/// the schema would conflate two different targets.
fn normalize_fk_definition(def: &str) -> String {
    const NEEDLE: &str = "REFERENCES ";
    let Some(pos) = def.find(NEEDLE) else {
        return def.to_string();
    };
    let after = pos + NEEDLE.len();
    let rest = &def[after..];
    // The referenced object reference runs up to the opening `(` of its column list.
    let Some(paren_rel) = rest.find('(') else {
        return def.to_string();
    };
    let obj = &rest[..paren_rel]; // e.g. `"schema".people` / `schema.people` / `people`
    // Keep only the FINAL dotted segment (the table), dropping any `<schema>.` prefix.
    // Handles quoted identifiers by splitting on the last `.`.
    //
    // **SAFE post-validation** — `rsplit('.')` would mis-split a referenced table
    // whose own (quoted) identifier contained a literal dot (e.g. `"a.b"`). That
    // case is UNREACHABLE here: the declared side is built by
    // [`crate::render::declarative::fk_definition_pg`] from a `target` that has already
    // passed `validate_ident` (rejects `.` in identifiers) and `reject_cross_app_ref`
    // (rejects dotted FK targets, `declarative.rs`), so the referenced table is
    // ALWAYS a single dot-free segment. The live side comes from
    // `pg_get_constraintdef`, which double-quotes such a name — but the catalog only
    // ever holds names this same author path created, so it is dot-free too. The
    // debug_assert pins that invariant; if identifier rules ever loosen this must
    // become a quote-aware split.
    let table_seg = obj.rsplit('.').next().unwrap_or(obj).trim();
    debug_assert!(
        !table_seg.trim_matches('"').contains('.'),
        "FK referenced table segment must be dot-free post-validation (validate_ident / \
         reject_cross_app_ref); got {table_seg:?} from {def:?}"
    );
    let mut out = String::with_capacity(def.len());
    out.push_str(&def[..after]);
    out.push_str(table_seg);
    out.push_str(&rest[paren_rel..]);
    out
}

/// Whether `table.column` exists in the live snapshot.
fn column_present(live: &SchemaSnapshot, table: &str, column: &str) -> bool {
    live.tables
        .get(table)
        .is_some_and(|t| t.columns.iter().any(|c| c.name == column))
}

/// Build a `FailDrift` verdict.
fn drift(object: &str, field: &str, expected: &str, actual: &str) -> GuardVerdict {
    GuardVerdict::FailDrift(Divergence {
        object: object.to_string(),
        field: field.to_string(),
        expected: expected.to_string(),
        actual: actual.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::snapshot::{
        ColumnSnapshot, ConstraintSnapshot, IndexElementSnapshot, IndexSnapshot, TableSnapshot,
    };
    use std::collections::BTreeMap;

    /// `decide` on the PG leg (raw `information_schema` compare).
    fn decide_pg(probe: &GuardProbe, live: &SchemaSnapshot) -> GuardVerdict {
        decide(probe, live, SqlDialect::Postgres)
    }

    /// `decide` on the SQLite leg (affinity-fold compare — F1).
    fn decide_sqlite(probe: &GuardProbe, live: &SchemaSnapshot) -> GuardVerdict {
        decide(probe, live, SqlDialect::Sqlite)
    }

    fn col(name: &str, dtype: &str, nullable: bool) -> ColumnSnapshot {
        ColumnSnapshot {
            name: name.to_string(),
            data_type: dtype.to_string(),
            nullable,
            default: None,
            ddl_type_override: None,
            inline_checks: Vec::new(),
            generated: None,
            identity: None,
            case_sensitive: None,
            encryption_sentinel: None,
            comment_sentinel: None,
            comment: None,
        }
    }

    fn ec(name: &str, dtype: &str, nullable: bool) -> ExpectColumn {
        ExpectColumn { name: name.to_string(), data_type: dtype.to_string(), nullable }
    }

    fn snapshot_with(table: &str, t: TableSnapshot) -> SchemaSnapshot {
        let mut tables = BTreeMap::new();
        tables.insert(table.to_string(), t);
        SchemaSnapshot { tables, ..Default::default() }
    }

    fn empty_table() -> TableSnapshot {
        TableSnapshot {
            columns: Vec::new(),
            indexes: Vec::new(),
            constraints: Vec::new(),
            runtime_options: Default::default(),
            partition_by: None,
            comment: None,
            stored_create_sql: None,
        }
    }

    // -- Column ifNotExists ------------------------------------------------

    #[test]
    fn column_ifnotexists_absent_runs_bare() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
        };
        let live = SchemaSnapshot::default();
        assert_eq!(decide_pg(&probe, &live), GuardVerdict::RunBare);
    }

    #[test]
    fn column_ifnotexists_present_matching_is_noop() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
        };
        let mut t = empty_table();
        t.columns.push(col("email", "text", true));
        let live = snapshot_with("users", t);
        assert_eq!(decide_pg(&probe, &live), GuardVerdict::SatisfiedNoop);
    }

    #[test]
    fn column_ifnotexists_present_divergent_type_fails() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
        };
        let mut t = empty_table();
        t.columns.push(col("email", "integer", true));
        let live = snapshot_with("users", t);
        match decide_pg(&probe, &live) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "data_type"),
            v => panic!("expected FailDrift(data_type), got {v:?}"),
        }
    }

    #[test]
    fn column_ifnotexists_present_divergent_nullability_fails() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
        };
        let mut t = empty_table();
        t.columns.push(col("email", "text", false));
        let live = snapshot_with("users", t);
        match decide_pg(&probe, &live) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "nullable"),
            v => panic!("expected FailDrift(nullable), got {v:?}"),
        }
    }

    // -- F1: SQLite within-text-affinity facet change = noop (differ-consistent) --

    #[test]
    fn column_ifnotexists_sqlite_within_text_affinity_facet_change_is_noop() {
        // **F1** — on SQLite the live column reads back as the `text` affinity; the
        // guard declares a column whose snapshot is `timestamp with time zone` (a
        // `date` facet) which folds to the same `text` affinity. The within-text-
        // affinity facet blind spot is a documented SQLite divergence the DIFFER also
        // accepts (it compares only `sqlite_canonical_type`). The guard matches: an
        // affinity-match is a SatisfiedNoop (idempotent re-run), NOT a fail-closed and
        // NOT a false `timestamp with time zone != text` drift. (On SQLite a `ref`/date
        // column is physically a plain `text` column anyway — no provable divergence.)
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "happened".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("timestamp with time zone".into(), true)),
        };
        let mut t = empty_table();
        // live column → introspected affinity `text`.
        t.columns.push(col("happened", "text", true));
        assert_eq!(
            decide_sqlite(&probe, &snapshot_with("users", t)),
            GuardVerdict::SatisfiedNoop,
            "a within-text-affinity facet on SQLite is an idempotent no-op (differ-consistent)"
        );
    }

    #[test]
    fn column_ifnotexists_no_facet_text_match_is_noop() {
        // A plain `text` == `text` match is a legitimate SatisfiedNoop.
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "email".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
        };
        let mut t = empty_table();
        t.columns.push(col("email", "text", true));
        assert_eq!(decide_pg(&probe, &snapshot_with("users", t)), GuardVerdict::SatisfiedNoop);
    }

    #[test]
    fn table_ifnotexists_sqlite_text_affinity_reruns_idempotent_noop() {
        // **F1 — the createTable Table leg is presence + affinity only.** A guarded
        // createTable re-run over a table THIS engine made: the declared `timestamp
        // with time zone` (date) / `text` columns fold to the SQLite `text` affinity
        // and MATCH the live `text` affinity → SatisfiedNoop (idempotent re-run), NOT
        // a false `timestamp with time zone != text` drift and NOT an H1 fail-closed.
        let probe = GuardProbe::Table {
            schema: "app".into(),
            table: "events".into(),
            direction: GuardDir::IfNotExists,
            expect_columns: vec![
                // a timestamp snapshot column (PG spelling) over a live SQLite `text`.
                ec("at", "timestamp with time zone", true),
                // a string column (snapshot `text`) over a live `text`.
                ec("name", "text", true),
            ],
        };
        let mut t = empty_table();
        t.columns.push(col("at", "text", true));
        t.columns.push(col("name", "text", true));
        assert_eq!(
            decide_sqlite(&probe, &snapshot_with("events", t)),
            GuardVerdict::SatisfiedNoop,
            "a SQLite createTable re-run over text-affinity columns must be idempotent"
        );
    }

    // -- Column ifExists (dropColumn) --------------------------------------

    #[test]
    fn column_ifexists_present_runs_absent_noops() {
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "users".into(),
            column: "legacy".into(),
            direction: GuardDir::IfExists,
            expect: None,
        };
        let mut t = empty_table();
        t.columns.push(col("legacy", "text", true));
        assert_eq!(decide_pg(&probe, &snapshot_with("users", t)), GuardVerdict::RunBare);
        assert_eq!(decide_pg(&probe, &SchemaSnapshot::default()), GuardVerdict::SatisfiedNoop);
    }

    // -- Table ifNotExists -------------------------------------------------

    #[test]
    fn table_ifnotexists_present_extra_live_column_fails() {
        let probe = GuardProbe::Table {
            schema: "app".into(),
            table: "users".into(),
            direction: GuardDir::IfNotExists,
            expect_columns: vec![ec("id", "integer", false)],
        };
        let mut t = empty_table();
        t.columns.push(col("id", "integer", false));
        t.columns.push(col("sneaky", "text", true)); // extra live column
        match decide_pg(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "columns"),
            v => panic!("expected FailDrift(columns) for extra live column, got {v:?}"),
        }
    }

    #[test]
    fn table_ifnotexists_present_matching_is_noop() {
        let probe = GuardProbe::Table {
            schema: "app".into(),
            table: "users".into(),
            direction: GuardDir::IfNotExists,
            expect_columns: vec![ec("id", "integer", false)],
        };
        let mut t = empty_table();
        t.columns.push(col("id", "integer", false));
        assert_eq!(decide_pg(&probe, &snapshot_with("users", t)), GuardVerdict::SatisfiedNoop);
    }

    // -- Index ifNotExists -------------------------------------------------

    #[test]
    fn index_ifnotexists_present_unique_flip_fails() {
        let probe = GuardProbe::Index {
            schema: "app".into(),
            table: "users".into(),
            name: "users_email_idx".into(),
            direction: GuardDir::IfNotExists,
            expect: Some((true, vec!["email".into()])),
        };
        let mut t = empty_table();
        t.indexes
            .push(IndexSnapshot::btree("users_email_idx".to_string(), false, vec!["email".to_string()]));
        match decide_pg(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "unique"),
            v => panic!("expected FailDrift(unique), got {v:?}"),
        }
    }

    #[test]
    fn index_ifnotexists_present_expression_index_fails_closed() {
        let probe = GuardProbe::Index {
            schema: "app".into(),
            table: "users".into(),
            name: "users_lower_idx".into(),
            direction: GuardDir::IfNotExists,
            expect: Some((false, vec!["email".into()])),
        };
        let mut t = empty_table();
        let mut idx = IndexSnapshot::btree("users_lower_idx".to_string(), false, Vec::new());
        idx.elements = vec![IndexElementSnapshot::expr("lower(email)")];
        t.indexes.push(idx);
        match decide_pg(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "elements"),
            v => panic!("expected FailDrift(elements) for partial/expression index, got {v:?}"),
        }
    }

    // -- Constraint ifNotExists (MED finding) ------------------------------

    fn constraint(name: &str, kind: &str, definition: &str) -> ConstraintSnapshot {
        ConstraintSnapshot {
            name: name.to_string(),
            kind: kind.to_string(),
            definition: definition.to_string(),
            comment: None,
        }
    }

    #[test]
    fn constraint_ifnotexists_absent_runs_bare() {
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "users".into(),
            name: "users_email_key".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("UNIQUE".into()),
            expect_definition: None,
        };
        assert_eq!(decide_pg(&probe, &SchemaSnapshot::default()), GuardVerdict::RunBare);
    }

    #[test]
    fn constraint_ifnotexists_present_different_kind_fails_kind() {
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "users".into(),
            name: "users_pk".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("UNIQUE".into()),
            expect_definition: None,
        };
        let mut t = empty_table();
        t.constraints
            .push(constraint("users_pk", "PRIMARY KEY", "PRIMARY KEY (id)"));
        match decide_pg(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "kind"),
            v => panic!("expected FailDrift(kind), got {v:?}"),
        }
    }

    #[test]
    fn constraint_ifnotexists_present_same_kind_fails_definition_not_noop() {
        // MED finding: a same-name + same-kind constraint must NOT be SatisfiedNoop
        // — the live pg_get_constraintdef body cannot be proven equal to the IR's
        // un-normalized constraint, so a possibly-divergent CHECK/FK is refused.
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "users".into(),
            name: "users_age_chk".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("CHECK".into()),
            expect_definition: None,
        };
        let mut t = empty_table();
        t.constraints
            .push(constraint("users_age_chk", "CHECK", "CHECK ((age > 18))"));
        match decide_pg(&probe, &snapshot_with("users", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "definition"),
            v => panic!("expected FailDrift(definition) on same-name+same-kind, got {v:?}"),
        }
    }

    // -- F1: SQLite affinity-fold data_type compare ------------------------

    #[test]
    fn sqlite_timestamp_snapshot_vs_text_live_is_noop_not_false_drift() {
        // **F1 root-cause unit** — the PG-spelled snapshot data_type for a timestamp
        // column is `timestamp with time zone`, but a REAL SQLite catalog reports the
        // `text` affinity. The OLD raw `expect != live` compare false-FailDrifted on
        // EVERY re-run (every table has created_at/updated_at/deleted_at timestamps).
        // After the affinity fold both sides canonicalize to `text` → SatisfiedNoop.
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "events".into(),
            column: "created_at".into(),
            direction: GuardDir::IfNotExists,
            // snapshot spelling (PG) vs the live SQLite `text` affinity.
            expect: Some(("timestamp with time zone".into(), false)),
            // a timestamp is NOT a TEXT-affinity SDK-facet blind spot from the
            // addColumn leg's perspective only when no facet flag is set; here we
            // assert the fold alone removes the false drift (facet None).
        };
        let mut t = empty_table();
        t.columns.push(col("created_at", "text", false));
        assert_eq!(
            decide_sqlite(&probe, &snapshot_with("events", t)),
            GuardVerdict::SatisfiedNoop,
            "a timestamp-snapshot vs text-affinity live must NOT false-drift on SQLite"
        );
    }

    #[test]
    fn sqlite_jsonb_snapshot_vs_text_live_is_noop() {
        // **F1** — a `json` column: snapshot `jsonb`, live SQLite `text` affinity. The
        // affinity fold makes them MATCH (both `text`) → SatisfiedNoop. NOT a false
        // `jsonb != text` drift; the within-text-affinity facet blind spot is accepted
        // (differ-consistent), so a guarded re-run over a json column is idempotent.
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "docs".into(),
            column: "body".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("jsonb".into(), true)),
        };
        let mut t = empty_table();
        t.columns.push(col("body", "text", true));
        assert_eq!(
            decide_sqlite(&probe, &snapshot_with("docs", t)),
            GuardVerdict::SatisfiedNoop,
            "a jsonb-snapshot vs text-affinity live must fold-match to a no-op on SQLite"
        );
    }

    #[test]
    fn sqlite_real_snapshot_matches_real_live_is_noop() {
        // A non-text affinity (`number` → snapshot `double precision`, live `real`):
        // unambiguous, no facet flag. The fold maps both to `real` → SatisfiedNoop.
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "m".into(),
            column: "amount".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("double precision".into(), true)),
        };
        let mut t = empty_table();
        t.columns.push(col("amount", "real", true));
        assert_eq!(
            decide_sqlite(&probe, &snapshot_with("m", t)),
            GuardVerdict::SatisfiedNoop
        );
    }

    #[test]
    fn sqlite_real_text_genuine_change_still_diverges() {
        // A REAL affinity change (string→number): snapshot `text` vs live `real` fold
        // to DIFFERENT canonical tokens → FailDrift even on SQLite.
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "m".into(),
            column: "x".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("text".into(), true)),
        };
        let mut t = empty_table();
        t.columns.push(col("x", "real", true));
        match decide_sqlite(&probe, &snapshot_with("m", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "data_type"),
            v => panic!("expected FailDrift(data_type) on a genuine text→real change, got {v:?}"),
        }
    }

    #[test]
    fn pg_leg_does_not_fold_affinities() {
        // The PG leg compares raw `information_schema` spellings. A `jsonb` declared
        // over a `text` live column is a real PG divergence (NOT folded to match).
        let probe = GuardProbe::Column {
            schema: "app".into(),
            table: "docs".into(),
            column: "body".into(),
            direction: GuardDir::IfNotExists,
            expect: Some(("jsonb".into(), true)),
        };
        let mut t = empty_table();
        t.columns.push(col("body", "text", true));
        match decide_pg(&probe, &snapshot_with("docs", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "data_type"),
            v => panic!("PG leg must NOT fold jsonb→text, got {v:?}"),
        }
    }

    // -- F2: createTable deferred-FK structural compare --------------------

    #[test]
    fn constraint_ifnotexists_with_definition_schema_qualifier_normalized_is_noop() {
        // **F2** — the createTable deferred-FK probe carries the declared
        // `pg_get_constraintdef` body, which is ALWAYS schema-qualified
        // (`REFERENCES app.people(id)`), but the LIVE `pg_get_constraintdef` OMITS the
        // schema when the referenced table is in the search_path (`REFERENCES
        // people(id)`). After normalizing the qualifier out of both sides they MATCH →
        // SatisfiedNoop (the real-world re-deploy case — a hard FailDrift here was the
        // F2 bug).
        let declared = "FOREIGN KEY (owner) REFERENCES app.people(id) DEFERRABLE INITIALLY DEFERRED";
        let live = "FOREIGN KEY (owner) REFERENCES people(id) DEFERRABLE INITIALLY DEFERRED";
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "pets".into(),
            name: "pets_owner_fkey".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("FOREIGN KEY".into()),
            expect_definition: Some(declared.into()),
        };
        let mut t = empty_table();
        t.constraints.push(constraint("pets_owner_fkey", "FOREIGN KEY", live));
        assert_eq!(
            decide_pg(&probe, &snapshot_with("pets", t)),
            GuardVerdict::SatisfiedNoop,
            "schema-qualifier-only difference must normalize to an idempotent no-op"
        );
    }

    #[test]
    fn normalize_fk_definition_strips_referenced_schema_qualifier() {
        // Quoted, bare, and already-unqualified targets all normalize to the same form.
        let a = normalize_fk_definition(
            "FOREIGN KEY (owner) REFERENCES \"app\".people(id) ON DELETE RESTRICT",
        );
        let b = normalize_fk_definition(
            "FOREIGN KEY (owner) REFERENCES app.people(id) ON DELETE RESTRICT",
        );
        let c = normalize_fk_definition(
            "FOREIGN KEY (owner) REFERENCES people(id) ON DELETE RESTRICT",
        );
        assert_eq!(a, c);
        assert_eq!(b, c);
        assert!(c.contains("REFERENCES people(id)"), "table + columns preserved: {c}");
        // A re-pointed target survives normalization → still DIFFERENT.
        let d = normalize_fk_definition(
            "FOREIGN KEY (owner) REFERENCES app.companies(id) ON DELETE RESTRICT",
        );
        assert_ne!(c, d, "a different referenced TABLE must not be normalized away");
    }

    #[test]
    fn normalize_fk_definition_quoted_dotted_schema_keeps_dotfree_table() {
        // **LOW (latent-invariant pin)** — the rsplit('.') table extraction is safe
        // ONLY because the referenced TABLE segment is dot-free post-validation
        // (`validate_ident` rejects dots; `reject_cross_app_ref` rejects dotted FK
        // targets). The qualifier ahead of it may itself be quoted, but the FINAL
        // segment is the dot-free table. A quoted reserved-word SCHEMA qualifier must
        // still strip cleanly to the dot-free table, MATCHING the bare-schema and
        // already-unqualified forms (the debug_assert must NOT fire on this path).
        let quoted_schema =
            normalize_fk_definition("FOREIGN KEY (owner) REFERENCES \"order\".people(id)");
        let bare = normalize_fk_definition("FOREIGN KEY (owner) REFERENCES people(id)");
        assert_eq!(
            quoted_schema, bare,
            "a quoted schema qualifier over a dot-free table normalizes to the bare form"
        );
        assert!(
            quoted_schema.contains("REFERENCES people(id)"),
            "the dot-free table survives: {quoted_schema}"
        );
    }

    #[test]
    fn constraint_ifnotexists_with_definition_divergent_fails_closed() {
        // A re-pointed FK (different live definition) still fails CLOSED naming
        // `definition` even with the structural-compare path.
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "pets".into(),
            name: "pets_owner_fkey".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("FOREIGN KEY".into()),
            expect_definition: Some(
                "FOREIGN KEY (owner) REFERENCES app.people(id) DEFERRABLE INITIALLY DEFERRED".into(),
            ),
        };
        let mut t = empty_table();
        // live FK points at a DIFFERENT table.
        t.constraints.push(constraint(
            "pets_owner_fkey",
            "FOREIGN KEY",
            "FOREIGN KEY (owner) REFERENCES app.companies(id) DEFERRABLE INITIALLY DEFERRED",
        ));
        match decide_pg(&probe, &snapshot_with("pets", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "definition"),
            v => panic!("expected FailDrift(definition) on a re-pointed FK, got {v:?}"),
        }
    }

    #[test]
    fn constraint_ifnotexists_with_definition_kind_clash_still_fails_kind() {
        // A kind clash takes precedence over the structural definition compare.
        let probe = GuardProbe::Constraint {
            schema: "app".into(),
            table: "pets".into(),
            name: "pets_owner_fkey".into(),
            direction: GuardDir::IfNotExists,
            expect_kind: Some("FOREIGN KEY".into()),
            expect_definition: Some("FOREIGN KEY (owner) REFERENCES app.people(id)".into()),
        };
        let mut t = empty_table();
        t.constraints.push(constraint("pets_owner_fkey", "UNIQUE", "UNIQUE (owner)"));
        match decide_pg(&probe, &snapshot_with("pets", t)) {
            GuardVerdict::FailDrift(d) => assert_eq!(d.field, "kind"),
            v => panic!("expected FailDrift(kind) on a kind clash, got {v:?}"),
        }
    }

    // -- ColumnPresence (alter/rename ifExists) ----------------------------

    #[test]
    fn column_presence_present_runs_absent_noops() {
        let probe = GuardProbe::ColumnPresence {
            schema: "app".into(),
            table: "users".into(),
            column: "name".into(),
            direction: GuardDir::IfExists,
        };
        let mut t = empty_table();
        t.columns.push(col("name", "text", true));
        assert_eq!(decide_pg(&probe, &snapshot_with("users", t)), GuardVerdict::RunBare);
        assert_eq!(decide_pg(&probe, &SchemaSnapshot::default()), GuardVerdict::SatisfiedNoop);
    }
}
