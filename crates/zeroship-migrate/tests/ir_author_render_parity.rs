//! §6.4 — the cross-path byte-identity golden gate.
//!
//! For `createTable` and each DDL op, the stand-alone `IrAuthor::lower` render
//! MUST be byte-identical to the render the declarative path
//! (`DeclarativeAuthor::diff`) produces for the same LOGICAL shape. We construct
//! the same table TWO ways:
//!   - a `t.*` schema (descriptors) fed through `DeclarativeAuthor::diff`, and
//!   - the equivalent DSL IR fed through `IrAuthor::lower`,
//! and assert the emitted SQL (`up` + `down` + the `COMMENT ON COLUMN` side
//! outputs + the injected system fields) is identical.
//!
//! This is a PURE-RENDER gate (no DB needed): both paths build a `TableSnapshot`
//! from the SHARED `build_table_snapshot` and render through the SAME methods, so
//! the test proves the §6.5 single-source builder + the §6.4 seam agree. If a
//! future change forks the two outputs, this fails.

use std::collections::{BTreeSet, HashMap};

use zeroship_migrate::ir::{ColType, IrColumn, IrIndex, MigrationIr, Op};
use zeroship_migrate::ir_author::IrAuthor;
use zeroship_migrate::{
    CollectionDescriptor, DeclarativeAuthor, DesiredSchema, FieldDescriptor, IndexDescriptor,
    SchemaSnapshot, TableSnapshot,
};
use zeroship_schema::query::SqlDialect;

/// A live `TableSnapshot` placeholder for an FK target — only its presence as a
/// live key matters to the differ's inline-vs-defer decision.
fn empty_table_snapshot() -> TableSnapshot {
    TableSnapshot {
        columns: vec![],
        indexes: vec![],
        constraints: vec![],
        stored_create_sql: None,
    }
}

const SCHEMA: &str = "app";
const OWNER: &str = "app_test";

/// The `(up, down)` SQL pairs of a migration list — the byte-comparable render
/// surface (the UUIDv7 version + the human name are non-deterministic identity,
/// excluded from the parity comparison).
fn sql_pairs(migs: &[zeroship_migrate::Migration]) -> Vec<(String, Option<String>)> {
    migs.iter().map(|m| (m.up.clone(), m.down.clone())).collect()
}

/// Run the declarative path: descriptors → `desired_snapshot` → `diff` against an
/// empty live schema → the emitted migrations, on the given dialect.
fn declarative_pairs_for(
    descs: &[CollectionDescriptor],
    dialect: SqlDialect,
) -> Vec<(String, Option<String>)> {
    let desired: DesiredSchema =
        zeroship_migrate::declarative::desired_snapshot_for_dialect(SCHEMA, descs, dialect)
            .expect("desired snapshot");
    let author = DeclarativeAuthor::new_for_dialect(SCHEMA, OWNER, dialect);
    let plan = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("declarative diff");
    sql_pairs(&plan.migrations)
}

/// Run the declarative path on Postgres (the historical helper).
fn declarative_pairs(descs: &[CollectionDescriptor]) -> Vec<(String, Option<String>)> {
    declarative_pairs_for(descs, SqlDialect::Postgres)
}

/// Run the IR path: ops → `IrAuthor::lower` against the given live tables, on the
/// given dialect.
fn ir_pairs_for(
    ops: Vec<Op>,
    live: &BTreeSet<String>,
    dialect: SqlDialect,
) -> Vec<(String, Option<String>)> {
    let ir = MigrationIr {
        ir_version: 1,
        name: "m".into(),
        owner_app: OWNER.into(),
        ops,
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    };
    let author = IrAuthor::new(SCHEMA, OWNER, dialect);
    let migs = author.lower(&ir, live).expect("ir lower");
    sql_pairs(&migs)
}

/// Run the IR path on Postgres (the historical helper).
fn ir_pairs(ops: Vec<Op>, live: &BTreeSet<String>) -> Vec<(String, Option<String>)> {
    ir_pairs_for(ops, live, SqlDialect::Postgres)
}

