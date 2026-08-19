//! **Table presence means the same thing to the preview, the lower, and the fold.**
//!
//! Three places used to answer "which table names does a later op in this stream get
//! to reference": the fold (correctly), `render::lower`'s working `live_tables`, and
//! the offline preview's own presence carrier. The latter two knew `createTable` and
//! nothing else, so every one of them agreed only on streams that never dropped,
//! renamed or detached anything.
//!
//! Two failure directions, both measured on PostgreSQL 18.4 before this file was
//! written (see `PG ORACLE` notes on each test) — which matters, because a test that
//! only compares the preview against the fold proves they agree, not that either is
//! right about a real server:
//!
//! 1. **A dropped table stayed referenceable.** The lower INLINED a create-time
//!    foreign key onto a table the same envelope had already dropped, and the preview
//!    printed the whole thing as resolved SQL with nothing runtime-deferred. The
//!    server answers `ERROR: relation "alpha" does not exist` — mid-migration, after
//!    the DROP has already committed.
//! 2. **A renamed or detached table became unreachable.** The new name was never
//!    recorded, so a create-time FK naming it deferred with no target and the lower
//!    REFUSED the artifact outright. The server accepts both sequences.
//!
//! These are NOT preview-only defects, and this file deliberately measures both
//! layers: the preview's per-op carrier is a faithful surfacing of the lower, so a
//! fix applied only to the preview would have made the preview render SQL the lower
//! then refuses — trading a wrong answer for an inconsistent one.
//!
//! OFFLINE. No DB connection: the SQL quoted in the oracle notes was run by hand
//! against a live PostgreSQL and its verdict recorded here, exactly as the rest of
//! the preview suite records what it cannot re-derive offline.

use crate::support;

use zero_migrate::render::lower::{IrAuthor, LiveSchema};
use zero_migrate::render::sql_preview::{render_ir_envelope_sql, PreviewOpts, RUNTIME_RESOLVED};
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{resolve_create_table_policy, EffectivePolicy, MigrationIr};

fn opts(charter: &EffectivePolicy) -> PreviewOpts {
    PreviewOpts {
        default_schema: "public".to_string(),
        owner_app: "app_preview".to_string(),
        effective_policy: charter.clone(),
    }
}

fn resolve(ir: &str, charter: &EffectivePolicy) -> String {
    let raw: MigrationIr = serde_json::from_str(ir).expect("fixture IR parses");
    let resolved =
        resolve_create_table_policy(&raw, charter, "public").expect("fixture IR resolves");
    serde_json::to_string(&resolved).expect("resolved fixture serializes")
}

/// The table names the FOLD says exist after the stream — the authority the other
/// two layers are measured against.
fn folded_tables(resolved: &str, dialect: SqlDialect, charter: &EffectivePolicy) -> Vec<String> {
    let ir: MigrationIr = serde_json::from_str(resolved).expect("resolved parses");
    zero_migrate::render::fold::fold_ops(&ir.ops, dialect, "public", charter)
        .expect("the fixture streams are coherent for the fold")
        .tables
        .keys()
        .cloned()
        .collect()
}

/// What the WHOLE-IR lower — the path an apply takes — makes of the same stream.
fn whole_ir_lower(
    resolved: &str,
    dialect: SqlDialect,
    charter: &EffectivePolicy,
) -> Result<Vec<String>, String> {
    let ir: MigrationIr = serde_json::from_str(resolved).expect("resolved parses");
    let author = IrAuthor::new("public", "app_preview", dialect, charter);
    author
        .lower_steps(&ir, &LiveSchema::default())
        .map(|steps| {
            steps
                .iter()
                .filter_map(|step| match step {
                    zero_migrate::PlanStep::Ddl(m) => Some(m.up.clone()),
                    _ => None,
                })
                .collect()
        })
        .map_err(|e| e.to_string())
}

/// `createTable alpha` → `dropTable alpha` → `createTable gamma REFERENCES alpha`.
///
/// PG ORACLE (PostgreSQL 18.4, schema `sixth_drop`):
/// ```text
/// CREATE TABLE alpha (id varchar(255) PRIMARY KEY NOT NULL, label text);
/// DROP TABLE alpha;
/// CREATE TABLE gamma (id varchar(255) PRIMARY KEY, alpha_id text,
///   CONSTRAINT gamma_alpha_fk FOREIGN KEY (alpha_id) REFERENCES alpha (id));
/// -- ERROR:  relation "alpha" does not exist
/// ```
/// So an inline `REFERENCES … alpha` here is not a difference of opinion with the
/// fold; it is SQL the server refuses, printed by a preview claiming it resolved.
const DROP_THEN_FK: &str = r#"{
  "ir_version": 1,
  "name": "drop_then_fk",
  "ops": [
    {"op":"createTable","name":"alpha","columns":[{"name":"label","type":"text","nullable":true}]},
    {"op":"dropTable","table":"alpha"},
    {"op":"createTable","name":"gamma","columns":[{"name":"alpha_id","type":"text","nullable":true}],
      "constraints":[{"name":"gamma_alpha_fk","kind":{"kind":"fk","columns":["alpha_id"],
        "referencesTable":"alpha","referencesColumns":["id"]}}]}
  ]
}"#;

