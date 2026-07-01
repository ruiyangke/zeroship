//! Statement-classification correctness (Task 3).
//!
//! `classify` maps each parsed statement to a [`StatementClass`] — the DDL
//! kind, additive/destructive facets, transactionality, and the set of
//! schemas it references. The security guard (Task 4) is built on top of this.

use zeroship_migrate::classify::{classify, DataSecurityClass, DdlKind};

fn one(sql: &str) -> zeroship_migrate::classify::StatementClass {
    let mut v = classify(sql).expect("should parse");
    assert_eq!(v.len(), 1, "expected exactly one statement for: {sql}");
    v.pop().unwrap()
}

#[test]
fn create_table_is_additive() {
    let c = one("CREATE TABLE products(id int primary key, name text)");
    assert_eq!(c.kind, DdlKind::CreateTable);
    assert!(c.additive, "CREATE TABLE is additive");
    assert!(!c.destructive);
    assert!(!c.non_transactional);
}

#[test]
fn add_column_is_additive() {
    let c = one("ALTER TABLE products ADD COLUMN sku text");
    assert_eq!(c.kind, DdlKind::AddColumn);
    assert!(c.additive);
    assert!(!c.destructive);
}

#[test]
fn drop_column_is_destructive() {
    let c = one("ALTER TABLE products DROP COLUMN sku");
    assert_eq!(c.kind, DdlKind::DropColumn);
    assert!(!c.additive);
    assert!(c.destructive, "DROP COLUMN loses data");
}

#[test]
fn drop_table_is_destructive() {
    let c = one("DROP TABLE products");
    assert_eq!(c.kind, DdlKind::DropTable);
    assert!(c.destructive);
}

#[test]
fn create_index_concurrently_is_non_transactional() {
    let c = one("CREATE INDEX CONCURRENTLY i ON products(sku)");
    assert_eq!(c.kind, DdlKind::CreateIndexConcurrently);
    assert!(c.non_transactional, "CONCURRENTLY cannot run in a txn");
    // A plain index is transactional.
    let plain = one("CREATE INDEX i ON products(sku)");
    assert_eq!(plain.kind, DdlKind::CreateIndex);
    assert!(!plain.non_transactional);
}

#[test]
fn add_constraint_classified() {
    let c = one("ALTER TABLE products ADD CONSTRAINT chk CHECK (price > 0)");
    assert_eq!(c.kind, DdlKind::AddConstraint);
    assert!(c.additive);
    assert!(!c.destructive);
}

#[test]
fn drop_constraint_is_destructive() {
    let c = one("ALTER TABLE products DROP CONSTRAINT chk");
    assert_eq!(c.kind, DdlKind::DropConstraint);
    assert!(c.destructive);
    assert_eq!(c.destructive_operation, Some("DROP CONSTRAINT"));
}

#[test]
fn expanded_destructive_set_is_classified_canonically() {
    for (sql, operation) in [
        ("DELETE FROM products", "DELETE without WHERE"),
        ("DROP MATERIALIZED VIEW product_rollups", "DROP MATERIALIZED VIEW"),
        ("DROP VIEW product_view", "DROP VIEW"),
        ("DROP INDEX product_idx", "DROP INDEX"),
        ("DROP SEQUENCE product_seq", "DROP SEQUENCE"),
        ("DROP TYPE product_type", "DROP TYPE"),
        ("DROP DOMAIN product_domain", "DROP DOMAIN"),
        ("DROP FUNCTION product_fn()", "DROP FUNCTION"),
        ("ALTER TABLE products ALTER COLUMN price TYPE int", "ALTER COLUMN TYPE"),
        ("ALTER TABLE products DETACH PARTITION products_2026", "DETACH PARTITION"),
    ] {
        let c = one(sql);
        assert!(c.destructive, "{sql} must be classified destructive");
        assert_eq!(c.destructive_operation, Some(operation), "{sql}");
    }
}

#[test]
fn select_collects_referenced_schema() {
    let c = one("SELECT * FROM control.creator_billing");
    assert_eq!(c.kind, DdlKind::Select);
    assert_eq!(c.referenced_schemas, vec!["control".to_string()]);
}

#[test]
fn unqualified_select_has_no_schema() {
    let c = one("SELECT * FROM products");
    assert_eq!(c.kind, DdlKind::Select);
    assert!(
        c.referenced_schemas.is_empty(),
        "unqualified relation has no explicit schema"
    );
}

#[test]
fn multi_statement_yields_one_class_each_in_order() {
    let v = classify(
        "CREATE TABLE a(id int); ALTER TABLE a ADD COLUMN b text; DROP TABLE a;",
    )
    .expect("should parse");
    assert_eq!(v.len(), 3);
    assert_eq!(v[0].kind, DdlKind::CreateTable);
    assert_eq!(v[1].kind, DdlKind::AddColumn);
    assert_eq!(v[2].kind, DdlKind::DropTable);
}

