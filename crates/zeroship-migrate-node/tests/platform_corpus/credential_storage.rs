use super::fixture::Platform;

#[test]
fn platform_hash_columns_have_no_textual_plaintext_sibling() {
    Platform::with_database(async |client| {
        let rows = client
            .query(
                "SELECT rel.relname::text,
                        hashed.attname::text,
                        EXISTS (
                            SELECT 1
                            FROM pg_attribute plaintext
                            WHERE plaintext.attrelid = hashed.attrelid
                              AND plaintext.attname = left(
                                  hashed.attname,
                                  length(hashed.attname) - length('_hash')
                              )
                              AND format_type(plaintext.atttypid, plaintext.atttypmod)
                                  IN ('text', 'character varying', 'character', 'bytea')
                              AND plaintext.attnum > 0
                              AND NOT plaintext.attisdropped
                        )
                 FROM pg_attribute hashed
                 JOIN pg_class rel ON rel.oid = hashed.attrelid
                 JOIN pg_namespace n ON n.oid = rel.relnamespace
                 WHERE n.nspname = 'zeroship'
                   AND rel.relkind IN ('r', 'p')
                   AND right(hashed.attname, length('_hash')) = '_hash'
                   AND hashed.attnum > 0
                   AND NOT hashed.attisdropped
                 ORDER BY rel.relname, hashed.attname",
                &[],
            )
            .await
            .expect("inspect platform hash columns");
        assert!(
            !rows.is_empty(),
            "platform corpus contains no hash columns to verify"
        );

        let pairs = rows
            .iter()
            .filter(|row| row.get::<_, bool>(2))
            .map(|row| {
                let table: String = row.get(0);
                let hashed: String = row.get(1);
                let plaintext = hashed
                    .strip_suffix("_hash")
                    .expect("catalog query selected a hash suffix");
                format!("zeroship.{table}.{plaintext}")
            })
            .collect::<Vec<_>>();
        assert!(
            pairs.is_empty(),
            "textual plaintext columns are stored beside their hashes: {pairs:?}"
        );
    });
}