/// `createTable alpha` → `renameTable alpha -> beta` → `createTable gamma REFERENCES beta`.
///
/// PG ORACLE (PostgreSQL 18.4, schema `sixth_oracle`): the identical sequence runs
/// clean and leaves `beta` and `gamma`. Refusing it offline is a FALSE refusal — the
/// migration is legal and the author has nothing to act on.
const RENAME_THEN_FK: &str = r#"{
  "ir_version": 1,
  "name": "rename_then_fk",
  "ops": [
    {"op":"createTable","name":"alpha","columns":[{"name":"label","type":"text","nullable":true}]},
    {"op":"renameTable","table":"alpha","to":"beta"},
    {"op":"createTable","name":"gamma","columns":[{"name":"beta_id","type":"text","nullable":true}],
      "constraints":[{"name":"gamma_beta_fk","kind":{"kind":"fk","columns":["beta_id"],
        "referencesTable":"beta","referencesColumns":["id"]}}]}
  ]
}"#;

/// The OTHER half of a rename: the name the relation used to have stops resolving.
///
/// PG ORACLE (PostgreSQL 18.4, schema `sixth_stale`):
/// ```text
/// CREATE TABLE alpha (id varchar(255) PRIMARY KEY NOT NULL, label text);
/// ALTER TABLE alpha RENAME TO beta;
/// CREATE TABLE gamma (…, CONSTRAINT gamma_alpha_fk FOREIGN KEY (alpha_id)
///   REFERENCES alpha (id));
/// -- ERROR:  relation "alpha" does not exist
/// ```
/// A rename that only ADDED the new name would leave the old one referenceable and
/// inline exactly that statement. The neuter that makes `renameTable` skip its
/// `tables.remove` is invisible without this fixture, which is why it exists.
const RENAME_THEN_STALE_FK: &str = r#"{
  "ir_version": 1,
  "name": "rename_then_stale_fk",
  "ops": [
    {"op":"createTable","name":"alpha","columns":[{"name":"label","type":"text","nullable":true}]},
    {"op":"renameTable","table":"alpha","to":"beta"},
    {"op":"createTable","name":"gamma","columns":[{"name":"alpha_id","type":"text","nullable":true}],
      "constraints":[{"name":"gamma_alpha_fk","kind":{"kind":"fk","columns":["alpha_id"],
        "referencesTable":"alpha","referencesColumns":["id"]}}]}
  ]
}"#;

/// `detachPartition` promotes a child to an ordinary table, and the fold records it
/// as one.
///
/// PG ORACLE (PostgreSQL 18.4, schema `sixth_det`): `ALTER TABLE parentt DETACH
/// PARTITION childp;` followed by `CREATE TABLE gamma (… REFERENCES childp (id))`
/// is accepted.
const DETACH_THEN_FK: &str = r#"{
  "ir_version": 1,
  "name": "detach_then_fk",
  "ops": [
    {"op":"createTable","name":"parentt","columns":[{"name":"label","type":"text","nullable":true}],
      "partitionBy":{"kind":"list","columns":["id"]}},
    {"op":"createPartition","name":"childp","of":"parentt",
      "bounds":{"kind":"list","values":[{"kind":"string","value":"a"}]}},
    {"op":"detachPartition","parent":"parentt","name":"childp"},
    {"op":"createTable","name":"gamma","columns":[{"name":"child_id","type":"text","nullable":true}],
      "constraints":[{"name":"gamma_child_fk","kind":{"kind":"fk","columns":["child_id"],
        "referencesTable":"childp","referencesColumns":["id"]}}]}
  ]
}"#;