#[test]
fn rename_table_and_column() {
    let t = one("ALTER TABLE products RENAME TO items");
    assert_eq!(t.kind, DdlKind::RenameTable);
    let col = one("ALTER TABLE products RENAME COLUMN sku TO code");
    assert_eq!(col.kind, DdlKind::RenameColumn);
}

#[test]
fn alter_column_type_classified() {
    let c = one("ALTER TABLE products ALTER COLUMN price TYPE bigint");
    assert_eq!(c.kind, DdlKind::AlterColumnType);
}

#[test]
fn dml_and_copy_and_create_extension() {
    assert_eq!(one("INSERT INTO products(id) VALUES (1)").kind, DdlKind::Dml);
    assert_eq!(one("UPDATE products SET id = 2").kind, DdlKind::Dml);
    assert_eq!(one("DELETE FROM products").kind, DdlKind::Dml);
    assert_eq!(
        one(
            "MERGE INTO products USING incoming ON products.id = incoming.id \
             WHEN MATCHED THEN UPDATE SET id = incoming.id"
        )
        .kind,
        DdlKind::Dml
    );
    assert_eq!(one("CREATE EXTENSION pgcrypto").kind, DdlKind::CreateExtension);
    assert_eq!(one("COPY products TO '/tmp/x'").kind, DdlKind::Copy);
}

#[test]
fn data_security_trichotomy_is_explicit() {
    assert_eq!(
        one("CREATE TABLE products(id int primary key)").data_security,
        DataSecurityClass::NonDestructive
    );
    assert_eq!(
        one("ALTER TABLE products ADD COLUMN sku text").data_security,
        DataSecurityClass::NonDestructive
    );
    assert_eq!(
        one("CREATE INDEX products_sku_idx ON products(sku)").data_security,
        DataSecurityClass::NonDestructive
    );
    assert_eq!(
        one("INSERT INTO products(id) VALUES (1)").data_security,
        DataSecurityClass::NonDestructive
    );
    assert_eq!(
        one("DO $$ BEGIN NULL; END $$").data_security,
        DataSecurityClass::Unknown
    );
}

#[test]
fn destructive_dml_holes_are_classified() {
    for (sql, operation) in [
        ("UPDATE products SET sku = NULL", "UPDATE without WHERE"),
        (
            "UPDATE products SET sku = NULL WHERE true",
            "UPDATE with non-restrictive WHERE",
        ),
        ("DELETE FROM products WHERE 1=1", "DELETE with non-restrictive WHERE"),
        (
            "MERGE INTO products USING incoming ON products.id = incoming.id \
             WHEN MATCHED THEN DELETE",
            "MERGE matched DELETE",
        ),
    ] {
        let c = one(sql);
        assert_eq!(
            c.data_security,
            DataSecurityClass::Destructive(operation),
            "{sql}"
        );
        assert_eq!(c.destructive_operation, Some(operation), "{sql}");
    }

    assert_eq!(
        one("DELETE FROM products WHERE id = 42").data_security,
        DataSecurityClass::Unknown,
        "row-affecting DML with an unproven predicate is not treated as safe"
    );
}

#[test]
fn create_function_and_role_and_grant_and_altersystem() {
    assert_eq!(
        one("CREATE FUNCTION f() RETURNS int LANGUAGE sql AS 'SELECT 1'").kind,
        DdlKind::CreateFunction
    );
    assert_eq!(one("CREATE ROLE evil").kind, DdlKind::CreateRole);
    assert_eq!(one("GRANT SELECT ON products TO evil").kind, DdlKind::Grant);
    assert_eq!(
        one("ALTER SYSTEM SET shared_buffers = '1GB'").kind,
        DdlKind::AlterSystem
    );
}

#[test]
fn schema_qualified_dml_collects_schema() {
    let c = one("INSERT INTO auth.users(id) VALUES (1)");
    assert_eq!(c.kind, DdlKind::Dml);
    assert_eq!(c.referenced_schemas, vec!["auth".to_string()]);
}

#[test]
fn join_collects_all_schemas() {
    let c = one(
        "SELECT * FROM products p JOIN control.billing b ON b.id = p.id JOIN auth.users u ON u.id = p.uid",
    );
    let mut got = c.referenced_schemas;
    got.sort();
    assert_eq!(got, vec!["auth".to_string(), "control".to_string()]);
}

#[test]
fn empty_input_yields_no_statements() {
    assert!(classify("").expect("empty parses").is_empty());
    assert!(classify("  -- just a comment\n").expect("comment-only parses").is_empty());
}

#[test]
fn unparseable_sql_is_an_error() {
    assert!(classify("this is not valid sql ;;;").is_err());
}
