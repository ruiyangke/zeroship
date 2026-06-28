//! Faithful `generate` e2e against a REAL Postgres (no shims): eval a sample
//! `schema.js` → IR → diff against an empty live schema → assert the produced
//! versioned dbmate migration contains the expected DDL for the goodies
//! (vector / encrypted / mask sentinel / FK), and that a second `generate`
//! against the now-applied schema is a no-op.
//!
//! Requires `zeroship_migrate_test` on :5440.

use compio_postgres::Client;

const SAMPLE: &str = include_str!("fixtures/sample_schema.js");

const DEFAULT_DSN: &str =
    "host=localhost port=5440 user=postgres password=zeroship dbname=zeroship_migrate_test";

fn dsn() -> String {
    std::env::var("MIGRATE_TEST_DB").unwrap_or_else(|_| DEFAULT_DSN.to_string())
}

async fn pg() -> Client {
    let (client, conn) = compio_postgres::connect(&dsn(), compio_postgres::NoTls)
        .await
        .expect("connect to zeroship_migrate_test on :5440");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn unique_schema() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("mjs_{}_{nanos}_{n}", std::process::id())
}

#[compio::test]
async fn generate_emits_versioned_migration_with_goodie_ddl() {
    let conn = pg().await;
    let schema = unique_schema();
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .await
        .expect("create schema");

    let tmp = std::env::temp_dir().join(format!("{schema}_migs"));

    let outcome = zeroship_migrate::frontend::generate_migration(
        SAMPLE,
        &dsn(),
        &schema,
        "app_test",
        "initial_schema",
        &tmp,
    )
    .await
    .expect("generate should succeed");

    // A migration file was written.
    let path = outcome.written.expect("a migration file is written for a fresh schema");
    assert!(path.exists(), "migration file exists on disk");
    let fname = path.file_name().unwrap().to_string_lossy();
    // dbmate naming: <14-digit-ts>_<slug>.sql
    assert!(
        fname.ends_with("_initial_schema.sql"),
        "file slug, got {fname}"
    );
    assert!(
        fname.chars().take(14).all(|c| c.is_ascii_digit()),
        "14-digit ts prefix, got {fname}"
    );

    let body = std::fs::read_to_string(&path).expect("read migration file");

    // dbmate section markers present.
    assert!(body.contains("-- migrate:up"), "has up section");
    assert!(body.contains("-- migrate:down"), "has down section");

    // CREATE TABLE for both collections, qualified into the project schema.
    assert!(
        body.contains(&format!("\"{schema}\".\"users\"")),
        "users table qualified into schema; body:\n{body}"
    );
    assert!(
        body.contains(&format!("\"{schema}\".\"docs\"")),
        "docs table qualified; body:\n{body}"
    );

    // vector(1536) DDL for docs.embedding.
    assert!(
        body.contains("vector(1536)"),
        "vector column DDL present; body:\n{body}"
    );

    // Encrypted column (ssn) → BYTEA. The IR faithfully carries the
    // `encrypted` facet (asserted in tests/eval_ir.rs); the engine's
    // declarative differ renders the column TYPE as BYTEA here.
    //
    // The spec §4 contract — `BYTEA + inline /* zsenc:... */` and a
    // `COMMENT ... __zsmask:...` on the mask sibling — is satisfied by the
    // declarative differ as of P4 HALF A (the `DeclarativeAuthor` threads two
    // emission-only sentinel fields through `desired_snapshot` →
    // `render_create_table`, built via the SHARED `zeroship_schema` codec). The
    // sentinels are therefore present in the generated SQL (asserted just below),
    // not a follow-up gap — earlier revisions of this comment predated that work.
    assert!(
        body.to_lowercase().contains("\"ssn\" bytea"),
        "encrypted column is BYTEA; body:\n{body}"
    );
    // The zsenc sentinel is emitted on the encrypted column (P4 HALF A).
    assert!(
        body.contains("/* zsenc:") && body.contains("zsenc:deterministic:pii_key:string"),
        "encrypted column carries the inline zsenc sentinel; body:\n{body}"
    );
    // The __zsmask sentinel is emitted on the masked sibling(s) (P4 HALF A).
    assert!(
        body.contains("__zsmask:kind="),
        "masked siblings carry the __zsmask sentinel; body:\n{body}"
    );

    // The mask transform's hidden sibling columns ARE materialised
    // (`<col>_masked text`).
    assert!(
        body.contains("\"ssn_masked\" text"),
        "encrypted column gets a _masked sibling; body:\n{body}"
    );
    assert!(
        body.contains("\"phone_masked\" text"),
        "explicitly-masked column gets a _masked sibling; body:\n{body}"
    );

    // FK clause for docs.authorId → users ON DELETE CASCADE.
    assert!(
        body.contains("REFERENCES") && body.to_uppercase().contains("ON DELETE CASCADE"),
        "FK with ON DELETE CASCADE; body:\n{body}"
    );

    assert!(outcome.migration_count >= 2, "at least the two CREATE TABLEs");

    // cleanup
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = conn
        .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
        .await;
}

#[compio::test]
async fn generate_against_matching_live_schema_is_noop() {
    // A schema whose tables already exist (we apply the generated up SQL) →
    // a second generate produces no diff. We build a tiny schema (no goodies
    // that need extensions) so the up SQL applies on a vanilla PG.
    let conn = pg().await;
    let schema = unique_schema();
    conn.batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
        .await
        .expect("create schema");

    let simple = r#"
        import { t } from "@zeroship/db";
        export default { schema: {
            widgets: { label: t.string().required(), qty: t.number() },
        } };
    "#;
    let tmp = std::env::temp_dir().join(format!("{schema}_migs"));

    // First generate → a migration with the CREATE TABLE up SQL.
    let first = zeroship_migrate::frontend::generate_migration(
        simple, &dsn(), &schema, "app_test", "init", &tmp,
    )
    .await
    .expect("first generate");
    assert!(first.written.is_some(), "first generate writes a file");

    // Apply the up SQL directly (search_path so unqualified objects land in
    // our schema; the engine qualifies its own DDL anyway).
    let up = first
        .body
        .split("-- migrate:down")
        .next()
        .unwrap()
        .replace("-- migrate:up", "");
    conn.batch_execute(&format!("SET search_path TO \"{schema}\""))
        .await
        .unwrap();
    conn.batch_execute(&up)
        .await
        .unwrap_or_else(|e| panic!("apply up SQL failed: {e}\nSQL:\n{up}"));

    // Second generate against the now-live schema → no-op.
    let second = zeroship_migrate::frontend::generate_migration(
        simple, &dsn(), &schema, "app_test", "noop", &tmp,
    )
    .await
    .expect("second generate");
    assert!(
        second.written.is_none() && second.migration_count == 0,
        "second generate is a no-op (schema already matches); got {:?}",
        second.written
    );

    let _ = std::fs::remove_dir_all(&tmp);
    let _ = conn
        .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
        .await;
}
