//! **The RENAME CARRIER SWEEP, measured against live PostgreSQL.**
//!
//! `docs/proposals/single-fold-and-effects.md` section H: "For every carrier that can
//! spell a column name ... a rename must follow it. Section B shows four found one at a
//! time and four still open." This is the sweep that ends the one-at-a-time pattern.
//!
//! A CARRIER is any place a column's name is stored as TEXT rather than as a structured
//! reference. `support::carriers` enumerates them by EXHAUSTIVE DESTRUCTURING of every
//! snapshot type, so a new field is a compile error here until it is classified; this
//! file supplies the fixture that populates every carrier the enumeration declares, and
//! asserts that the fixture really does populate all of them. A carrier nobody
//! exercises proves nothing, so an empty one fails.
//!
//! ## What is measured, and against what
//!
//! The same shape the rest of the fold oracles use. The CREATE half of the history is
//! applied through the real engine to a real PostgreSQL; the rename is then run as
//! PostgreSQL's own `ALTER TABLE ... RENAME COLUMN` (the engine's `renameColumn` op
//! lowers to an online expand-contract whose contract step is a separate deploy, so it
//! is the wrong instrument for "what does one rename do to the catalog" - see
//! `drift_between_fold_and_live` in `fold_rename_column_index_cascade_pg.rs`); the
//! server is then introspected. The SAME ops plus the rename are folded offline, and
//! the two carrier inventories are compared.
//!
//! PostgreSQL follows the rename into EVERY carrier, because every one of them is held
//! in the catalog as ATTRIBUTE NUMBERS and deparsed on read - `conkey` for a
//! constraint, `indkey` / `indpred` / `indexprs` for an index, `partattrs` for a
//! partition key, the generated column's parse tree for its expression. Measured on
//! PostgreSQL 18.4, renaming `a` to `renamed_a`:
//!
//! ```text
//! CHECK (((a > 0) AND (note <> 'a'::text)))  ->  CHECK (((renamed_a > 0) AND (note <> 'a'::text)))
//! UNIQUE (a, note)                           ->  UNIQUE (renamed_a, note)
//! PRIMARY KEY (a, b)                         ->  PRIMARY KEY (renamed_a, b)
//! FOREIGN KEY (a) REFERENCES parent(pid)     ->  FOREIGN KEY (renamed_a) REFERENCES parent(pid)
//! EXCLUDE USING gist (ex WITH =, a WITH =)   ->  EXCLUDE USING gist (ex WITH =, renamed_a WITH =)
//! ... USING btree (a) WHERE (a > 0)          ->  ... USING btree (renamed_a) WHERE (renamed_a > 0)
//! ... USING btree (((a + 1)))                ->  ... USING btree (((renamed_a + 1)))
//! ... USING btree (b) INCLUDE (a)            ->  ... USING btree (b) INCLUDE (renamed_a)
//! RANGE (created_at)                         ->  RANGE (event_day)
//! ```
//!
//! Note the LITERAL: `note <> 'a'` survives untouched while the reference beside it
//! moves. That is the trap that ruled out naive text substitution for years, and
//! `have_the_carriers_that_hold_rendered_sql_kept_their_string_literals` pins it
//! directly - a rewrite that corrupts a literal spelling the old column name is worse
//! than the staleness it repairs.
//!
//! ## Why a spelling is scrubbed before it is searched
//!
//! `carrier_spellings_that_reference_a_column` strips `'...'` string literals before
//! looking for the old name. A string literal is NOT a column reference on either side
//! - PostgreSQL leaves it alone, and so must the fold - so leaving it in the haystack
//! would make the fixture's own anti-corruption witness read as a missed carrier. The
//! literal is asserted separately, and asserted to SURVIVE.

use crate::support;

use std::collections::{BTreeMap, BTreeSet};

