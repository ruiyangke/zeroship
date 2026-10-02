use super::fixture::Platform;
use std::collections::BTreeSet;

const DENORMALIZED_USER_ID_COLUMNS: &[(&str, &str)] = &[
    ("app_audit", "actor_user_id"),
    ("app_egress_rules", "created_by"),
    ("audit_events", "actor_user_id"),
    ("authz_decisions", "actor_user_id"),
    ("gateway_sessions", "user_id"),
];

#[test]
fn platform_user_identity_uses_one_canonical_base36_storage_contract() {
    Platform::with_database(async |client| {
        let foreign_keys = client
            .query(
                "SELECT child.relname, child_column.attname
                 FROM pg_constraint con
                 JOIN pg_class parent ON parent.oid = con.confrelid
                 JOIN pg_namespace parent_namespace ON parent_namespace.oid = parent.relnamespace
                 JOIN pg_class child ON child.oid = con.conrelid
                 JOIN pg_namespace child_namespace ON child_namespace.oid = child.relnamespace
                 CROSS JOIN LATERAL unnest(con.conkey, con.confkey)
                     AS key_columns(child_attnum, parent_attnum)
                 JOIN pg_attribute child_column
                   ON child_column.attrelid = child.oid
                  AND child_column.attnum = key_columns.child_attnum
                 JOIN pg_attribute parent_column
                   ON parent_column.attrelid = parent.oid
                  AND parent_column.attnum = key_columns.parent_attnum
                 WHERE con.contype = 'f'
                   AND parent_namespace.nspname = 'zeroship'
                   AND parent.relname = 'users'
                   AND parent_column.attname = 'id'
                   AND child_namespace.nspname = 'zeroship'",
                &[],
            )
            .await
            .unwrap();
        assert!(
            !foreign_keys.is_empty(),
            "platform corpus contains no foreign keys to zeroship.users(id)"
        );

        let mut user_id_columns: BTreeSet<(String, String)> = foreign_keys
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        user_id_columns.extend(
            DENORMALIZED_USER_ID_COLUMNS
                .iter()
                .map(|(table, column)| ((*table).to_owned(), (*column).to_owned())),
        );
        user_id_columns.insert(("users".to_owned(), "id".to_owned()));

        for (table, column) in user_id_columns {
            let row = client
                .query_one(
                    "SELECT format_type(a.atttypid, a.atttypmod),
                            co.collname::text,
                            pg_get_expr(d.adbin, d.adrelid),
                            EXISTS (
                                SELECT 1
                                FROM pg_constraint con
                                WHERE con.conrelid = a.attrelid
                                  AND con.contype = 'c'
                                  AND a.attnum = ANY(con.conkey)
                                  AND pg_get_constraintdef(con.oid)
                                      LIKE '%^usr_[0-9a-z]{25}$%'
                            )
                     FROM pg_attribute a
                     JOIN pg_class rel ON rel.oid = a.attrelid
                     JOIN pg_namespace n ON n.oid = rel.relnamespace
                     LEFT JOIN pg_collation co ON co.oid = a.attcollation
                     LEFT JOIN pg_attrdef d
                       ON d.adrelid = a.attrelid AND d.adnum = a.attnum
                     WHERE n.nspname = 'zeroship'
                       AND rel.relname = $1
                       AND a.attname = $2
                       AND a.attnum > 0
                       AND NOT a.attisdropped",
                    &[&table, &column],
                )
                .await
                .unwrap_or_else(|error| panic!("inspect zeroship.{table}.{column}: {error}"));

            assert_eq!(
                row.get::<_, String>(0),
                "text",
                "zeroship.{table}.{column} is not text"
            );
            assert_eq!(
                row.get::<_, Option<String>>(1).as_deref(),
                Some("C"),
                "zeroship.{table}.{column} is not bytewise"
            );
            assert!(
                row.get::<_, bool>(3),
                "zeroship.{table}.{column} lacks canonical usr base36 validation"
            );
            if table == "users" && column == "id" {
                assert_eq!(
                    row.get::<_, Option<String>>(2),
                    None,
                    "zeroship.users.id must be minted by the application"
                );
            }
        }
    });
}
