use super::fixture::Platform;
use compio_postgres::{Client, error::SqlState};

#[derive(Debug)]
struct UserReference {
    schema: String,
    table: String,
    constraint: String,
    delete_action: String,
    nonnullable_delete_columns: Vec<String>,
}

impl UserReference {
    fn permits_erasure(&self) -> bool {
        match self.delete_action.as_str() {
            "c" => true,
            "n" => self.nonnullable_delete_columns.is_empty(),
            _ => false,
        }
    }
}

async fn user_references(client: &Client) -> Vec<UserReference> {
    client
        .query(
            "SELECT n.nspname::text, t.relname::text, c.conname::text,
                    c.confdeltype::text,
                    ARRAY(
                        SELECT a.attname::text
                        FROM unnest(COALESCE(c.confdelsetcols, c.conkey)) AS k(attnum)
                        JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum
                        WHERE a.attnotnull
                        ORDER BY a.attnum
                    )
             FROM pg_constraint c
             JOIN pg_class t ON t.oid = c.conrelid
             JOIN pg_namespace n ON n.oid = t.relnamespace
             WHERE c.contype = 'f' AND c.confrelid = 'zeroship.users'::regclass
             ORDER BY n.nspname, t.relname, c.conname",
            &[],
        )
        .await
        .expect("inspect incoming user references in the migrated catalog")
        .into_iter()
        .map(|row| UserReference {
            schema: row.get(0),
            table: row.get(1),
            constraint: row.get(2),
            delete_action: row.get(3),
            nonnullable_delete_columns: row.get(4),
        })
        .collect()
}

#[test]
fn migrated_user_references_allow_erasure() {
    Platform::with_database(async |client| {
        let references = user_references(client).await;
        assert!(
            references.len() >= 20,
            "missing platform user references: {references:?}"
        );
        for reference in references {
            assert!(
                reference.permits_erasure(),
                "{}.{} constraint {} blocks account erasure: {reference:?}",
                reference.schema,
                reference.table,
                reference.constraint,
            );
        }
    });
}

#[test]
fn erasure_catalog_check_agrees_with_postgres_delete_behavior() {
    Platform::with_database(async |client| {
        // These changes belong only to this test's disposable database. The
        // composite key exposes mixed nullability and targeted SET NULL actions.
        client
            .batch_execute(
                "CREATE SCHEMA erasure_fixture;
                 ALTER TABLE zeroship.users ADD UNIQUE (id, credential_version);",
            )
            .await
            .unwrap();

        for (action, nullability, rejection) in [
            ("CASCADE", "NOT NULL", None),
            ("SET NULL", "", None),
            ("SET NULL (user_id)", "NOT NULL", None),
            ("SET NULL", "NOT NULL", Some(SqlState::NOT_NULL_VIOLATION)),
            ("RESTRICT", "", Some(SqlState::FOREIGN_KEY_VIOLATION)),
            ("NO ACTION", "", Some(SqlState::FOREIGN_KEY_VIOLATION)),
            ("SET DEFAULT", "", Some(SqlState::FOREIGN_KEY_VIOLATION)),
        ] {
            client
                .batch_execute(&format!(
                    "CREATE TABLE erasure_fixture.reference (
                         user_id uuid DEFAULT '00000000-0000-0000-0000-000000000000',
                         version bigint {nullability} DEFAULT 0,
                         CONSTRAINT erasure_probe FOREIGN KEY (user_id, version)
                             REFERENCES zeroship.users (id, credential_version) ON DELETE {action}
                     );
                     INSERT INTO zeroship.users (email, name)
                         VALUES ('erasure-catalog@zeroship.test', 'Erasure catalog control');
                     INSERT INTO erasure_fixture.reference
                         SELECT id, credential_version FROM zeroship.users
                         WHERE email = 'erasure-catalog@zeroship.test';"
                ))
                .await
                .unwrap();

            let references = user_references(client).await;
            let probe = references
                .iter()
                .find(|reference| {
                    reference.schema == "erasure_fixture" && reference.constraint == "erasure_probe"
                })
                .expect("catalog query must discover the new reference");
            assert_eq!(
                probe.permits_erasure(),
                rejection.is_none(),
                "{action}: {probe:?}"
            );

            let deleted = client
                .execute(
                    "DELETE FROM zeroship.users WHERE email = 'erasure-catalog@zeroship.test'",
                    &[],
                )
                .await;
            if let Some(expected) = rejection {
                let error = deleted.expect_err("PostgreSQL must refuse this erasure");
                assert_eq!(
                    error.as_db_error().expect("database refusal").code(),
                    &expected,
                    "{action}: {error}"
                );
            } else {
                assert_eq!(
                    deleted.expect("PostgreSQL must permit this erasure"),
                    1,
                    "{action}"
                );
                let rows = client
                    .query(
                        "SELECT user_id::text, version FROM erasure_fixture.reference",
                        &[],
                    )
                    .await
                    .unwrap();
                if action == "CASCADE" {
                    assert!(rows.is_empty());
                } else {
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].get::<_, Option<String>>(0), None);
                    assert_eq!(
                        rows[0].get::<_, Option<i64>>(1),
                        (action == "SET NULL (user_id)").then_some(0)
                    );
                }
            }
            client
                .batch_execute(
                    "DROP TABLE erasure_fixture.reference;
                     DELETE FROM zeroship.users WHERE email = 'erasure-catalog@zeroship.test';",
                )
                .await
                .unwrap();
        }
    });
}