#[test]
fn preview_never_references_a_table_the_fold_says_was_dropped() {
    let charter = support::confined_charter();
    let resolved = resolve(DROP_THEN_FK, &charter);

    // The fold's verdict: `alpha` is gone, only `gamma` survives.
    assert_eq!(
        folded_tables(&resolved, SqlDialect::Postgres, &charter),
        vec!["gamma".to_string()],
        "the fold must retire a dropped table"
    );

    let preview = render_ir_envelope_sql(&resolved, SqlDialect::Postgres, &opts(&charter))
        .expect("renders offline");

    // The defect, stated as the SQL it produced: an inline create-time FK naming a
    // table this very envelope dropped four statements earlier. PostgreSQL refuses
    // that statement outright (see DROP_THEN_FK's oracle note).
    assert!(
        !preview.contains(r#"REFERENCES "public"."alpha""#),
        "preview rendered a create-time foreign key onto a table the same envelope \
         dropped; PostgreSQL answers `relation \"alpha\" does not exist`:\n{preview}"
    );

    // And the lower an apply takes must not author it either — the preview is only
    // ever as right as what it surfaces.
    if let Ok(statements) = whole_ir_lower(&resolved, SqlDialect::Postgres, &charter) {
        assert!(
            !statements
                .iter()
                .any(|s| s.contains(r#"REFERENCES "public"."alpha""#)),
            "the whole-IR lower authored an FK onto the dropped table:\n{statements:#?}"
        );
    }
}

#[test]
fn preview_reaches_a_table_under_the_name_the_fold_renamed_it_to() {
    let charter = support::confined_charter();
    for dialect in [SqlDialect::Postgres, SqlDialect::Sqlite] {
        let resolved = resolve(RENAME_THEN_FK, &charter);

        // The fold's verdict: the rename MOVED the relation; `beta` exists.
        let mut folded = folded_tables(&resolved, dialect, &charter);
        folded.sort();
        assert_eq!(
            folded,
            vec!["beta".to_string(), "gamma".to_string()],
            "{dialect:?}: the fold must re-key a renamed table"
        );

        let preview =
            render_ir_envelope_sql(&resolved, dialect, &opts(&charter)).expect("renders offline");

        // The defect: `beta` was never recorded, so the create-time FK naming it had
        // no target and the whole `createTable` degraded to a label. PostgreSQL
        // accepts the sequence, so there is nothing here for a preview to defer.
        assert!(
            !preview.contains(RUNTIME_RESOLVED),
            "{dialect:?}: preview deferred a createTable whose FK target the fold \
             says exists under its new name; PostgreSQL accepts this sequence:\n{preview}"
        );
        assert!(
            preview.contains("gamma"),
            "{dialect:?}: preview dropped the gamma createTable entirely:\n{preview}"
        );

        // Same question of the apply path, whose refusal the preview was surfacing.
        whole_ir_lower(&resolved, dialect, &charter).unwrap_or_else(|e| {
            panic!("{dialect:?}: the whole-IR lower REFUSED a sequence PostgreSQL accepts: {e}")
        });
    }
}

#[test]
fn preview_stops_reaching_a_table_under_the_name_the_fold_renamed_it_away_from() {
    let charter = support::confined_charter();
    let resolved = resolve(RENAME_THEN_STALE_FK, &charter);

    // The fold's verdict: `alpha` no longer names anything.
    let folded = folded_tables(&resolved, SqlDialect::Postgres, &charter);
    assert!(
        !folded.contains(&"alpha".to_string()),
        "the fold must retire the pre-rename name: {folded:?}"
    );

    let preview = render_ir_envelope_sql(&resolved, SqlDialect::Postgres, &opts(&charter))
        .expect("renders offline");
    assert!(
        !preview.contains(r#"REFERENCES "public"."alpha""#),
        "preview rendered a create-time foreign key onto the name a rename in the \
         same envelope moved away from; PostgreSQL answers `relation \"alpha\" does \
         not exist`:\n{preview}"
    );

    if let Ok(statements) = whole_ir_lower(&resolved, SqlDialect::Postgres, &charter) {
        assert!(
            !statements
                .iter()
                .any(|s| s.contains(r#"REFERENCES "public"."alpha""#)),
            "the whole-IR lower authored an FK onto the pre-rename name:\n{statements:#?}"
        );
    }
}

#[test]
fn preview_reaches_a_partition_the_fold_says_detach_promoted_to_a_table() {
    let charter = support::confined_charter();
    let resolved = resolve(DETACH_THEN_FK, &charter);

    let mut folded = folded_tables(&resolved, SqlDialect::Postgres, &charter);
    folded.sort();
    assert_eq!(
        folded,
        vec![
            "childp".to_string(),
            "gamma".to_string(),
            "parentt".to_string()
        ],
        "the fold must record a detached partition as an ordinary table"
    );

    let preview = render_ir_envelope_sql(&resolved, SqlDialect::Postgres, &opts(&charter))
        .expect("renders offline");
    assert!(
        !preview.contains(RUNTIME_RESOLVED),
        "preview deferred a createTable whose FK names a detached partition; \
         PostgreSQL accepts a foreign key onto one:\n{preview}"
    );

    whole_ir_lower(&resolved, SqlDialect::Postgres, &charter)
        .unwrap_or_else(|e| panic!("the whole-IR lower REFUSED a detach PostgreSQL accepts: {e}"));
}

/// **The measured exclusion.** `attachPartition` is the one op where the fold's
/// `tables` map and the referenceable-name set legitimately part company: the fold
/// `remove`s an attached child only because it re-homes the snapshot into its
/// `partitions` map, while the relation itself keeps existing.
///
/// PG ORACLE (PostgreSQL 18.4, schema `sixth_att`):
/// ```text
/// ALTER TABLE parentt ATTACH PARTITION childp FOR VALUES IN ('a');
/// CREATE TABLE gamma (…, CONSTRAINT gamma_child_fk FOREIGN KEY (child_id)
///   REFERENCES childp (id));           -- ACCEPTED
/// ```
/// So mirroring that removal would manufacture a refusal the server does not make.
/// This test exists so that "we left `attachPartition` out" stays a claim someone
/// can check rather than a sentence in a commit message.
///
/// `attachPartition` is a privileged vendor primitive, so this one authors under the
/// operator charter; that charter injects no platform columns, hence the explicit
/// `id` column and primary key the other fixtures get for free.
///
/// It is also measured through the LOWER rather than the preview, for a reason worth
/// recording: `render_ir_envelope_sql` validates with
/// `model::validate::validate_ir`, which takes a dialect and no policy, so an
/// envelope's vendor capabilities are checked against an ambient Confined creator set
/// regardless of the charter in `PreviewOpts`. An `attachPartition` therefore cannot
/// be previewed at all today. That is a separate, pre-existing boundary of the
/// preview and is NOT what this test is about — the referenceable-name rule under
/// test is the lower's, and the preview reads the same one.
#[test]
fn an_attached_partition_stays_referenceable_even_though_the_fold_unkeys_it() {
    const ATTACH_THEN_FK: &str = r#"{
      "ir_version": 1,
      "name": "attach_then_fk",
      "ops": [
        {"op":"createTable","name":"parentt","columns":[
            {"name":"id","type":{"string":{"length":255}},"nullable":false},
            {"name":"label","type":"text","nullable":true}],
          "primaryKey":["id"],
          "partitionBy":{"kind":"list","columns":["id"]}},
        {"op":"createTable","name":"childp","columns":[
            {"name":"id","type":{"string":{"length":255}},"nullable":false},
            {"name":"label","type":"text","nullable":true}],
          "primaryKey":["id"]},
        {"op":"attachPartition","parent":"parentt","name":"childp",
          "bound":{"kind":"list","values":[{"kind":"string","value":"a"}]}},
        {"op":"createTable","name":"gamma","columns":[
            {"name":"id","type":{"string":{"length":255}},"nullable":false},
            {"name":"child_id","type":{"string":{"length":255}},"nullable":true}],
          "primaryKey":["id"],
          "constraints":[{"name":"gamma_child_fk","kind":{"kind":"fk","columns":["child_id"],
            "referencesTable":"childp","referencesColumns":["id"]}}]}
      ]
    }"#;

    let charter = support::operator_charter("public");
    let resolved = resolve(ATTACH_THEN_FK, &charter);

    // The fold DOES un-key it — this is the divergence we are choosing to keep.
    let folded = folded_tables(&resolved, SqlDialect::Postgres, &charter);
    assert!(
        !folded.contains(&"childp".to_string()),
        "the fold is expected to re-home an attached child out of its tables map; \
         if that changed, this exclusion needs re-deciding: {folded:?}"
    );

    // The referenceable-name set must still reach it, because PostgreSQL does: the
    // create-time FK inlines rather than deferring onto a target that never arrives.
    let statements =
        whole_ir_lower(&resolved, SqlDialect::Postgres, &charter).unwrap_or_else(|e| {
            panic!(
                "the lower refused a foreign key onto an attached partition, which \
             PostgreSQL accepts: {e}"
            )
        });
    assert!(
        statements
            .iter()
            .any(|s| s.contains("gamma_child_fk") && s.contains("childp")),
        "the attached child must stay nameable by a later foreign key:\n{statements:#?}"
    );
}