#[test]
fn create_table_render_is_byte_identical_pg() {
    // A rich table: a NOT NULL string, a unique string, a plain int, a default,
    // and a self-contained shape (no FK so both paths inline nothing). The
    // declarative path injects the seven system fields + three system indexes;
    // the IR path MUST inject the identical set via the shared builder.
    let desc = CollectionDescriptor {
        name: "widgets".into(),
        owner_app: OWNER.into(),
        fields: vec![
            FieldDescriptor {
                name: "title".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "slug".into(),
                ty: "string".into(),
                unique: true,
                ..Default::default()
            },
            FieldDescriptor { name: "qty".into(), ty: "int".into(), ..Default::default() },
        ],
        indexes: vec![],
    };

    let ops = vec![Op::CreateTable {
        name: "widgets".into(),
        columns: vec![
            IrColumn {
                name: "title".into(),
                ty: ColType::String,
                nullable: Some(false),
                default: None,
                unique: None,
            },
            IrColumn {
                name: "slug".into(),
                ty: ColType::String,
                nullable: None,
                default: None,
                unique: Some(true),
            },
            IrColumn {
                name: "qty".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None,
            },
        ],
        constraints: vec![],
        indexes: vec![],
    }];

    let decl = declarative_pairs(&[desc]);
    let ir = ir_pairs(ops, &BTreeSet::new());
    assert_eq!(
        decl, ir,
        "createTable render must be byte-identical across the declarative and IR paths"
    );
    // Sanity: the render is non-trivial (a CREATE TABLE + at least the unique index).
    assert!(decl.iter().any(|(up, _)| up.contains("CREATE TABLE \"app\".\"widgets\"")));
    assert!(decl.iter().any(|(up, _)| up.contains("CREATE UNIQUE INDEX")));
}