use crate::support::carriers::{carriers_of_schema, REQUIRED_CARRIER_FIELDS};
use crate::support::PgDevSession;
use zero_migrate::apply::backend::MigrationBackend;
use zero_migrate::driver::SqlSession;
use zero_migrate::{
    fold_ops, resolve_create_table_policy, snapshot_schema, Approval, EffectivePolicy,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr,
    PostgresBackend, SchemaSnapshot, SqlDialect,
};

const OWNER: &str = "app_rename_carrier_sweep_pg";

/// The pre-rename column name. Chosen so it is NOT a substring of [`NEW_COLUMN`] and
/// vice versa, so "does this body name the old column" needs no tokenizer.
const OLD_COLUMN: &str = "qty_on_hand";

/// The post-rename column name.
const NEW_COLUMN: &str = "amount_on_hand";

/// The SECOND renamed column, and the reason there is one. `inline_checks` is populated
/// only by the enum / domain / UUID / value-format writers, and every one of them
/// attaches its predicate to the column it describes - so no rename of the INT column
/// above can ever exercise it. `sku_code` is a TypeID-formatted TEXT column, which is
/// the shape that makes the PostgreSQL fold emit an inline format CHECK naming its own
/// column. Renaming it is what puts `TableSnapshot::columns[].inline_checks` on the
/// swept list rather than the excused one.
const OLD_FORMAT_COLUMN: &str = "sku_code";

/// The post-rename name of [`OLD_FORMAT_COLUMN`].
const NEW_FORMAT_COLUMN: &str = "article_code";

/// Every rename this fixture performs, as `(before, after)`. The sweep searches for
/// EVERY `before` and the live oracle looks for every `after`, so adding a rename to
/// the IR without adding it here would silently narrow the sweep.
const RENAMES: &[(&str, &str)] = &[
    (OLD_COLUMN, NEW_COLUMN),
    (OLD_FORMAT_COLUMN, NEW_FORMAT_COLUMN),
];

/// The table that carries every non-partition carrier.
const MAIN_TABLE: &str = "carrier_sweep_main";

/// The PARTITIONED table. It cannot be merged into [`MAIN_TABLE`]: PostgreSQL refuses
/// an EXCLUDE constraint on a partitioned table, and refuses a primary key that does
/// not contain the partition key. So the fixture spreads the carriers over two tables
/// and the inventory unions them back together.
const PART_TABLE: &str = "carrier_sweep_partitioned";

/// A string literal that SPELLS the column being renamed, planted inside the CHECK
/// body. This is the false positive that rules out naive substitution, and the reason
/// every rewrite in this crate walks quoted runs instead: after the rename the
/// reference beside it must read `amount_on_hand` while this literal still reads
/// `qty_on_hand`.
const LITERAL_TRAP: &str = "qty_on_hand";

