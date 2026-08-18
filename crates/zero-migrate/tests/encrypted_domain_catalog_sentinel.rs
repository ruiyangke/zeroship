//! **An encrypted DOMAIN column's catalog sentinel must name the domain's BASE type.**
//!
//! A `t.encrypted({ of: t.domain("positive_number") })` column stores ciphertext in
//! `BYTEA`/`BLOB`, and the ONLY record of what the plaintext is shaped like is the
//! `zero-migrate:enc:<mode>:<keyId>:<wraps>` sentinel the lower stamps into the catalog:
//!
//! ```text
//!   postgres  "amount" bytea /* zero-migrate:enc:randomised:default:string */ NOT NULL
//!             COMMENT ON COLUMN "app"."amounts"."amount" IS 'zero-migrate:enc:…:string'
//!   sqlite    "amount" BLOB  /* zero-migrate:enc:randomised:default:string */ NOT NULL
//! ```
//!
//! `string` — for a domain over `int`. `wraps` is not decoration: it selects which
//! type-checker validates the plaintext before the encrypt pass swaps bytes in
//! (`schema::diff::EncryptionMeta::wraps`), so every write to that column ran the text
//! validator over an integer, and every read decoded it back as text.
//!
//! WHY THIS IS NOT THE SAME DEFECT THE DOMAIN TOKEN FIX CLOSED. That fix resolved the
//! RUNTIME descriptor and deliberately excluded encrypted columns, because `wraps` has a
//! SECOND producer — the DDL lower — and resolving one side alone would have made the
//! runtime say `number` while the catalog still said `string`. Both producers now resolve
//! through the one shared `resolve_domain_base_type` walk, so the two agree.
//!
//! WHAT IS ASSERTED. Content, never `ok`: the pre-fix answer was `ok=true` on all three
//! dialects, which is exactly how it shipped. The controls carry the weight — an
//! encrypted column over a plain `int` proves the sentinel was already right when the
//! type is NOT behind a domain, an encrypted column over `text` proves `string` is still
//! reachable (so the fix resolves rather than hardcodes `number`), and the strongest
//! form asserts the domain column's DDL is BYTE-IDENTICAL to its resolved base's.

mod support;

use std::collections::BTreeSet;

use serde_json::json;
use zero_migrate::model::ir::{MigrationIr, Op};
use zero_migrate::render::lower::IrAuthor;
use zero_migrate::schema::query::SqlDialect;
use zero_migrate::{fold_ops, fold_to_field_defs, resolve_create_table_policy, LiveSchema};

const SCHEMA: &str = "app";
const OWNER: &str = "app_test";

/// The dialects an encrypted domain column is expressible on. MySQL is included and
/// measured separately: it emits NO `zero-migrate:enc:` sentinel at all (see
/// [`mysql_emits_no_encryption_sentinel_to_disagree_with`]), so there is nothing for the
/// runtime descriptor to disagree with there.
const SENTINEL_DIALECTS: [(&str, SqlDialect); 2] = [
    ("postgres", SqlDialect::Postgres),
    ("sqlite", SqlDialect::Sqlite),
];

const ALL_DIALECTS: [(&str, SqlDialect); 3] = [
    ("postgres", SqlDialect::Postgres),
    ("sqlite", SqlDialect::Sqlite),
    ("mysql", SqlDialect::Mysql),
];

/// `createTable` with one encrypted column whose inner type is `inner`, preceded by a
/// `createDomain positive_number AS <base>`.
fn create_ops(base: serde_json::Value, inner: serde_json::Value) -> Vec<Op> {
    serde_json::from_value(json!([
        { "op": "createDomain", "name": "positive_number", "as": base },
        {
            "op": "createTable",
            "name": "amounts",
            "columns": [
                { "name": "amount", "type": { "encrypted": { "of": inner } }, "nullable": false },
            ],
            "primaryKey": null,
        },
    ]))
    .expect("create ops deserialize")
}

