use super::fixture::Platform;

#[test]
fn platform_tables_have_a_non_nullable_id_primary_key() {
    Platform::with_database(async |client| {
        // Migration journals are owned by the engine, outside the platform corpus.
        let tables = client
            .query(
                "SELECT n.nspname || '.' || t.relname AS name,
                        a.attnotnull,
                        ARRAY(SELECT k.attname::text
                              FROM pg_constraint c
                              CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS cols(attnum, pos)
                              JOIN pg_attribute k ON k.attrelid = t.oid AND k.attnum = cols.attnum
                              WHERE c.conrelid = t.oid AND c.contype = 'p'
                              ORDER BY cols.pos) AS primary_key
                 FROM pg_class t
                 JOIN pg_namespace n ON n.oid = t.relnamespace
                 LEFT JOIN pg_attribute a ON a.attrelid = t.oid AND a.attname = 'id'
                                        AND a.attnum > 0 AND NOT a.attisdropped
                 WHERE n.nspname IN ('zeroship', 'service_authn')
                   AND t.relkind IN ('r', 'p')
                   AND left(t.relname, length('__zeroship_')) <> '__zeroship_'
                 ORDER BY name",
                &[],
            )
            .await
            .unwrap();
        assert!(!tables.is_empty(), "platform corpus contains no tables");
        let invalid: Vec<_> = tables
            .iter()
            .filter_map(|row| {
                let name: String = row.get(0);
                let non_null: Option<bool> = row.get(1);
                let primary_key: Vec<String> = row.get(2);
                (non_null != Some(true) || primary_key != ["id"]).then_some((
                    name,
                    non_null,
                    primary_key,
                ))
            })
            .collect();
        assert!(
            invalid.is_empty(),
            "platform table identity violations: {invalid:?}"
        );
    });
}

#[test]
fn generated_identity_preserves_natural_keys_and_service_writes() {
    Platform::with_database(async |client| {
        client
            .batch_execute("SET ROLE zeroship_auth")
            .await
            .unwrap();
        let first = client
            .query_one(
                "INSERT INTO zeroship.email_suppressions (email, reason)
             VALUES ('Identity@example.test', 'bounce') RETURNING id",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        let duplicate = client
            .execute(
                "INSERT INTO zeroship.email_suppressions (email, reason)
             VALUES ('identity@EXAMPLE.test', 'bounce')",
                &[],
            )
            .await
            .unwrap_err();
        assert_eq!(
            duplicate.code(),
            Some(&compio_postgres::error::SqlState::UNIQUE_VIOLATION)
        );
        let upserted = client
            .query_one(
                "INSERT INTO zeroship.email_suppressions (email, reason)
             VALUES ('identity@EXAMPLE.test', 'complaint')
             ON CONFLICT (email) DO UPDATE SET reason = EXCLUDED.reason RETURNING id",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        assert_eq!(first, upserted, "upsert replaced the row identity");

        client
            .batch_execute("RESET ROLE; SET ROLE zeroship_worker")
            .await
            .unwrap();
        let claimed = client
            .query_one(
                "INSERT INTO service_authn.service_assertion_replay (replay_key, expires_at)
             VALUES ('identity|claim', now() + interval '1 hour') RETURNING id",
                &[],
            )
            .await
            .unwrap()
            .get::<_, i64>(0);
        let replay = client
            .execute(
                "INSERT INTO service_authn.service_assertion_replay (replay_key, expires_at)
             VALUES ('identity|claim', now() + interval '1 hour')
             ON CONFLICT (replay_key) DO UPDATE SET expires_at = EXCLUDED.expires_at
             WHERE service_assertion_replay.expires_at <= now()",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(replay, 0, "a live assertion was replayable");
        assert_eq!(client.query_one(
            "SELECT id FROM service_authn.service_assertion_replay WHERE replay_key = 'identity|claim'",
            &[],
        ).await.unwrap().get::<_, i64>(0), claimed);

        client.batch_execute("RESET ROLE").await.unwrap();
        let rows = client
            .query(
                "INSERT INTO zeroship.token_revocations (client_id, sub)
             VALUES ('identity-client', 'first'), ('identity-client', 'second') RETURNING id",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].get::<_, i64>(0), rows[1].get::<_, i64>(0));
        let duplicate = client.execute(
            "INSERT INTO zeroship.token_revocations (client_id, sub) VALUES ('identity-client', 'first')",
            &[],
        ).await.unwrap_err();
        assert_eq!(
            duplicate.code(),
            Some(&compio_postgres::error::SqlState::UNIQUE_VIOLATION)
        );
    });
}
