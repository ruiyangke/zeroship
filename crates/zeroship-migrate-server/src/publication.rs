//! Migration-owned logical publication reconciliation.

use compio_postgres::Client;
use zeroship_core::app_derivation;
use zeroship_id::AppId;

/// A failure to reconcile an app publication after its schema migration.
///
/// There is no invalid-name arm any more. The publication used to be named from
/// an untyped `&str` through `zeroship_core::replication_names::publication_name`,
/// whose two refusals are an empty id and an embedded NUL; an [`AppId`] can be
/// neither, so [`app_derivation::publication_name`] is infallible and the arm
/// had no producer left.
#[derive(Debug, thiserror::Error)]
pub enum PublicationError {
    #[error("publication reconciliation database error: {0}")]
    Database(#[from] compio_postgres::Error),
}

const fn creator_table_query() -> &'static str {
    "SELECT c.relname
       FROM pg_class AS c
       JOIN pg_namespace AS n ON n.oid = c.relnamespace
      WHERE n.nspname = $1
        AND c.relkind IN ('r', 'p')
        AND NOT c.relispartition
      ORDER BY c.relname"
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn publication_membership_sql(
    publication: &str,
    schema: &str,
    tables: &[String],
    exists: bool,
) -> String {
    let publication = quote_ident(publication);
    let members = tables
        .iter()
        .map(|table| format!("{}.{}", quote_ident(schema), quote_ident(table)))
        .collect::<Vec<_>>()
        .join(", ");

    match (exists, members.is_empty()) {
        (false, true) => {
            format!("CREATE PUBLICATION {publication} WITH (publish_via_partition_root = true)")
        }
        (false, false) => format!(
            "CREATE PUBLICATION {publication} FOR TABLE {members} \
             WITH (publish_via_partition_root = true)"
        ),
        (true, false) => format!(
            "ALTER PUBLICATION {publication} SET (publish_via_partition_root = true); \
             ALTER PUBLICATION {publication} SET TABLE {members}"
        ),
        // PostgreSQL has no empty SET TABLE form. Recreate the publication
        // inside this transaction to clear every stale member.
        (true, true) => format!(
            "DROP PUBLICATION {publication}; \
             CREATE PUBLICATION {publication} WITH (publish_via_partition_root = true)"
        ),
    }
}

/// Reconcile an app's publication to its top-level tables.
///
/// This runs on the privileged migration connection after a successful apply.
/// Membership is scoped only by the app schema. Table names do not alter CDC
/// visibility; partition children remain represented by their top-level table.
pub async fn reconcile_app_publication(
    client: &Client,
    app: &AppId,
) -> Result<(), PublicationError> {
    let publication = app_derivation::publication_name(app);
    let schema = app_derivation::schema_name(app);
    client.batch_execute("BEGIN").await?;

    let result = reconcile_in_transaction(client, &schema, &publication).await;
    match result {
        Ok(()) => {
            client.batch_execute("COMMIT").await?;
            Ok(())
        }
        Err(err) => {
            let _ = client.batch_execute("ROLLBACK").await;
            Err(err)
        }
    }
}

async fn reconcile_in_transaction(
    client: &Client,
    schema: &str,
    publication: &str,
) -> Result<(), PublicationError> {
    client
        .query_text_params(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[publication],
        )
        .await?;

    let rows = client
        .query_text_params(creator_table_query(), &[schema])
        .await?;
    let tables = rows
        .iter()
        .map(|row| row.get::<_, String>("relname"))
        .collect::<Vec<_>>();
    let exists = !client
        .query_text_params(
            "SELECT 1 FROM pg_publication WHERE pubname = $1",
            &[publication],
        )
        .await?
        .is_empty();
    let sql = publication_membership_sql(publication, schema, &tables, exists);
    if !sql.is_empty() {
        client.batch_execute(&sql).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use compio_postgres::NoTls;
    use uuid::Uuid;

    #[test]
    fn catalog_query_includes_every_top_level_table_in_the_app_schema() {
        let sql = creator_table_query();
        assert!(sql.contains("c.relkind IN ('r', 'p')"));
        assert!(sql.contains("NOT c.relispartition"));
        assert!(!sql.contains("LIKE"));
        assert!(sql.contains("ORDER BY c.relname"));
    }

    #[test]
    fn publication_ddl_names_each_creator_table_explicitly() {
        let tables = ["notes".to_string(), "odd\"name".to_string()];
        let sql = publication_membership_sql("__zs_pub_deadbeef", "app-one", &tables, false);
        assert_eq!(
            sql,
            "CREATE PUBLICATION \"__zs_pub_deadbeef\" FOR TABLE \"app-one\".\"notes\", \"app-one\".\"odd\"\"name\" WITH (publish_via_partition_root = true)"
        );
        assert!(!sql.contains("FOR TABLES IN SCHEMA"));

        let alter = publication_membership_sql("__zs_pub_deadbeef", "app-one", &tables, true);
        assert_eq!(
            alter,
            "ALTER PUBLICATION \"__zs_pub_deadbeef\" SET (publish_via_partition_root = true); ALTER PUBLICATION \"__zs_pub_deadbeef\" SET TABLE \"app-one\".\"notes\", \"app-one\".\"odd\"\"name\""
        );
        assert_eq!(
            publication_membership_sql("__zs_pub_deadbeef", "app-one", &[], true),
            "DROP PUBLICATION \"__zs_pub_deadbeef\"; CREATE PUBLICATION \"__zs_pub_deadbeef\" WITH (publish_via_partition_root = true)"
        );
    }

    #[compio::test]
    async fn reconciliation_publishes_prefixed_tables_but_not_partition_children() {
        let (client, connection) =
            compio_postgres::connect(&zeroship_core::config::test_database_url(), NoTls)
                .await
                .expect("connect to the migrate-server test database");
        compio::runtime::spawn(async move {
            let _ = connection.run().await;
        })
        .detach();

        let app = AppId::mint();
        let schema = app_derivation::schema_name(&app);
        let schema_q = quote_ident(&schema);
        let publication = app_derivation::publication_name(&app);
        let publication_q = quote_ident(&publication);
        client
            .batch_execute(&format!(
                "CREATE SCHEMA {schema_q};
                 CREATE TABLE {schema_q}.notes (id bigint PRIMARY KEY);
                 CREATE TABLE {schema_q}.__zeroship_journal (id bigint PRIMARY KEY);
                 CREATE TABLE {schema_q}.events (id bigint, bucket integer) PARTITION BY LIST (bucket);
                 CREATE TABLE {schema_q}.events_default PARTITION OF {schema_q}.events DEFAULT;
                 CREATE VIEW {schema_q}.note_ids AS SELECT id FROM {schema_q}.notes;"
            ))
            .await
            .expect("create publication fixtures");

        reconcile_app_publication(&client, &app)
            .await
            .expect("reconcile app publication");
        let initial_tables = client
            .query_text_params(
                "SELECT c.relname
                   FROM pg_publication_rel AS pr
                   JOIN pg_publication AS p ON p.oid = pr.prpubid
                   JOIN pg_class AS c ON c.oid = pr.prrelid
                   JOIN pg_namespace AS n ON n.oid = c.relnamespace
                  WHERE p.pubname = $1 AND n.nspname = $2
                  ORDER BY c.relname",
                &[&publication, &schema],
            )
            .await
            .expect("read publication membership")
            .iter()
            .map(|row| row.get::<_, String>("relname"))
            .collect::<Vec<_>>();

        assert_eq!(
            initial_tables,
            ["__zeroship_journal", "events", "notes"],
            "publication membership must follow schema and relation kind only"
        );
        let publishes_via_root = client
            .query_one_scalar::<bool, _>(
                "SELECT pubviaroot FROM pg_publication WHERE pubname = $1",
                &[&publication],
            )
            .await
            .expect("read publication partition behavior");
        assert!(
            publishes_via_root,
            "partition writes must be emitted under the declared root collection"
        );

        client
            .batch_execute(&format!(
                "CREATE TABLE {schema_q}.__zeroship_late (id bigint PRIMARY KEY)"
            ))
            .await
            .expect("create a table after publication creation");
        reconcile_app_publication(&client, &app)
            .await
            .expect("reconcile existing app publication");
        let reconciled_tables = client
            .query_text_params(
                "SELECT c.relname
                   FROM pg_publication_rel AS pr
                   JOIN pg_publication AS p ON p.oid = pr.prpubid
                   JOIN pg_class AS c ON c.oid = pr.prrelid
                   JOIN pg_namespace AS n ON n.oid = c.relnamespace
                  WHERE p.pubname = $1 AND n.nspname = $2
                  ORDER BY c.relname",
                &[&publication, &schema],
            )
            .await
            .expect("read reconciled publication membership")
            .iter()
            .map(|row| row.get::<_, String>("relname"))
            .collect::<Vec<_>>();

        assert_eq!(
            reconciled_tables,
            ["__zeroship_journal", "__zeroship_late", "events", "notes"],
            "reconciliation must update an existing publication without filtering prefixes"
        );

        let sibling = Uuid::new_v4().to_string();
        let sibling_q = quote_ident(&sibling);
        client
            .batch_execute(&format!(
                "DROP VIEW {schema_q}.note_ids;
                 DROP TABLE {schema_q}.notes, {schema_q}.__zeroship_journal,
                            {schema_q}.__zeroship_late, {schema_q}.events CASCADE;
                 CREATE SCHEMA {sibling_q};
                 CREATE TABLE {sibling_q}.stale_member (id bigint PRIMARY KEY);
                 ALTER PUBLICATION {publication_q} ADD TABLE {sibling_q}.stale_member;
                 ALTER PUBLICATION {publication_q} SET (publish_via_partition_root = false);"
            ))
            .await
            .expect("seed stale publication membership");
        reconcile_app_publication(&client, &app)
            .await
            .expect("reconcile an empty creator schema");
        let empty_membership = client
            .query_text_params(
                "SELECT 1 FROM pg_publication_rel AS pr
                   JOIN pg_publication AS p ON p.oid = pr.prpubid
                  WHERE p.pubname = $1",
                &[&publication],
            )
            .await
            .expect("read empty publication membership");
        assert!(
            empty_membership.is_empty(),
            "an empty creator schema must clear stale publication members"
        );
        let publishes_via_root = client
            .query_one_scalar::<bool, _>(
                "SELECT pubviaroot FROM pg_publication WHERE pubname = $1",
                &[&publication],
            )
            .await
            .expect("read reconciled partition behavior");
        assert!(publishes_via_root);

        client
            .batch_execute(&format!(
                "DROP PUBLICATION {publication_q};
                 DROP SCHEMA {schema_q} CASCADE;
                 DROP SCHEMA {sibling_q} CASCADE;"
            ))
            .await
            .expect("remove publication fixtures");
    }
}