fn token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "carrier_sweep_pg_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// The CREATE half: every carrier the inventory declares, populated at once.
///
/// The EXCLUDE constraint uses `USING btree`, not `gist`. `btree` supports an exclusion
/// constraint over `=` natively, so the fixture needs no `btree_gist` extension - and
/// installing one would leave a catalog object behind in a database this test is
/// required to return unchanged.
const CREATE_IR: &str = r#"{
  "ir_version": 1,
  "name": "carrier_sweep_create",
  "owner_app": "app_rename_carrier_sweep_pg",
  "ops": [
    {"op":"createTable","name":"carrier_sweep_parent","columns":[
      {"name":"id","type":"int","nullable":false}
    ],"primaryKey":["id"]},

    {"op":"createTable","name":"carrier_sweep_main","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":false},
      {"name":"note","type":"text","nullable":true},
      {"name":"total_cents","type":"int","nullable":true,
       "generated":{"expr":{"node":"binOp","op":"add",
         "lhs":{"node":"colRef","name":"qty_on_hand"},
         "rhs":{"node":"literal","value":1}},"stored":true}},
      {"name":"sku_code","type":"text","nullable":true,
       "valueFormat":{"typeId":{"prefix":"sku"}}}
    ],"primaryKey":["id","qty_on_hand"],"constraints":[
      {"name":"carrier_sweep_main_uq","kind":{"kind":"unique",
        "columns":["qty_on_hand","note"]}},
      {"name":"carrier_sweep_main_ck","kind":{"kind":"check","expr":{
        "node":"binOp","op":"and",
        "lhs":{"node":"binOp","op":"gt",
          "lhs":{"node":"colRef","name":"qty_on_hand"},
          "rhs":{"node":"literal","value":0}},
        "rhs":{"node":"binOp","op":"ne",
          "lhs":{"node":"colRef","name":"note"},
          "rhs":{"node":"literal","value":"qty_on_hand"}}}}},
      {"name":"carrier_sweep_main_ex","kind":{"kind":"exclusion","usingMethod":"btree",
        "elements":[{"target":{"kind":"column","name":"qty_on_hand"},"operator":"="}]}}
    ]},

    {"op":"addConstraint","table":"carrier_sweep_main","constraint":{
      "name":"carrier_sweep_main_fk","kind":{"kind":"fk",
        "columns":["qty_on_hand"],
        "referencesTable":"carrier_sweep_parent","referencesColumns":["id"]}}},

    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_plain",
     "columns":[{"kind":"column","name":"qty_on_hand"}],"unique":false},
    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_partial",
     "columns":[{"kind":"column","name":"id"}],"unique":false,
     "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"qty_on_hand"},
              "rhs":{"node":"literal","value":0}}},
    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_expr",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"qty_on_hand"},"rhs":{"node":"literal","value":1}}}],
     "unique":false},
    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_incl",
     "columns":[{"kind":"column","name":"note"}],"include":["qty_on_hand"],"unique":false},

    {"op":"createTable","name":"carrier_sweep_partitioned","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":false}
    ],"partitionBy":{"kind":"range","columns":["qty_on_hand"],"collapse":false}}
  ]
}"#;

/// The SAME ops plus the two renames, folded offline. This is the projection under
/// test: every carrier in it must spell [`NEW_COLUMN`].
const FOLDED_IR: &str = r#"{
  "ir_version": 1,
  "name": "carrier_sweep_folded",
  "owner_app": "app_rename_carrier_sweep_pg",
  "ops": [
    {"op":"createTable","name":"carrier_sweep_parent","columns":[
      {"name":"id","type":"int","nullable":false}
    ],"primaryKey":["id"]},

    {"op":"createTable","name":"carrier_sweep_main","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":false},
      {"name":"note","type":"text","nullable":true},
      {"name":"total_cents","type":"int","nullable":true,
       "generated":{"expr":{"node":"binOp","op":"add",
         "lhs":{"node":"colRef","name":"qty_on_hand"},
         "rhs":{"node":"literal","value":1}},"stored":true}},
      {"name":"sku_code","type":"text","nullable":true,
       "valueFormat":{"typeId":{"prefix":"sku"}}}
    ],"primaryKey":["id","qty_on_hand"],"constraints":[
      {"name":"carrier_sweep_main_uq","kind":{"kind":"unique",
        "columns":["qty_on_hand","note"]}},
      {"name":"carrier_sweep_main_ck","kind":{"kind":"check","expr":{
        "node":"binOp","op":"and",
        "lhs":{"node":"binOp","op":"gt",
          "lhs":{"node":"colRef","name":"qty_on_hand"},
          "rhs":{"node":"literal","value":0}},
        "rhs":{"node":"binOp","op":"ne",
          "lhs":{"node":"colRef","name":"note"},
          "rhs":{"node":"literal","value":"qty_on_hand"}}}}},
      {"name":"carrier_sweep_main_ex","kind":{"kind":"exclusion","usingMethod":"btree",
        "elements":[{"target":{"kind":"column","name":"qty_on_hand"},"operator":"="}]}}
    ]},

    {"op":"addConstraint","table":"carrier_sweep_main","constraint":{
      "name":"carrier_sweep_main_fk","kind":{"kind":"fk",
        "columns":["qty_on_hand"],
        "referencesTable":"carrier_sweep_parent","referencesColumns":["id"]}}},

    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_plain",
     "columns":[{"kind":"column","name":"qty_on_hand"}],"unique":false},
    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_partial",
     "columns":[{"kind":"column","name":"id"}],"unique":false,
     "where":{"node":"binOp","op":"gt","lhs":{"node":"colRef","name":"qty_on_hand"},
              "rhs":{"node":"literal","value":0}}},
    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_expr",
     "columns":[{"kind":"expr","expr":{"node":"binOp","op":"add",
       "lhs":{"node":"colRef","name":"qty_on_hand"},"rhs":{"node":"literal","value":1}}}],
     "unique":false},
    {"op":"createIndex","table":"carrier_sweep_main","name":"carrier_sweep_main_incl",
     "columns":[{"kind":"column","name":"note"}],"include":["qty_on_hand"],"unique":false},

    {"op":"createTable","name":"carrier_sweep_partitioned","columns":[
      {"name":"id","type":"int","nullable":false},
      {"name":"qty_on_hand","type":"int","nullable":false}
    ],"partitionBy":{"kind":"range","columns":["qty_on_hand"],"collapse":false}},

    {"op":"renameColumn","table":"carrier_sweep_main","from":"qty_on_hand",
     "to":"amount_on_hand","type":"int"},
    {"op":"renameColumn","table":"carrier_sweep_partitioned","from":"qty_on_hand",
     "to":"amount_on_hand","type":"int"},
    {"op":"renameColumn","table":"carrier_sweep_main","from":"sku_code",
     "to":"article_code","type":"text"}
  ]
}"#;