#[test]
fn create_table_with_live_fk_render_is_byte_identical_pg() {
    // A table with a ref → an ALREADY-LIVE table: both paths INLINE the FK (the
    // target exists), so the CREATE carries the inline `CONSTRAINT … FOREIGN KEY`.
    let posts = CollectionDescriptor {
        name: "posts".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            references: Some("authors".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    // `authors` is declared (so it stays in the union, not dropped) AND already
    // live (so the FK inlines, not deferred). The diff then emits ONLY the `posts`
    // CREATE — directly comparable to the IR path's single createTable.
    let authors = CollectionDescriptor {
        name: "authors".into(),
        owner_app: OWNER.into(),
        fields: vec![],
        indexes: vec![],
    };
    let mut live_snapshot = SchemaSnapshot::default();
    live_snapshot.tables.insert("authors".into(), empty_table_snapshot());

    let desired = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[posts, authors])
        .expect("desired snapshot");
    let mut live_ownership = HashMap::new();
    live_ownership.insert("authors".to_string(), OWNER.to_string());
    let author = DeclarativeAuthor::new(SCHEMA, OWNER);
    let plan = author
        .diff(&desired, &live_snapshot, &live_ownership, &[])
        .expect("declarative diff");
    let decl = sql_pairs(&plan.migrations);

    let ops = vec![Op::CreateTable {
        name: "posts".into(),
        columns: vec![IrColumn {
            name: "author".into(),
            ty: ColType::Ref { references: "authors".into() },
            nullable: None,
            default: None,
            unique: None,
        }],
        constraints: vec![],
        indexes: vec![],
    }];
    let mut live = BTreeSet::new();
    live.insert("authors".to_string());
    let ir = ir_pairs(ops, &live);

    // Compare only the `posts`-related render (the empty live `authors` snapshot
    // makes the differ also backfill `authors`' system fields — a test-setup
    // artifact unrelated to the createTable-with-FK render we are pinning).
    let decl_posts: Vec<_> =
        decl.into_iter().filter(|(up, _)| up.contains("posts")).collect();
    let ir_posts: Vec<_> = ir.into_iter().filter(|(up, _)| up.contains("posts")).collect();
    assert_eq!(
        decl_posts, ir_posts,
        "createTable-with-inline-FK render must match across paths"
    );
    assert!(
        decl_posts.iter().any(|(up, _)| up.contains("FOREIGN KEY")),
        "the FK must be inlined into the CREATE on both paths"
    );
}

#[test]
fn create_table_with_encrypted_column_render_is_byte_identical_pg() {
    // The sentinel trap (§6.5): an encrypted column. Both paths MUST carry the
    // byte-identical BYTEA type + the `/* zsenc:… */` inline sentinel + the
    // `COMMENT ON COLUMN … 'zsenc:…'` side output — built by the shared kernel
    // (`zeroship_schema::{query,mask_codec}`), NEVER re-spelled in IrAuthor.
    let desc = CollectionDescriptor {
        name: "vault".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor {
            name: "secret".into(),
            ty: "string".into(),
            encrypted: Some(serde_json::json!({})),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let ops = vec![Op::CreateTable {
        name: "vault".into(),
        columns: vec![IrColumn {
            // The IR carries an encrypted column wrapping a string.
            name: "secret".into(),
            ty: ColType::Encrypted { of: Box::new(ColType::String) },
            nullable: None,
            default: None,
            unique: None,
        }],
        constraints: vec![],
        indexes: vec![],
    }];

    let decl = declarative_pairs(&[desc]);
    let ir = ir_pairs(ops, &BTreeSet::new());

    assert_eq!(
        decl, ir,
        "encrypted-column createTable render must be byte-identical across paths"
    );
    // Sanity: the encryption sentinel (built by the shared kernel) is present.
    assert!(
        decl.iter().any(|(up, _)| up.contains("zsenc:")),
        "the encryption sentinel must be emitted (shared-kernel source, not re-spelled)"
    );
    assert!(
        decl.iter().any(|(up, _)| up.contains("bytea")),
        "an encrypted column's physical type is BYTEA on both paths"
    );
}

#[test]
fn add_column_render_is_byte_identical_pg() {
    // addColumn: the declarative path renders it when a column is desired-but-
    // absent in live. We diff a one-column table against a live table that has
    // only the system fields, so the diff emits exactly the ADD COLUMN.
    let desc = CollectionDescriptor {
        name: "people".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor {
            name: "nickname".into(),
            ty: "string".into(),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[desc])
        .expect("desired snapshot");
    // Live = the SAME table with system fields but WITHOUT `nickname`.
    let live_desc = CollectionDescriptor {
        name: "people".into(),
        owner_app: OWNER.into(),
        fields: vec![],
        indexes: vec![],
    };
    let live_full = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[live_desc])
        .expect("live snapshot");
    let mut live_ownership = HashMap::new();
    live_ownership.insert("people".to_string(), OWNER.to_string());
    let author = DeclarativeAuthor::new(SCHEMA, OWNER);
    let plan = author
        .diff(&desired, &live_full.snapshot, &live_ownership, &[])
        .expect("declarative diff");
    let decl = sql_pairs(&plan.migrations);

    let ops = vec![Op::AddColumn {
        table: "people".into(),
        column: "nickname".into(),
        ty: ColType::String,
        nullable: None,
        default: None,
    }];
    let mut live = BTreeSet::new();
    live.insert("people".to_string());
    let ir = ir_pairs(ops, &live);

    assert_eq!(decl, ir, "addColumn render must be byte-identical across paths");
    assert!(decl.iter().any(|(up, _)| up.contains("ADD COLUMN")));
}

#[test]
fn create_index_render_is_byte_identical_pg() {
    // createIndex: a named index in the createTable's `indexes`. The declarative
    // path emits a CREATE INDEX; the IR path's createIndex op must match.
    let desc = CollectionDescriptor {
        name: "events".into(),
        owner_app: OWNER.into(),
        fields: vec![
            FieldDescriptor { name: "kind".into(), ty: "string".into(), ..Default::default() },
            FieldDescriptor { name: "at".into(), ty: "date".into(), ..Default::default() },
        ],
        indexes: vec![IndexDescriptor {
            name: "events_kind_at_idx".into(),
            columns: vec!["kind".into(), "at".into()],
            unique: false,
        }],
    };
    // Diff against a live table that already has the columns but not the index, so
    // the ONLY emitted op is the CREATE INDEX.
    let desired = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[desc.clone()])
        .expect("desired");
    let mut live_desc = desc.clone();
    live_desc.indexes = vec![];
    let live_full = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[live_desc])
        .expect("live");
    let mut live_ownership = HashMap::new();
    live_ownership.insert("events".to_string(), OWNER.to_string());
    let author = DeclarativeAuthor::new(SCHEMA, OWNER);
    let plan = author
        .diff(&desired, &live_full.snapshot, &live_ownership, &[])
        .expect("diff");
    let decl = sql_pairs(&plan.migrations);

    let ops = vec![Op::CreateIndex {
        table: "events".into(),
        columns: vec!["kind".into(), "at".into()],
        name: Some("events_kind_at_idx".into()),
        unique: None,
        using: None,
        r#where: None,
        concurrently: None,
    }];
    let mut live = BTreeSet::new();
    live.insert("events".to_string());
    let ir = ir_pairs(ops, &live);

    assert_eq!(decl, ir, "createIndex render must be byte-identical across paths");
    assert!(decl.iter().any(|(up, _)| up.contains("CREATE INDEX")));

    // Silence the unused-import lint for IrIndex (reserved for a future
    // createTable-with-inline-index parity case).
    let _ = std::any::type_name::<IrIndex>();
}

// ===========================================================================
// §6.4 SQLite leg — the SAME byte-identity gate on the SQLite dialect.
//
// The task mandates the cross-path byte-identity golden on BOTH PG and SQLite.
// The SQLite createTable routes through the SHARED `zeroship_schema::query`
// emitter (the same call the differ's `render_create_table_sqlite` makes), fed
// the SDK schema `Value` IrAuthor builds from the op descriptor via the same
// `descriptor_to_sdk_schema` bridge. So the SQLite leg is byte-identical BY
// CONSTRUCTION, exactly as the PG leg is.
// ===========================================================================

// ===========================================================================
// §6.4 stand-alone constraint + alterColumn* render coverage (§1260/§1270).
//
// The spec places stand-alone constraint + `alterColumn*` render coverage in
// PR1, with a cross-path parity golden for each. These ops are PG-native (SQLite
// reconciles them via the 12-step rebuild in the differ, which needs full live
// structure — out of this pure-render lower's scope); the SQLite leg of the
// stand-alone IR lower fails closed (asserted below).
// ===========================================================================

/// Lower a single IR op on the given dialect, returning the migration list.
fn ir_lower_one(op: Op, live: &BTreeSet<String>, dialect: SqlDialect) -> Vec<zeroship_migrate::Migration> {
    let ir = MigrationIr {
        ir_version: 1,
        name: "m".into(),
        owner_app: OWNER.into(),
        ops: vec![op],
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    };
    IrAuthor::new(SCHEMA, OWNER, dialect)
        .lower(&ir, live)
        .expect("ir lower")
}

#[test]
fn alter_column_type_render_is_byte_identical_pg() {
    use zeroship_migrate::ir::{ColType, Op};
    // The differ emits an `ALTER COLUMN … TYPE` when a same-name column's type
    // changed live→desired. Live `qty` is `int`; desired is `number` (double
    // precision). Diff that one-column change.
    let desired_desc = CollectionDescriptor {
        name: "widgets".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor { name: "qty".into(), ty: "number".into(), ..Default::default() }],
        indexes: vec![],
    };
    let live_desc = CollectionDescriptor {
        name: "widgets".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor { name: "qty".into(), ty: "int".into(), ..Default::default() }],
        indexes: vec![],
    };
    let desired = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[desired_desc]).expect("desired");
    let live = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[live_desc]).expect("live");
    let mut own = HashMap::new();
    own.insert("widgets".to_string(), OWNER.to_string());
    let plan = DeclarativeAuthor::new(SCHEMA, OWNER)
        .diff(&desired, &live.snapshot, &own, &[])
        .expect("diff");
    let decl: Vec<_> = sql_pairs(&plan.migrations)
        .into_iter()
        .filter(|(up, _)| up.contains("ALTER COLUMN"))
        .collect();

    let mut live_set = BTreeSet::new();
    live_set.insert("widgets".to_string());
    let ir = sql_pairs(&ir_lower_one(
        Op::AlterColumnType {
            table: "widgets".into(),
            column: "qty".into(),
            ty: ColType::Float,
            using: None,
        },
        &live_set,
        SqlDialect::Postgres,
    ));
    assert_eq!(decl, ir, "alterColumnType render must be byte-identical across paths");
    assert!(decl.iter().any(|(up, _)| up.contains("ALTER COLUMN \"qty\" TYPE")));
}

