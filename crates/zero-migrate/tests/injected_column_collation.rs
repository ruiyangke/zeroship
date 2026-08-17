//! A charter-injected system column can pin a BYTEWISE collation, so `ORDER BY id`
//! stays creation order on a database whose default collation is not `C`.
//!
//! The subject is ORDERING, not DDL spelling. A test that only greps the emitted
//! statement for `COLLATE "C"` measures how the engine spells itself; the defect is
//! that rows come back in the wrong order. So the PostgreSQL leg inserts ids whose
//! BYTE order is known, reads them back with `ORDER BY id`, and compares sequences.
//!
//! [`injected_id_without_a_pinned_collation_loses_creation_order`] is the permanent
//! neuter check: it pins that the SAME fixture with the collation removed from the
//! charter still comes back WRONG. Without it a `C`-collated test database would let
//! the fixed test pass while proving nothing, and that is exactly the failure mode
//! this issue is about.

mod support;

use zero_migrate::driver::SqlSession;
use zero_migrate::model::ir::{MigrationIr, CURRENT_IR_VERSION};
use zero_migrate::{
    effective_policy_from_charter_toml, EffectivePolicy, IrAuthor, LiveSchema, SqlDialect,
};

const OWNER: &str = "app_injected_collation";

/// Four ids in the consumer's shape - a prefix plus base62 - listed in CREATION
/// order, which for base62 of a monotonic UUIDv7 is BYTE order.
///
/// The four suffixes vary only in the case runs `A`, `Z`, `a`, `z`, whose byte
/// values ascend (0x41 < 0x5A < 0x61 < 0x7A). Under `en_US.utf8` PostgreSQL
/// interleaves those runs, which is the whole defect; under `C` it does not.
const CREATION_ORDER: [&str; 4] = [
    "note_0000000000000000000AAA",
    "note_0000000000000000000Zzz",
    "note_0000000000000000000aaa",
    "note_0000000000000000000zzz",
];

/// The consumer's charter shape: a mandatory inject pinning the system columns,
/// optionally pinning a collation on the three text ones.
fn charter(collation: Option<&str>) -> EffectivePolicy {
    let pin = collation.map_or_else(String::new, |token| format!(", collation = {token:?}"));
    let toml = format!(
        r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[grant]]
key = "safety.destructive_ops"
value = "allow"
scope = "all"

[[inject]]
scope = "all"
mandatory = true
primary_key = ["id"]
author_primary_key = "forbid"
columns = [
  {{ name = "id",         type = "text",        nullable = false{pin} }},
  {{ name = "created_at", type = "timestamptz", nullable = false }},
  {{ name = "created_by", type = "text",        nullable = true{pin} }},
  {{ name = "version",    type = "integer",     nullable = false }},
]
"#
    );
    effective_policy_from_charter_toml(&toml).expect("injected-collation charter composes")
}

fn authored_ir(table: &str) -> MigrationIr {
    serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": format!("create_{table}"),
        "owner_app": OWNER,
        "ops": [{
            "op": "createTable",
            "name": table,
            "columns": [{ "name": "title", "type": "text", "nullable": false }],
            "primaryKey": null,
            "indexes": []
        }]
    }))
    .expect("authored create-table IR deserializes")
}

/// Resolve the charter's injection onto the authored create and lower it. This is
/// the production path: `resolve_create_table_policy` is what the recorder and the
/// descriptor fold both call before anything is lowered.
fn injected_ddl(dialect: SqlDialect, schema: &str, table: &str, collation: Option<&str>) -> String {
    let policy = charter(collation);
    let resolved = zero_migrate::resolve_create_table_policy(&authored_ir(table), &policy, schema)
        .expect("the charter's injection resolves");
    let migrations = IrAuthor::new(schema, OWNER, dialect, &policy)
        .lower(&resolved, &LiveSchema::default())
        .expect("the resolved create lowers");
    migrations
        .into_iter()
        .map(|migration| migration.up)
        .collect::<Vec<_>>()
        .join(";\n")
}

