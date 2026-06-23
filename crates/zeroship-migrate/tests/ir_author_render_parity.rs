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
/// empty live schema → the emitted migrations.
fn declarative_pairs(descs: &[CollectionDescriptor]) -> Vec<(String, Option<String>)> {
    let desired: DesiredSchema =
        zeroship_migrate::declarative::desired_snapshot(SCHEMA, descs).expect("desired snapshot");
    let author = DeclarativeAuthor::new(SCHEMA, OWNER);
    let plan = author
        .diff(&desired, &SchemaSnapshot::default(), &HashMap::new(), &[])
        .expect("declarative diff");
    sql_pairs(&plan.migrations)
}

/// Run the IR path: ops → `IrAuthor::lower` against the given live tables.
fn ir_pairs(ops: Vec<Op>, live: &BTreeSet<String>) -> Vec<(String, Option<String>)> {
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
    let author = IrAuthor::new(SCHEMA, OWNER, SqlDialect::Postgres);
    let migs = author.lower(&ir, live).expect("ir lower");
    sql_pairs(&migs)
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