#[test]
fn alter_column_nullability_render_is_byte_identical_pg() {
    use zeroship_migrate::ir::Op;
    // SET NOT NULL: live `name` nullable, desired required. The differ emits
    // `ALTER COLUMN … SET NOT NULL`.
    let desired_desc = CollectionDescriptor {
        name: "people".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: true, ..Default::default() }],
        indexes: vec![],
    };
    let live_desc = CollectionDescriptor {
        name: "people".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor { name: "name".into(), ty: "string".into(), required: false, ..Default::default() }],
        indexes: vec![],
    };
    let desired = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[desired_desc]).expect("desired");
    let live = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[live_desc]).expect("live");
    let mut own = HashMap::new();
    own.insert("people".to_string(), OWNER.to_string());
    let plan = DeclarativeAuthor::new(SCHEMA, OWNER)
        .diff(&desired, &live.snapshot, &own, &[])
        .expect("diff");
    let decl: Vec<_> = sql_pairs(&plan.migrations)
        .into_iter()
        .filter(|(up, _)| up.contains("NOT NULL"))
        .collect();

    let mut live_set = BTreeSet::new();
    live_set.insert("people".to_string());
    let ir = sql_pairs(&ir_lower_one(
        Op::AlterColumnNullability { table: "people".into(), column: "name".into(), nullable: false },
        &live_set,
        SqlDialect::Postgres,
    ));
    assert_eq!(decl, ir, "alterColumnNullability (SET NOT NULL) render must be byte-identical");
    assert!(decl.iter().any(|(up, _)| up.contains("SET NOT NULL")));
}