/// Strip every `'...'` STRING LITERAL from a rendered SQL fragment, so a search for a
/// column name cannot be answered by a literal that merely spells one.
///
/// Quote DOUBLING is the escape grammar on every dialect this crate emits
/// (`render::dml::sql_string_literal`), and PostgreSQL's deparser uses it too, so a
/// doubled `''` inside a run does not close it.
fn strip_string_literals(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some(open) = rest.find('\'') {
        out.push_str(&rest[..open]);
        let mut cursor = open + 1;
        loop {
            let Some(offset) = rest[cursor..].find('\'') else {
                // An unterminated run: everything after it is inside the literal.
                return out;
            };
            let close = cursor + offset;
            if rest[close + 1..].starts_with('\'') {
                cursor = close + 2;
                continue;
            }
            cursor = close + 1;
            break;
        }
        rest = &rest[cursor..];
    }
    out.push_str(rest);
    out
}

/// Every carrier spelling in `snapshot`, with string literals scrubbed, keyed by field
/// path. This is the haystack the sweep searches for a stale column name.
fn carrier_spellings_that_reference_a_column(
    snapshot: &SchemaSnapshot,
) -> BTreeMap<&'static str, Vec<String>> {
    carriers_of_schema(snapshot)
        .into_iter()
        .map(|carrier| {
            (
                carrier.field,
                carrier
                    .spellings
                    .iter()
                    .map(|spelling| strip_string_literals(spelling))
                    .collect(),
            )
        })
        .collect()
}

/// Fold the CREATE half ALONE - the same history with no rename applied.
///
/// The baseline the coverage gate interrogates: it answers "did the fixture put a
/// to-be-renamed column into this carrier at all", which is the question a non-emptiness
/// check cannot ask. The project schema is irrelevant to a carrier's spelling, so this
/// needs no live database and can run under the same policy shape the measured fold uses.
fn fold_baseline() -> SchemaSnapshot {
    let schema = "carrier_sweep_baseline";
    fold(CREATE_IR, &support::no_inject(schema), schema)
        .expect("the create-only history folds offline")
}