// ── the emitted DDL, per dialect ────────────────────────────────────────────────
//
// These pin the SPELLING, which is not the defect but is the thing the ordering
// legs below depend on. They also hold the dialect boundary: the PostgreSQL
// collation must not leak into a SQLite or MySQL statement.

#[test]
fn a_pinned_bytewise_collation_is_spelled_per_dialect() {
    let pg = injected_ddl(SqlDialect::Postgres, "app", "notes", Some("bytewise"));
    assert!(
        pg.contains(r#""id" character varying(255) COLLATE "C""#),
        "PostgreSQL must pin C on the injected id; SQL was:\n{pg}"
    );
    assert!(
        pg.contains(r#""created_by" character varying(255) COLLATE "C""#),
        "PostgreSQL must pin C on every column the charter names; SQL was:\n{pg}"
    );
    assert!(
        !pg.contains(r#""created_at" timestamptz COLLATE"#),
        "a column the charter did not name must be untouched; SQL was:\n{pg}"
    );

    let sqlite = injected_ddl(SqlDialect::Sqlite, "app", "notes", Some("bytewise"));
    assert!(
        sqlite.contains(r#""id" TEXT COLLATE BINARY"#),
        "SQLite must spell the same intent as BINARY; SQL was:\n{sqlite}"
    );
    assert!(
        !sqlite.contains(r#"COLLATE "C""#),
        "the PostgreSQL collation must never reach SQLite; SQL was:\n{sqlite}"
    );

    // `utf8mb4_0900_bin`, not the legacy `utf8mb4_bin`: the legacy one is PAD SPACE,
    // so `'x' = 'x '` there, which is a real hazard on a primary key. The 0900 family
    // is NO PAD and compares the encoded bytes, which is what `C` and BINARY mean.
    // And utf8mb4 rather than the value-format path's `CHARACTER SET ascii`: that
    // path earns ascii from a CHECK proving the content is ascii, which an inject
    // facet has no equivalent of - `created_by` may hold a non-ascii name, and an
    // ascii column would REJECT it. Trading a silent ordering bug for a loud
    // insert failure is not a fix.
    let mysql = injected_ddl(SqlDialect::Mysql, "app", "notes", Some("bytewise"));
    assert!(
        mysql.contains("`id` VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin"),
        "MySQL must spell the same intent as utf8mb4_0900_bin; SQL was:\n{mysql}"
    );
    assert!(
        !mysql.contains(r#"COLLATE "C""#),
        "the PostgreSQL collation must never reach MySQL; SQL was:\n{mysql}"
    );
}

#[test]
fn an_unpinned_injected_column_emits_exactly_what_it_emitted_before() {
    // The facet is opt-in. A charter that does not name it must emit the same DDL
    // it emitted before the facet existed, on every dialect.
    let pg = injected_ddl(SqlDialect::Postgres, "app", "notes", None);
    assert!(
        !pg.contains("COLLATE"),
        "an unpinned charter must emit no collation on PostgreSQL; SQL was:\n{pg}"
    );
    let sqlite = injected_ddl(SqlDialect::Sqlite, "app", "notes", None);
    assert!(
        !sqlite.contains("COLLATE"),
        "an unpinned charter must emit no collation on SQLite; SQL was:\n{sqlite}"
    );
    // MySQL is the exception, and not because of this facet: every character column
    // there already carries an explicit `utf8mb4_0900_as_cs` so equality matches the
    // other two dialects. What must not appear is the bytewise one.
    let mysql = injected_ddl(SqlDialect::Mysql, "app", "notes", None);
    assert!(
        mysql.contains("`id` VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_as_cs"),
        "an unpinned MySQL charter must keep the case-sensitive default; SQL was:\n{mysql}"
    );
    assert!(
        !mysql.contains("_bin"),
        "an unpinned charter must not reach a binary collation; SQL was:\n{mysql}"
    );
}

// ── SQLite: it orders correctly today, and must still ───────────────────────────

#[test]
fn sqlite_orders_an_injected_id_by_creation_with_and_without_the_pin() {
    for (label, collation) in [("unpinned", None), ("pinned", Some("bytewise"))] {
        let conn = rusqlite::Connection::open_in_memory().expect("open SQLite");
        conn.execute_batch(&injected_ddl(SqlDialect::Sqlite, "app", "notes", collation))
            .unwrap_or_else(|error| panic!("apply the {label} SQLite fixture: {error}"));
        for (index, id) in CREATION_ORDER.iter().enumerate() {
            conn.execute(
                "INSERT INTO notes(id, created_at, created_by, version, title) \
                 VALUES (?1, '2026-08-17T00:00:00Z', 'someone', ?2, 'title')",
                rusqlite::params![id, index as i64],
            )
            .unwrap_or_else(|error| panic!("insert {id} into the {label} fixture: {error}"));
        }
        let mut statement = conn
            .prepare("SELECT id FROM notes ORDER BY id")
            .expect("prepare the ordered read");
        let read: Vec<String> = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("read the ordered ids")
            .collect::<Result<_, _>>()
            .expect("every id decodes");
        assert_eq!(
            read,
            CREATION_ORDER.to_vec(),
            "SQLite's BINARY default must keep creation order ({label})"
        );
    }
}

// ── the charter surface: authorable, closed, and part of the policy's identity ──

#[test]
fn an_unknown_collation_token_is_refused_at_charter_load() {
    // At LOAD, with the TOML line, is where an operator can still fix it. The
    // alternative - carrying an unrecognised string to the render seam - is a
    // charter that reads as pinning something and pins nothing.
    for token in ["en_US.utf8", "C", "binary", "BYTEWISE"] {
        let toml = format!(
            r#"policy_version = 1

[[inject]]
scope = "all"
columns = [
  {{ name = "id", type = "text", nullable = false, collation = {token:?} }},
]
"#
        );
        let error = effective_policy_from_charter_toml(&toml)
            .unwrap_err_or_panic(&format!("collation = {token:?} must not load"));
        assert!(
            error.contains("collation"),
            "the load error must name the field; got: {error}"
        );
    }
}

/// `Result::unwrap_err` needs `Debug` on the Ok side; the composed policy has none
/// worth printing, so spell the failure here instead.
trait UnwrapErrOrPanic {
    fn unwrap_err_or_panic(self, message: &str) -> String;
}

impl<T, E: std::fmt::Display> UnwrapErrOrPanic for Result<T, E> {
    fn unwrap_err_or_panic(self, message: &str) -> String {
        match self {
            Ok(_) => panic!("{message}"),
            Err(error) => format!("{error}"),
        }
    }
}

#[test]
fn a_pinned_collation_is_part_of_the_policy_identity_the_seal_covers() {
    // A seal that cannot tell two policies apart is not tamper evidence. These two
    // charters differ ONLY in the collation, and the DDL they resolve to differs, so
    // a seal minted over one must NOT verify against the other.
    const MAC_KEY: &[u8] = b"an-injected-collation-test-mac-key";
    const DIALECT: &str = "postgres";
    const MATCHER: u32 = 1;
    const CHARTER_VERSION: u64 = 1;

    let plain = charter(None);
    let pinned = charter(Some("bytewise"));
    let digest = plain.registry().digest();
    let sealed = zero_migrate::seal::seal(
        &plain,
        MAC_KEY,
        [0u8; 16],
        DIALECT,
        MATCHER,
        CHARTER_VERSION,
    );
    sealed
        .verify(MAC_KEY, &plain, &digest, DIALECT, MATCHER, CHARTER_VERSION)
        .expect("a seal verifies against the policy it was minted over");
    assert_eq!(
        sealed.verify(MAC_KEY, &pinned, &digest, DIALECT, MATCHER, CHARTER_VERSION),
        Err(zero_migrate::SealError::TagMismatch),
        "a charter that adds an injected column's collation must not verify under the \
         seal minted before it"
    );
}

#[test]
fn an_author_column_that_matches_every_field_but_the_collation_is_not_the_injected_shape() {
    // II.2.6b conformance. The author declares the `id` slot with the injected
    // type, nullability and default but WITHOUT the pinned collation. Accepting
    // that as conforming would leave the author's column verbatim - operator-shaped
    // on paper, author-ordered in the database - which is the exact silent
    // divergence this facet exists to end.
    let ir: MigrationIr = serde_json::from_value(serde_json::json!({
        "ir_version": CURRENT_IR_VERSION,
        "name": "create_notes",
        "owner_app": OWNER,
        "ops": [{
            "op": "createTable",
            "name": "notes",
            "columns": [
                { "name": "id", "type": { "string": { "length": 255 } }, "nullable": false },
                { "name": "created_at", "type": "timestamp", "nullable": false },
                { "name": "created_by", "type": { "string": { "length": 255 } }, "nullable": true },
                { "name": "version", "type": "int", "nullable": false }
            ],
            "primaryKey": ["id"],
            "indexes": []
        }]
    }))
    .expect("the author's near-miss create deserializes");
    let error = zero_migrate::resolve_create_table_policy(&ir, &charter(Some("bytewise")), "app")
        .expect_err("a column short of the pinned collation must not pass as conforming");
    assert!(
        format!("{error}").contains("id"),
        "the refusal must name the colliding column; got: {error}"
    );

    // The control: the SAME create against a charter pinning no collation IS the
    // injected shape, so the check did not simply become a blanket refusal.
    zero_migrate::resolve_create_table_policy(&ir, &charter(None), "app")
        .expect("the same create conforms to the charter that pins no collation");
}

#[test]
fn a_collation_pinned_on_a_non_text_system_column_is_refused() {
    // A charter that pins a collation on `timestamptz` is asking for something no
    // dialect can hear. Dropping it silently is the defect class this whole facet
    // is about, so the resolver refuses instead.
    let toml = r#"policy_version = 1

[[grant]]
key = "schema.create_table"
value = true
scope = "all"

[[inject]]
scope = "all"
columns = [
  { name = "created_at", type = "timestamptz", nullable = false, collation = "bytewise" },
]
"#;
    let policy = effective_policy_from_charter_toml(toml)
        .expect("the charter itself loads - the type check is the resolver's");
    let error = zero_migrate::resolve_create_table_policy(&authored_ir("notes"), &policy, "app")
        .expect_err("a collation on a timestamp column must be refused");
    assert!(
        format!("{error}").contains("collation"),
        "the refusal must say what it refused; got: {error}"
    );
}

// ── PostgreSQL: the defect and the fix, both measured as ROW ORDER ──────────────

#[cfg(test)]
fn quote_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn token(suffix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "injected_collation_{}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst),
        suffix
    )
}

/// Create the injected table in its own schema, insert [`CREATION_ORDER`], and
/// read the ids back with `ORDER BY id`.
async fn live_order(
    session: &support::PgDevSession,
    schema: &str,
    collation: Option<&str>,
) -> Result<Vec<String>, String> {
    let quoted_schema = quote_ident(schema);
    session
        .batch(&format!("CREATE SCHEMA {quoted_schema}"))
        .await
        .map_err(|error| format!("create the probe schema: {error}"))?;
    let ddl = injected_ddl(SqlDialect::Postgres, schema, "notes", collation);
    session
        .batch(&ddl)
        .await
        .map_err(|error| format!("apply the engine-emitted DDL: {error}\n{ddl}"))?;
    for (index, id) in CREATION_ORDER.iter().enumerate() {
        session
            .batch(&format!(
                "INSERT INTO {quoted_schema}.\"notes\" \
                 (id, created_at, created_by, version, title) \
                 VALUES ('{id}', now(), 'someone', {index}, 'title')"
            ))
            .await
            .map_err(|error| format!("insert {id}: {error}"))?;
    }
    let rows = session
        .query(
            &format!("SELECT id FROM {quoted_schema}.\"notes\" ORDER BY id"),
            &[],
        )
        .await
        .map_err(|error| format!("read the ordered ids: {error}"))?;
    rows.iter()
        .map(|row| {
            row.try_get::<_, String>(0)
                .map_err(|error| format!("an id column did not decode as text: {error}"))
        })
        .collect()
}

/// The instrument check. Every ordering claim below is void if this database's
/// default collation is already bytewise, because then the fixed and the unfixed
/// fixture agree and the test proves nothing.
async fn database_collation(session: &support::PgDevSession) -> Result<String, String> {
    let row = session
        .query_one(
            "SELECT datcollate FROM pg_database WHERE datname = current_database()",
            &[],
        )
        .await
        .map_err(|error| format!("read the database collation: {error}"))?;
    row.try_get::<_, String>(0)
        .map_err(|error| format!("datcollate did not decode as text: {error}"))
}

#[compio::test]
async fn injected_id_with_a_pinned_bytewise_collation_keeps_creation_order() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token("pinned");
    let _guard = support::SchemaGuard::arm(&session, [schema.clone()]);

    let result: Result<(), String> = async {
        let collate = database_collation(&session).await?;
        if collate == "C" || collate == "POSIX" {
            return Err(format!(
                "this database's default collation is {collate}, so an ordering \
                 assertion here cannot distinguish the fix from its absence"
            ));
        }
        let read = live_order(&session, &schema, Some("bytewise")).await?;
        if read != CREATION_ORDER {
            return Err(format!(
                "ORDER BY id must be creation order under a pinned bytewise \
                 collation on a {collate} database; got {read:?}"
            ));
        }

        // The other half of the claim. The fold writes `pg_catalog."C"` onto the
        // desired snapshot; live introspection reads the catalog back. If those two
        // disagree, every table the charter creates drifts the instant it exists -
        // a fix that emits the right DDL and then reports the schema as wrong.
        let policy = charter(Some("bytewise"));
        let resolved =
            zero_migrate::resolve_create_table_policy(&authored_ir("notes"), &policy, &schema)
                .map_err(|error| format!("resolve the injected create: {error}"))?;
        let expected =
            zero_migrate::fold_ops(&resolved.ops, SqlDialect::Postgres, &schema, &policy)
                .map_err(|error| format!("fold the injected create: {error}"))?;
        let live = zero_migrate::snapshot_schema(&session, &schema)
            .await
            .map_err(|error| format!("introspect the probe schema: {error}"))?;
        let drift = zero_migrate::diff_snapshots(&expected, &live);
        if !drift.is_clean() {
            return Err(format!(
                "a freshly created collated table must not drift: {drift:#?}"
            ));
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}

#[compio::test]
async fn injected_id_without_a_pinned_collation_loses_creation_order() {
    let url = skip_if_no_pg!();
    let session = support::PgDevSession::connect(&url);
    let schema = token("unpinned");
    let _guard = support::SchemaGuard::arm(&session, [schema.clone()]);

    let result: Result<(), String> = async {
        let collate = database_collation(&session).await?;
        if collate == "C" || collate == "POSIX" {
            return Err(format!(
                "this database's default collation is {collate}, so the neuter \
                 check cannot run and the fixed test above proves nothing"
            ));
        }
        let read = live_order(&session, &schema, None).await?;
        if read == CREATION_ORDER {
            return Err(format!(
                "the unpinned fixture came back in creation order on a {collate} \
                 database, so the pinned test above is not measuring the fix"
            ));
        }
        Ok(())
    }
    .await;
    result.unwrap_or_else(|error| panic!("{error}"));
}