#[test]
fn add_constraint_fk_render_is_byte_identical_pg() {
    use zeroship_migrate::ir::{IrConstraint, IrConstraintKind, Op};
    // A mutual-reference CYCLE (posts→authors, authors→posts) forces the differ
    // to DEFER the cycle-closing FK to a stand-alone `ALTER TABLE … ADD CONSTRAINT
    // … FOREIGN KEY` (it cannot inline both at CREATE). We compare the differ's
    // deferred FK for `posts.author → authors` to the equivalent stand-alone IR
    // addConstraint(fk) — two REAL render paths, byte-identical.
    let posts = CollectionDescriptor {
        name: "posts".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor {
            name: "author".into(),
            ty: "ref".into(),
            references: Some("authors".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let authors = CollectionDescriptor {
        name: "authors".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor {
            name: "pinned".into(),
            ty: "ref".into(),
            references: Some("posts".into()),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired = zeroship_migrate::declarative::desired_snapshot(SCHEMA, &[posts, authors]).expect("desired");
    let plan = DeclarativeAuthor::new(SCHEMA, OWNER)
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("diff");
    // The differ inlines `author_fkey` at the `posts` CREATE and DEFERS the
    // cycle-closing `pinned_fkey` (authors→posts) to a stand-alone ADD CONSTRAINT.
    // Isolate that deferred FK.
    let decl: Vec<_> = sql_pairs(&plan.migrations)
        .into_iter()
        .filter(|(up, _)| {
            up.contains("ADD CONSTRAINT \"pinned_fkey\"") && up.contains("FOREIGN KEY")
        })
        .collect();
    assert!(!decl.is_empty(), "the differ must defer the authors.pinned FK to a stand-alone ADD CONSTRAINT");

    // Stand-alone IR addConstraint(fk) on `authors.pinned` → posts(id) — the SAME
    // single-column FK shape the differ deferred.
    let mut live_set = BTreeSet::new();
    live_set.insert("posts".to_string());
    live_set.insert("authors".to_string());
    let ir = sql_pairs(&ir_lower_one(
        Op::AddConstraint {
            table: "authors".into(),
            constraint: IrConstraint {
                name: None,
                kind: IrConstraintKind::Fk {
                    columns: vec!["pinned".into()],
                    references_table: "posts".into(),
                    references_columns: vec!["id".into()],
                },
            },
        },
        &live_set,
        SqlDialect::Postgres,
    ));
    assert_eq!(decl, ir, "addConstraint(fk) render must be byte-identical to the differ's deferred FK");
    assert!(ir.iter().any(|(up, _)| up.contains("FOREIGN KEY")));
}

#[test]
fn add_constraint_unique_and_pk_and_drop_constraint_render_pg() {
    use zeroship_migrate::ir::{IrConstraint, IrConstraintKind, Op};
    // UNIQUE/PK have no stand-alone differ counterpart (the differ inlines them at
    // CREATE / renders single-col UNIQUE as an index), so these compare the IR
    // lower against the shared `lower_add_constraint` render seam directly — the
    // SAME render method, byte-stable. We pin the exact emitted SQL.
    let mut live = BTreeSet::new();
    live.insert("widgets".to_string());

    let uniq = sql_pairs(&ir_lower_one(
        Op::AddConstraint {
            table: "widgets".into(),
            constraint: IrConstraint {
                name: Some("widgets_slug_key".into()),
                kind: IrConstraintKind::Unique { columns: vec!["slug".into()] },
            },
        },
        &live,
        SqlDialect::Postgres,
    ));
    assert_eq!(
        uniq,
        vec![(
            "ALTER TABLE \"app\".\"widgets\" ADD CONSTRAINT \"widgets_slug_key\" UNIQUE (\"slug\")".to_string(),
            Some("ALTER TABLE \"app\".\"widgets\" DROP CONSTRAINT \"widgets_slug_key\"".to_string()),
        )],
        "stand-alone UNIQUE add renders the canonical ADD CONSTRAINT"
    );

    let pk = sql_pairs(&ir_lower_one(
        Op::AddConstraint {
            table: "widgets".into(),
            constraint: IrConstraint {
                name: Some("widgets_pkey".into()),
                kind: IrConstraintKind::Pk { columns: vec!["a".into(), "b".into()] },
            },
        },
        &live,
        SqlDialect::Postgres,
    ));
    assert_eq!(
        pk,
        vec![(
            "ALTER TABLE \"app\".\"widgets\" ADD CONSTRAINT \"widgets_pkey\" PRIMARY KEY (\"a\", \"b\")".to_string(),
            Some("ALTER TABLE \"app\".\"widgets\" DROP CONSTRAINT \"widgets_pkey\"".to_string()),
        )],
        "stand-alone PK add renders the canonical ADD CONSTRAINT"
    );

    let drop = sql_pairs(&ir_lower_one(
        Op::DropConstraint { table: "widgets".into(), name: "widgets_slug_key".into() },
        &live,
        SqlDialect::Postgres,
    ));
    assert_eq!(
        drop,
        vec![(
            "ALTER TABLE \"app\".\"widgets\" DROP CONSTRAINT \"widgets_slug_key\"".to_string(),
            None,
        )],
        "stand-alone DROP CONSTRAINT renders the canonical drop (down: None)"
    );
}

#[test]
fn standalone_alter_and_constraint_are_sqlite_rebuild_only() {
    use zeroship_migrate::ir::{ColType, IrConstraint, IrConstraintKind, Op};
    use zeroship_migrate::ir_author::IrLowerError;
    // SQLite has no native ALTER COLUMN / ADD|DROP CONSTRAINT — the stand-alone IR
    // lower fails closed (the differ reconciles these via the 12-step rebuild,
    // which is not this pure-render lower's path). Assert each op family.
    let mut live = BTreeSet::new();
    live.insert("widgets".to_string());
    let author = IrAuthor::new(SCHEMA, OWNER, SqlDialect::Sqlite);
    let one = |op: Op| {
        let ir = MigrationIr {
            ir_version: 1,
            name: "m".into(),
            owner_app: OWNER.into(),
            ops: vec![op],
            flags: Default::default(),
            depends_on: vec![],
            supersedes: vec![],
            preconditions: vec![],
            checksum: None,
        };
        author.lower(&ir, &live)
    };
    for (op, tag) in [
        (
            Op::AlterColumnType { table: "widgets".into(), column: "qty".into(), ty: ColType::Float, using: None },
            "alterColumnType",
        ),
        (
            Op::AlterColumnNullability { table: "widgets".into(), column: "qty".into(), nullable: false },
            "alterColumnNullability",
        ),
        (
            Op::AddConstraint {
                table: "widgets".into(),
                constraint: IrConstraint { name: None, kind: IrConstraintKind::Unique { columns: vec!["slug".into()] } },
            },
            "addConstraint",
        ),
        (Op::DropConstraint { table: "widgets".into(), name: "x".into() }, "dropConstraint"),
    ] {
        match one(op).unwrap_err() {
            IrLowerError::SqliteRebuildOnly(got) => assert_eq!(got, tag),
            other => panic!("expected SqliteRebuildOnly({tag}), got: {other}"),
        }
    }
}

#[test]
fn create_table_render_is_byte_identical_sqlite() {
    let desc = CollectionDescriptor {
        name: "widgets".into(),
        owner_app: OWNER.into(),
        fields: vec![
            FieldDescriptor {
                name: "title".into(),
                ty: "string".into(),
                required: true,
                ..Default::default()
            },
            FieldDescriptor {
                name: "slug".into(),
                ty: "string".into(),
                unique: true,
                ..Default::default()
            },
            FieldDescriptor { name: "qty".into(), ty: "int".into(), ..Default::default() },
        ],
        indexes: vec![],
    };
    let ops = vec![Op::CreateTable {
        name: "widgets".into(),
        columns: vec![
            IrColumn {
                name: "title".into(),
                ty: ColType::String,
                nullable: Some(false),
                default: None,
                unique: None,
            },
            IrColumn {
                name: "slug".into(),
                ty: ColType::String,
                nullable: None,
                default: None,
                unique: Some(true),
            },
            IrColumn {
                name: "qty".into(),
                ty: ColType::Int,
                nullable: None,
                default: None,
                unique: None,
            },
        ],
        constraints: vec![],
        indexes: vec![],
    }];

    let decl = declarative_pairs_for(&[desc], SqlDialect::Sqlite);
    let ir = ir_pairs_for(ops, &BTreeSet::new(), SqlDialect::Sqlite);
    assert_eq!(
        decl, ir,
        "createTable render must be byte-identical across paths on SQLite"
    );
    assert!(
        decl.iter().any(|(up, _)| up.contains("CREATE TABLE")),
        "the SQLite CREATE TABLE must be emitted on both paths"
    );
    assert!(
        decl.iter().any(|(up, _)| up.contains("CREATE UNIQUE INDEX")),
        "the unique index must be emitted on both paths (SQLite)"
    );
}

#[test]
fn create_table_with_encrypted_column_render_is_byte_identical_sqlite() {
    let desc = CollectionDescriptor {
        name: "vault".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor {
            name: "secret".into(),
            ty: "string".into(),
            encrypted: Some(serde_json::json!({})),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let ops = vec![Op::CreateTable {
        name: "vault".into(),
        columns: vec![IrColumn {
            name: "secret".into(),
            ty: ColType::Encrypted { of: Box::new(ColType::String) },
            nullable: None,
            default: None,
            unique: None,
        }],
        constraints: vec![],
        indexes: vec![],
    }];
    let decl = declarative_pairs_for(&[desc], SqlDialect::Sqlite);
    let ir = ir_pairs_for(ops, &BTreeSet::new(), SqlDialect::Sqlite);
    assert_eq!(
        decl, ir,
        "encrypted-column createTable render must be byte-identical across paths on SQLite"
    );
    assert!(
        decl.iter().any(|(up, _)| up.contains("zsenc:")),
        "the encryption sentinel must be emitted on the SQLite leg too (shared-kernel source)"
    );
}

#[test]
fn add_column_render_is_byte_identical_sqlite() {
    let desc = CollectionDescriptor {
        name: "people".into(),
        owner_app: OWNER.into(),
        fields: vec![FieldDescriptor {
            name: "nickname".into(),
            ty: "string".into(),
            ..Default::default()
        }],
        indexes: vec![],
    };
    let desired =
        zeroship_migrate::declarative::desired_snapshot_for_dialect(SCHEMA, &[desc], SqlDialect::Sqlite)
            .expect("desired snapshot");
    let live_desc = CollectionDescriptor {
        name: "people".into(),
        owner_app: OWNER.into(),
        fields: vec![],
        indexes: vec![],
    };
    let live_full =
        zeroship_migrate::declarative::desired_snapshot_for_dialect(SCHEMA, &[live_desc], SqlDialect::Sqlite)
            .expect("live snapshot");
    let mut live_ownership = HashMap::new();
    live_ownership.insert("people".to_string(), OWNER.to_string());
    let author = DeclarativeAuthor::new_for_dialect(SCHEMA, OWNER, SqlDialect::Sqlite);
    let plan = author
        .diff(&desired, &live_full.snapshot, &live_ownership, &[])
        .expect("declarative diff");
    let decl = sql_pairs(&plan.migrations);

    let ops = vec![Op::AddColumn {
        table: "people".into(),
        column: "nickname".into(),
        ty: ColType::String,
        nullable: None,
        default: None,
    }];
    let mut live = BTreeSet::new();
    live.insert("people".to_string());
    let ir = ir_pairs_for(ops, &live, SqlDialect::Sqlite);

    assert_eq!(decl, ir, "addColumn render must be byte-identical across paths on SQLite");
    assert!(decl.iter().any(|(up, _)| up.contains("ADD COLUMN")));
}

#[test]
fn create_index_render_is_byte_identical_sqlite() {
    let desc = CollectionDescriptor {
        name: "events".into(),
        owner_app: OWNER.into(),
        fields: vec![
            FieldDescriptor { name: "kind".into(), ty: "string".into(), ..Default::default() },
            FieldDescriptor { name: "at".into(), ty: "date".into(), ..Default::default() },
        ],
        indexes: vec![IndexDescriptor {
            name: "events_kind_at_idx".into(),
            columns: vec!["kind".into(), "at".into()],
            unique: false,
        }],
    };
    let desired =
        zeroship_migrate::declarative::desired_snapshot_for_dialect(SCHEMA, &[desc.clone()], SqlDialect::Sqlite)
            .expect("desired");
    let mut live_desc = desc.clone();
    live_desc.indexes = vec![];
    let live_full =
        zeroship_migrate::declarative::desired_snapshot_for_dialect(SCHEMA, &[live_desc], SqlDialect::Sqlite)
            .expect("live");
    let mut live_ownership = HashMap::new();
    live_ownership.insert("events".to_string(), OWNER.to_string());
    let author = DeclarativeAuthor::new_for_dialect(SCHEMA, OWNER, SqlDialect::Sqlite);
    let plan = author
        .diff(&desired, &live_full.snapshot, &live_ownership, &[])
        .expect("diff");
    let decl = sql_pairs(&plan.migrations);

    let ops = vec![Op::CreateIndex {
        table: "events".into(),
        columns: vec!["kind".into(), "at".into()],
        name: Some("events_kind_at_idx".into()),
        unique: None,
        using: None,
        r#where: None,
        concurrently: None,
    }];
    let mut live = BTreeSet::new();
    live.insert("events".to_string());
    let ir = ir_pairs_for(ops, &live, SqlDialect::Sqlite);

    assert_eq!(decl, ir, "createIndex render must be byte-identical across paths on SQLite");
    assert!(decl.iter().any(|(up, _)| up.contains("CREATE INDEX")));
}