/// Fold one authored IR document offline, through the same policy resolution the engine
/// applies.
fn fold(
    ir: &str,
    policy: &EffectivePolicy,
    project_schema: &str,
) -> Result<SchemaSnapshot, String> {
    let authored: MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, policy, project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    fold_ops(&resolved.ops, SqlDialect::Postgres, project_schema, policy)
        .map_err(|error| format!("fold the PostgreSQL ops: {error}"))
}

/// Apply `ir` through the real engine against the real database.
async fn apply_through_engine(
    backend: &PostgresBackend<'_, PgDevSession>,
    cfg: &ExecutorConfig,
    policy: &EffectivePolicy,
    ir: &str,
) -> Result<(), String> {
    let authored: MigrationIr =
        serde_json::from_str(ir).map_err(|error| format!("parse test IR: {error}"))?;
    let resolved = resolve_create_table_policy(&authored, policy, &cfg.project_schema)
        .map_err(|error| format!("resolve create-table policy: {error}"))?;
    let resolved_source = serde_json::to_string(&resolved)
        .map_err(|error| format!("serialize resolved IR: {error}"))?;
    let author = IrAuthor::new(&cfg.project_schema, OWNER, SqlDialect::Postgres, policy);
    let guard = GuardConfig::from_policy(policy.clone(), SqlDialect::Postgres);
    let artifact = author
        .load_and_lower_guarded(
            &resolved_source,
            OWNER,
            &BTreeMap::new(),
            &LiveSchema::default(),
            &guard,
        )
        .map_err(|error| format!("load and lower guarded IR plan: {error}"))?;
    MigrationEngine::new()
        .apply_plan(
            &artifact.plan.steps,
            Approval::Approved,
            backend,
            cfg,
            "rename-carrier-sweep-pg",
            LockMode::Acquire,
        )
        .await
        .map(|_| ())
        .map_err(|error| format!("{error}"))
}

/// What the run measured: both inventories, plus the raw folded snapshot for the
/// assertions that need more than a carrier spelling.
struct Measured {
    folded: SchemaSnapshot,
    live: SchemaSnapshot,
}

/// Create everything through the real engine, rename both columns with PostgreSQL's own
/// DDL, introspect, and fold the same history offline. `None` when no live database is
/// configured.
async fn measure() -> Option<Measured> {
    let Some(url) = support::pg_url() else {
        support::announce_live_db_skip(support::PG_URL_ENV);
        return None;
    };
    let session = PgDevSession::connect(&url);
    let schema = token();
    let policy = support::no_inject(&schema);
    let cfg = ExecutorConfig::new(format!("project_{schema}"), &schema, policy.clone());
    let quoted_schema = quote_ident(&cfg.project_schema);
    let quoted_meta_schema = quote_ident(&cfg.confinement.meta_schema);
    let _schema_guard = support::SchemaGuard::arm(
        &session,
        [
            cfg.project_schema.clone(),
            cfg.confinement.meta_schema.clone(),
        ],
    );
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .expect("create isolated carrier-sweep schema");

    let work: Result<Measured, String> = async {
        let backend = PostgresBackend::new_generic(&session);
        backend
            .ensure_journal(&cfg)
            .await
            .map_err(|error| format!("ensure migration journal: {error}"))?;

        let policy = support::no_inject(&cfg.project_schema);
        apply_through_engine(&backend, &cfg, &policy, CREATE_IR).await?;

        for (table, from, to) in [
            (MAIN_TABLE, OLD_COLUMN, NEW_COLUMN),
            (PART_TABLE, OLD_COLUMN, NEW_COLUMN),
            (MAIN_TABLE, OLD_FORMAT_COLUMN, NEW_FORMAT_COLUMN),
        ] {
            let rename = format!(
                "ALTER TABLE {quoted_schema}.{} RENAME COLUMN {} TO {}",
                quote_ident(table),
                quote_ident(from),
                quote_ident(to)
            );
            session
                .batch(&rename)
                .await
                .map_err(|error| format!("run native SQL `{rename}`: {error}"))?;
        }

        let folded = fold(FOLDED_IR, &policy, &cfg.project_schema)?;
        let live = snapshot_schema(&session, &cfg.project_schema)
            .await
            .map_err(|error| format!("snapshot the live PostgreSQL schema: {error}"))?;

        Ok(Measured { folded, live })
    }
    .await;

    let cleanup = session
        .batch(&format!(
            "DROP SCHEMA IF EXISTS {quoted_schema} CASCADE; \
             DROP SCHEMA IF EXISTS {quoted_meta_schema} CASCADE"
        ))
        .await;
    match (work, cleanup) {
        (Ok(measured), Ok(())) => Some(measured),
        (Err(work), Ok(())) => panic!("{work}"),
        (Ok(_), Err(cleanup)) => panic!("drop PostgreSQL test schemas: {cleanup}"),
        (Err(work), Err(cleanup)) => panic!("{work}; cleanup failed: {cleanup}"),
    }
}