/// The same shape reached through `addColumn` instead of `createTable` — the second
/// place the lower stamps this sentinel.
fn add_column_ops(base: serde_json::Value, inner: serde_json::Value) -> Vec<Op> {
    serde_json::from_value(json!([
        { "op": "createDomain", "name": "positive_number", "as": base },
        {
            "op": "createTable",
            "name": "amounts",
            "columns": [{ "name": "note", "type": "text", "nullable": false }],
            "primaryKey": null,
        },
        {
            "op": "addColumn",
            "table": "amounts",
            "column": "amount",
            "type": { "encrypted": { "of": inner } },
            "nullable": true,
        },
    ]))
    .expect("add ops deserialize")
}

/// Render ops through the REAL lower and return every emitted `up` statement joined.
fn rendered_sql(ops: Vec<Op>, dialect: SqlDialect) -> String {
    let ir = MigrationIr {
        inverse_ops: None,
        irreversible: None,
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
    let ir = resolve_create_table_policy(&ir, &support::confined_charter(), SCHEMA)
        .expect("IR resolves against the test charter");
    let author = IrAuthor::new(SCHEMA, OWNER, dialect, &support::confined_charter());
    let migs = author
        .lower(&ir, &LiveSchema::from(&BTreeSet::new()))
        .expect("ir lower");
    migs.iter()
        .map(|m| m.up.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `zero-migrate:enc:` sentinel bodies present in some SQL, in order.
fn enc_sentinels(sql: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = sql;
    while let Some(at) = rest.find("zero-migrate:enc:") {
        let tail = &rest[at..];
        let end = tail
            .find([' ', '*', '\'', ';'])
            .unwrap_or(tail.len());
        found.push(tail[..end].to_string());
        rest = &tail[end..];
    }
    found
}

/// THE DEFECT, on every dialect that carries the sentinel at all.
#[test]
fn an_encrypted_domain_columns_catalog_sentinel_names_the_base_type() {
    for (label, dialect) in SENTINEL_DIALECTS {
        let sql = rendered_sql(
            create_ops(
                json!("int"),
                json!({ "domain": { "name": "positive_number" } }),
            ),
            dialect,
        );
        let sentinels = enc_sentinels(&sql);
        assert!(
            !sentinels.is_empty(),
            "{label}: an encrypted column must carry an enc sentinel at all:\n{sql}"
        );
        for sentinel in &sentinels {
            assert_eq!(
                sentinel, "zero-migrate:enc:randomised:default:number",
                "{label}: the sentinel must name the DOMAIN's base type, not \"string\":\n{sql}"
            );
        }
    }
}

/// The same column reached through `addColumn`, which stamps the sentinel from a
/// different lower arm.
#[test]
fn an_added_encrypted_domain_columns_sentinel_names_the_base_type() {
    for (label, dialect) in SENTINEL_DIALECTS {
        let sql = rendered_sql(
            add_column_ops(
                json!("int"),
                json!({ "domain": { "name": "positive_number" } }),
            ),
            dialect,
        );
        let sentinels = enc_sentinels(&sql);
        assert!(
            !sentinels.is_empty(),
            "{label}: an added encrypted column must carry an enc sentinel:\n{sql}"
        );
        for sentinel in &sentinels {
            assert_eq!(
                sentinel, "zero-migrate:enc:randomised:default:number",
                "{label}: an ADD COLUMN sentinel must resolve the domain too:\n{sql}"
            );
        }
    }
}

/// The strongest form: an encrypted column over a domain whose base is `int` renders
/// BYTE-IDENTICALLY to an encrypted column over a plain `int`.
///
/// This is what "the sentinel is right" actually means — not that it contains a
/// substring, but that the column is indistinguishable from the resolved type it stands
/// for. It also proves the resolution changes NOTHING ELSE about the DDL: the physical
/// type, the masked sibling, and the system columns all have to match too.
#[test]
fn an_encrypted_domain_column_renders_identically_to_its_resolved_base() {
    for (label, dialect) in ALL_DIALECTS {
        let via_domain = rendered_sql(
            create_ops(
                json!("int"),
                json!({ "domain": { "name": "positive_number" } }),
            ),
            dialect,
        );
        // The control declares the same domain (so the `CREATE DOMAIN` DDL, which only
        // PostgreSQL emits, is present in both) but types the column directly.
        let via_base = rendered_sql(create_ops(json!("int"), json!("int")), dialect);
        assert_eq!(
            via_domain, via_base,
            "{label}: an encrypted domain column must render exactly as its base type does"
        );
    }
}

/// The control that proves the fix RESOLVES rather than hardcoding `number`: an
/// encrypted column over a domain whose base is textual still says `string`.
#[test]
fn an_encrypted_domain_over_a_text_base_still_says_string() {
    for (label, dialect) in SENTINEL_DIALECTS {
        let sql = rendered_sql(
            create_ops(
                json!({ "string": { "length": 40 } }),
                json!({ "domain": { "name": "positive_number" } }),
            ),
            dialect,
        );
        for sentinel in enc_sentinels(&sql) {
            assert_eq!(
                sentinel, "zero-migrate:enc:randomised:default:string",
                "{label}: a domain over varchar must still wrap a string:\n{sql}"
            );
        }
    }
}

/// A domain that was never declared is unresolvable, and an unresolvable name leaves the
/// sentinel exactly as it was rather than inventing a base type.
#[test]
fn an_undeclared_domain_leaves_the_sentinel_unchanged() {
    for (label, dialect) in SENTINEL_DIALECTS {
        let ops: Vec<Op> = serde_json::from_value(json!([{
            "op": "createTable",
            "name": "amounts",
            "columns": [{
                "name": "amount",
                "type": { "encrypted": { "of": { "domain": { "name": "never_declared" } } } },
                "nullable": false,
            }],
            "primaryKey": null,
        }]))
        .expect("orphan ops deserialize");
        let sql = rendered_sql(ops, dialect);
        for sentinel in enc_sentinels(&sql) {
            assert_eq!(
                sentinel, "zero-migrate:enc:randomised:default:string",
                "{label}: an undeclared domain must leave the sentinel alone:\n{sql}"
            );
        }
    }
}

/// MySQL emits NO `zero-migrate:enc:` sentinel at all — for a domain column AND for a
/// plain `int` one.
///
/// Recorded because it changes what this defect means per dialect: on MySQL there is no
/// catalog record of `wraps` for the runtime descriptor to disagree with, so the fix is
/// a no-op on the DDL there. Pinned so that a later change which STARTS emitting a MySQL
/// sentinel has to come here and decide what it should say, rather than silently
/// inheriting whichever answer the column happened to carry.
#[test]
fn mysql_emits_no_encryption_sentinel_to_disagree_with() {
    for inner in [
        json!({ "domain": { "name": "positive_number" } }),
        json!("int"),
    ] {
        let sql = rendered_sql(create_ops(json!("int"), inner.clone()), SqlDialect::Mysql);
        assert!(
            enc_sentinels(&sql).is_empty(),
            "mysql emits no enc sentinel today ({inner}):\n{sql}"
        );
    }
}

/// THE EXACT TRIGGER THIS DEFECT WAS FOUND THROUGH: an UNRELATED column rename.
///
/// On SQLite a rename is a 12-step table rebuild, and the rebuilt `CREATE TABLE` is
/// re-rendered from the FOLDED snapshot rather than from the original DDL. That is how a
/// half-fix showed itself: with only the runtime side resolving, renaming a different
/// column rewrote the live table as `…:number` while the comment the introspector reads
/// still said `…:string`, silently changing a deployed column's recorded encryption
/// posture.
///
/// The rebuild's source is the snapshot's own `encryption_sentinel`, so this asserts the
/// rename carries the encrypted column through with the SAME sentinel the original
/// `CREATE` stamped — no drift across the rebuild seam, in either direction.
#[test]
fn an_unrelated_rename_carries_the_encrypted_domain_columns_sentinel_unchanged() {
    for (label, dialect) in ALL_DIALECTS {
        // A second, ordinary column so the rename targets something that is genuinely
        // unrelated to the encrypted one.
        let mut ops: Vec<Op> = serde_json::from_value(json!([
            { "op": "createDomain", "name": "positive_number", "as": "int" },
            {
                "op": "createTable",
                "name": "amounts",
                "columns": [
                    {
                        "name": "amount",
                        "type": { "encrypted": { "of": { "domain": { "name": "positive_number" } } } },
                        "nullable": false,
                    },
                    { "name": "note", "type": "text", "nullable": false },
                ],
                "primaryKey": null,
            },
        ]))
        .expect("create ops deserialize");
        let created = fold_ops(&ops, dialect, SCHEMA, &support::confined_charter())
            .expect("snapshot fold of the create succeeds");
        let before = created
            .tables
            .get("amounts")
            .and_then(|t| t.columns.iter().find(|c| c.name == "amount"))
            .and_then(|c| {
                c.encryption_sentinel
                    .clone()
                    .or_else(|| c.comment_sentinel.clone())
            });

        // Rename a DIFFERENT column. On SQLite this is the 12-step rebuild.
        ops.push(
            serde_json::from_value(json!({
                "op": "renameColumn",
                "table": "amounts",
                "from": "note",
                "to": "memo",
                "type": "text",
            }))
            .expect("rename op deserializes"),
        );
        let renamed = fold_ops(&ops, dialect, SCHEMA, &support::confined_charter())
            .expect("snapshot fold of the rename succeeds");
        let after = renamed
            .tables
            .get("amounts")
            .and_then(|t| t.columns.iter().find(|c| c.name == "amount"))
            .and_then(|c| {
                c.encryption_sentinel
                    .clone()
                    .or_else(|| c.comment_sentinel.clone())
            });

        assert_eq!(
            before, after,
            "{label}: an unrelated rename must not rewrite the encrypted column's sentinel"
        );
        if let Some(sentinel) = after {
            if sentinel.contains("zero-migrate:enc:") {
                assert!(
                    sentinel.contains("randomised:default:number"),
                    "{label}: and it must be the resolved base type: {sentinel:?}"
                );
            }
        }
    }
}

/// THE REPLAY-AGREEMENT CHECK.
///
/// The sentinel has three producers that must not drift: the DDL lower
/// (`IrAuthor::lower`), the snapshot fold (`fold_ops`, which SEEDS the SQLite 12-step
/// rebuild), and the field-def replay (`fold_to_field_defs`, which produces the runtime
/// descriptor). The previous defect was precisely that the third disagreed with the
/// first two. All three are asserted here, in one test, on the same ops.
#[test]
fn the_lower_the_snapshot_fold_and_the_field_defs_agree_on_wraps() {
    for (label, dialect) in ALL_DIALECTS {
        let ops = create_ops(
            json!("int"),
            json!({ "domain": { "name": "positive_number" } }),
        );

        // 1. The DDL lower.
        let sql = rendered_sql(ops.clone(), dialect);
        for sentinel in enc_sentinels(&sql) {
            assert_eq!(
                sentinel, "zero-migrate:enc:randomised:default:number",
                "{label}: lower:\n{sql}"
            );
        }

        // 2. The snapshot fold — the source the SQLite rebuild re-renders from.
        let snap = fold_ops(&ops, dialect, SCHEMA, &support::confined_charter())
            .expect("snapshot fold succeeds");
        let table = snap.tables.get("amounts").expect("amounts in the snapshot");
        let amount = table
            .columns
            .iter()
            .find(|c| c.name == "amount")
            .expect("amount column in the snapshot");
        for sentinel in amount
            .encryption_sentinel
            .iter()
            .chain(amount.comment_sentinel.iter())
            .filter(|s| s.contains("zero-migrate:enc:"))
        {
            assert!(
                sentinel.contains("randomised:default:number"),
                "{label}: snapshot fold disagrees with the lower: {sentinel:?}"
            );
        }

        // 3. The field-def replay — the runtime descriptor.
        let defs = fold_to_field_defs(&ops, dialect, SCHEMA, &support::confined_charter())
            .expect("field-def fold succeeds");
        let amounts = defs.get("amounts").expect("amounts in the field defs");
        assert_eq!(
            amounts["amount"]["encrypted"]["wraps"], "number",
            "{label}: field-def replay disagrees: {amounts}"
        );
        assert_eq!(
            amounts["amount"]["type"], "int",
            "{label}: and the token must move with it: {amounts}"
        );
    }
}
