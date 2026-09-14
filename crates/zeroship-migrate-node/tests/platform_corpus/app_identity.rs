use super::fixture::Platform;

#[test]
fn platform_app_identity_columns_use_bytewise_text_storage() {
    Platform::with_database(async |client| {
        let rows = client
            .query(
                "SELECT rel.relname::text,
                        attr.attname::text,
                        format_type(attr.atttypid, attr.atttypmod),
                        co.collname::text,
                        pg_get_expr(d.adbin, d.adrelid),
                        EXISTS (
                            SELECT 1
                            FROM pg_constraint con
                            WHERE con.conrelid = attr.attrelid
                              AND con.contype = 'c'
                              AND attr.attnum = ANY(con.conkey)
                              AND pg_get_constraintdef(con.oid)
                                  LIKE '%^app_[0-9a-z]{25}$%'
                        )
                 FROM pg_attribute attr
                 JOIN pg_class rel ON rel.oid = attr.attrelid
                 JOIN pg_namespace n ON n.oid = rel.relnamespace
                 LEFT JOIN pg_collation co ON co.oid = attr.attcollation
                 LEFT JOIN pg_attrdef d
                   ON d.adrelid = attr.attrelid
                  AND d.adnum = attr.attnum
                 WHERE n.nspname = 'zeroship'
                   AND rel.relkind IN ('r', 'p')
                   AND (
                       (rel.relname = 'apps' AND attr.attname = 'id')
                       OR attr.attname IN ('app_id', 'owner_app')
                   )
                   AND attr.attnum > 0
                   AND NOT attr.attisdropped
                 ORDER BY rel.relname, attr.attname",
                &[],
            )
            .await
            .expect("inspect platform app identity columns");
        assert!(
            !rows.is_empty(),
            "platform corpus contains no app identity columns"
        );

        let mut root_seen = false;
        for row in rows {
            let table: String = row.get(0);
            let column: String = row.get(1);
            assert_eq!(
                row.get::<_, String>(2),
                "text",
                "zeroship.{table}.{column} is not text"
            );
            assert_eq!(
                row.get::<_, Option<String>>(3).as_deref(),
                Some("C"),
                "zeroship.{table}.{column} is not bytewise"
            );

            if table == "apps" && column == "id" {
                root_seen = true;
                assert_eq!(
                    row.get::<_, Option<String>>(4),
                    None,
                    "zeroship.apps.id must be minted by the application"
                );
                assert!(
                    row.get::<_, bool>(5),
                    "zeroship.apps.id lacks canonical app base36 validation"
                );
            }
        }
        assert!(
            root_seen,
            "zeroship.apps.id is absent from the migrated schema"
        );
    });
}