/// The gate that makes the inventory a PROOF rather than a list: every carrier field the
/// exhaustive destructure declares has to be POPULATED by this fixture. Adding a field
/// to a snapshot type is already a compile error in `support::carriers`; classifying the
/// new field as a carrier and then never exercising it fails HERE.
#[compio::test]
async fn does_the_fixture_populate_every_carrier_the_inventory_declares() {
    let Some(measured) = measure().await else {
        return;
    };
    let spellings = carrier_spellings_that_reference_a_column(&measured.folded);

    let declared: BTreeSet<&str> = spellings.keys().copied().collect();
    let required: BTreeSet<&str> = REQUIRED_CARRIER_FIELDS.iter().copied().collect();

    let undeclared: Vec<&&str> = required.difference(&declared).collect();
    assert!(
        undeclared.is_empty(),
        "`REQUIRED_CARRIER_FIELDS` names carrier fields the inventory never declared: \
         {undeclared:?}. Either the field path was renamed in `support::carriers` and \
         not here, or the fixture built no object of that shape at all"
    );

    // Coverage is measured on the NEVER-RENAMED baseline, and demands that each carrier
    // HELD a to-be-renamed column rather than merely being non-empty. A carrier
    // populated with some OTHER column satisfies the sweep without any rename having
    // touched it, which is a green that proves nothing.
    let baseline = fold_baseline();
    let held: BTreeSet<&str> = carriers_of_schema(&baseline)
        .into_iter()
        .filter(|carrier| {
            carrier.spellings.iter().any(|spelling| {
                RENAMES
                    .iter()
                    .any(|(from, _)| strip_string_literals(spelling).contains(from))
            })
        })
        .map(|carrier| carrier.field)
        .collect();
    let unexercised: Vec<&&str> = required.difference(&held).collect();
    assert!(
        unexercised.is_empty(),
        "before any rename, these carriers do not hold a to-be-renamed column, so the \
         sweep below proves nothing about them: {unexercised:?}. Extend `CREATE_IR` / \
         `FOLDED_IR` so each one names {RENAMES:?} - a carrier nobody exercises is \
         exactly the hole this sweep exists to close"
    );

    // The two classes the inventory deliberately does NOT sweep, asserted rather than
    // assumed, so the exclusions stay testable claims.
    let main = measured
        .folded
        .tables
        .get(MAIN_TABLE)
        .expect("the folded main table survives");
    assert!(
        main.stored_create_sql.is_none(),
        "`TableSnapshot::stored_create_sql` is classified `must_not_follow` on the \
         grounds that the FOLD never produces one, but this fold produced {:?}. If the \
         fold can populate it, it is a carrier and needs a rewrite",
        main.stored_create_sql
    );
    let exclusion = main
        .constraints
        .iter()
        .find(|c| c.kind == "EXCLUDE")
        .expect("the folded EXCLUDE constraint survives the rename");
    assert!(
        exclusion.definition.is_empty(),
        "the fold writes an EMPTY `definition` for an EXCLUDE on purpose, so \
         `REQUIRED_CARRIER_FIELDS` omits that path; it read {:?}, which would make it a \
         carrier that needs sweeping",
        exclusion.definition
    );
}

/// The sweep itself. After a rename, no carrier of the FOLDED snapshot may still spell
/// the old column name.
#[compio::test]
async fn does_every_folded_carrier_follow_a_column_rename() {
    let Some(measured) = measure().await else {
        return;
    };
    let spellings = carrier_spellings_that_reference_a_column(&measured.folded);

    let mut stale: Vec<String> = Vec::new();
    for (field, values) in &spellings {
        for value in values {
            for (from, to) in RENAMES {
                if value.contains(from) {
                    stale.push(format!(
                        "  {field}: {value}  (still names `{from}`, not `{to}`)"
                    ));
                }
            }
        }
    }
    assert!(
        stale.is_empty(),
        "these folded carriers still spell a PRE-rename column name:\n{}\n\nPostgreSQL \
         follows the rename into every one of them (it holds each as attribute NUMBERS \
         and deparses on read), so each line above is a snapshot describing a column the \
         table no longer has - phantom drift where the field is compared, and DDL naming \
         a dead column where it is spliced into SQL",
        stale.join("\n")
    );
}

/// The oracle that gives the sweep above its meaning: the SERVER really did move every
/// carrier. Without this, a fold that simply dropped the objects would pass.
#[compio::test]
async fn does_live_postgresql_follow_the_rename_into_every_carrier_it_reports() {
    let Some(measured) = measure().await else {
        return;
    };
    let live = carrier_spellings_that_reference_a_column(&measured.live);

    let mut stale: Vec<String> = Vec::new();
    let mut unseen: Vec<&str> = Vec::new();
    for (from, to) in RENAMES {
        let mut saw_new_name = false;
        for (field, values) in &live {
            for value in values {
                if value.contains(from) {
                    stale.push(format!("  {field}: {value}  (still names `{from}`)"));
                }
                if value.contains(to) {
                    saw_new_name = true;
                }
            }
        }
        if !saw_new_name {
            unseen.push(to);
        }
    }
    assert!(
        stale.is_empty(),
        "live PostgreSQL still reports a PRE-rename column name in these \
         carriers:\n{}\nthat contradicts the measurement this whole sweep is built on",
        stale.join("\n")
    );
    assert!(
        unseen.is_empty(),
        "no live carrier mentions {unseen:?} at all, so the introspection read something \
         other than the renamed tables and the sweep above compares against nothing"
    );
}

/// The anti-corruption witness. A rewrite that follows a rename into rendered SQL must
/// move the column REFERENCE and leave a STRING LITERAL spelling the same word alone.
/// This is the exact false positive that ruled out naive substitution, and getting it
/// wrong silently changes what a CHECK enforces - strictly worse than the staleness the
/// rewrite repairs.
#[compio::test]
async fn have_the_carriers_that_hold_rendered_sql_kept_their_string_literals() {
    let Some(measured) = measure().await else {
        return;
    };

    for (label, snapshot) in [("the fold", &measured.folded), ("live", &measured.live)] {
        let check = snapshot
            .tables
            .get(MAIN_TABLE)
            .and_then(|table| {
                table
                    .constraints
                    .iter()
                    .find(|c| c.name == "carrier_sweep_main_ck")
            })
            .unwrap_or_else(|| panic!("{label} lost the CHECK constraint"));

        assert!(
            check.definition.contains(&format!("'{LITERAL_TRAP}'")),
            "{label}'s CHECK body must still carry the string literal \
             `'{LITERAL_TRAP}'` after `rename {OLD_COLUMN} -> {NEW_COLUMN}`; it read \
             `{}`. A rewrite that substitutes inside a literal changes what the \
             constraint ENFORCES",
            check.definition
        );
        assert!(
            check.definition.contains(NEW_COLUMN),
            "{label}'s CHECK body must name the POST-rename column `{NEW_COLUMN}`; it \
             read `{}`",
            check.definition
        );
    }
}
