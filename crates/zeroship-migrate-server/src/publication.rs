//! Migration-owned membership in the datastore's ONE shared publication.
//!
//! # The object is shared and the edit is not
//!
//! A datastore carries a single relay-owned publication
//! ([`DATASTORE_PUBLICATION`]) whose membership is the union of every database's
//! published tables. An apply therefore reconciles ONLY the member entries
//! whose relations live in its own schema, and it does so under the datastore
//! publication mutex - an advisory lock keyed on the publication, which is the
//! object being edited, not on the database, which is only one contributor to
//! it.
//!
//! `ALTER PUBLICATION ... SET TABLE` names the WHOLE object. One database
//! issuing it would drop every co-tenant database's tables out of the shared
//! stream, with no error from `PostgreSQL` and nothing in the relay to notice
//! until a subscriber silently stopped receiving changes. So the reconciliation
//! below computes a delta and issues `ADD TABLE` / `DROP TABLE`, and never
//! `SET TABLE`.
//!
//! # Membership is not the tenant filter
//!
//! Because the publication spans every database on the datastore, its
//! membership says nothing about who may read what. The relay separates
//! subscribers by comparing each decoded relation's namespace against the
//! schema of the one database that subscriber is bound to
//! (`zeroship_data_cdc_server::source`). Keying or filtering on publication
//! membership is the attractive wrong answer here precisely because a shared
//! publication deliberately spans tenants.

use compio_postgres::Client;
use zeroship_core::database_derivation;
use zeroship_core::replication_names::DATASTORE_PUBLICATION;
use zeroship_core::DatabaseId;

/// A failure to reconcile a database's entries in the datastore publication.
#[derive(Debug, thiserror::Error)]
pub enum PublicationError {
    #[error("publication reconciliation database error: {0}")]
    Database(#[from] compio_postgres::Error),
}

/// Every top-level table in one database's schema: the membership this
/// database is entitled to contribute.
const fn creator_table_query() -> &'static str {
    "SELECT c.relname
       FROM pg_class AS c
       JOIN pg_namespace AS n ON n.oid = c.relnamespace
      WHERE n.nspname = $1
        AND c.relkind IN ('r', 'p')
        AND NOT c.relispartition
      ORDER BY c.relname"
}

/// The publication's CURRENT members drawn from one database's schema.
///
/// Scoped by `nspname` in the query rather than filtered afterwards: an
/// unscoped read would hand this reconciliation every co-tenant's relations and
/// the delta would then propose dropping them.
const fn published_member_query() -> &'static str {
    "SELECT c.relname
       FROM pg_publication_rel AS pr
       JOIN pg_publication AS p ON p.oid = pr.prpubid
       JOIN pg_class AS c ON c.oid = pr.prrelid
       JOIN pg_namespace AS n ON n.oid = c.relnamespace
      WHERE p.pubname = $1
        AND n.nspname = $2
      ORDER BY c.relname"
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn qualified(schema: &str, table: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(table))
}

/// The statements that move this database's entries from `current` to
/// `desired`, and nothing else.
///
/// Returns an empty string when the two agree, so a converged database issues
/// no write at all against an object every other database on the cluster is
/// also publishing through.
fn membership_delta_sql(
    publication: &str,
    schema: &str,
    desired: &[String],
    current: &[String],
) -> String {
    let publication = quote_ident(publication);
    let added = desired
        .iter()
        .filter(|table| !current.contains(table))
        .map(|table| qualified(schema, table))
        .collect::<Vec<_>>();
    let removed = current
        .iter()
        .filter(|table| !desired.contains(table))
        .map(|table| qualified(schema, table))
        .collect::<Vec<_>>();

    let mut statements = Vec::new();
    if !added.is_empty() {
        statements.push(format!(
            "ALTER PUBLICATION {publication} ADD TABLE {}",
            added.join(", ")
        ));
    }
    if !removed.is_empty() {
        statements.push(format!(
            "ALTER PUBLICATION {publication} DROP TABLE {}",
            removed.join(", ")
        ));
    }
    statements.join("; ")
}

/// Reconcile one database's entries in the datastore publication.
///
/// Runs on the privileged migration connection after a successful apply.
/// Membership is scoped by this database's schema and by relation kind; names
/// and prefixes do not narrow it, and partition children stay represented by
/// their top-level table.
///
/// # Errors
///
/// [`PublicationError::Database`] on any statement failure. The whole
/// reconciliation is one transaction, so a failure leaves the shared object
/// exactly as it was.
pub async fn reconcile_database_publication(
    client: &Client,
    database: &DatabaseId,
) -> Result<(), PublicationError> {
    let schema = database_derivation::schema_name(database);
    client.batch_execute("BEGIN").await?;

    let result = reconcile_in_transaction(client, &schema).await;
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

async fn reconcile_in_transaction(client: &Client, schema: &str) -> Result<(), PublicationError> {
    // THE DATASTORE PUBLICATION MUTEX. Keyed on the object being edited, so
    // two databases reconciling at once serialise against each other; a key on
    // the database would let them interleave `ADD TABLE`s against one
    // publication and observe each other's half-applied deltas.
    client
        .query_text_params(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[DATASTORE_PUBLICATION],
        )
        .await?;

    let publication_q = quote_ident(DATASTORE_PUBLICATION);
    let existing = client
        .query_text_params(
            "SELECT pubviaroot FROM pg_publication WHERE pubname = $1",
            &[DATASTORE_PUBLICATION],
        )
        .await?;
    match existing.first() {
        None => {
            client
                .batch_execute(&format!(
                    "CREATE PUBLICATION {publication_q} \
                     WITH (publish_via_partition_root = true)"
                ))
                .await?;
        }
        Some(row) => {
            // Re-asserted only when it is wrong. The parameter is a datastore-
            // wide invariant rather than this database's business, so the
            // converged path writes nothing to the shared row.
            if !row.get::<_, bool>("pubviaroot") {
                client
                    .batch_execute(&format!(
                        "ALTER PUBLICATION {publication_q} \
                         SET (publish_via_partition_root = true)"
                    ))
                    .await?;
            }
        }
    }

    let desired = client
        .query_text_params(creator_table_query(), &[schema])
        .await?
        .iter()
        .map(|row| row.get::<_, String>("relname"))
        .collect::<Vec<_>>();
    let current = client
        .query_text_params(published_member_query(), &[DATASTORE_PUBLICATION, schema])
        .await?
        .iter()
        .map(|row| row.get::<_, String>("relname"))
        .collect::<Vec<_>>();

    let sql = membership_delta_sql(DATASTORE_PUBLICATION, schema, &desired, &current);
    if !sql.is_empty() {
        client.batch_execute(&sql).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_query_includes_every_top_level_table_in_the_database_schema() {
        let sql = creator_table_query();
        assert!(sql.contains("c.relkind IN ('r', 'p')"));
        assert!(sql.contains("NOT c.relispartition"));
        assert!(!sql.contains("LIKE"));
        assert!(sql.contains("ORDER BY c.relname"));
    }

    /// The delta names only this schema's relations, and never `SET TABLE`.
    #[test]
    fn membership_edits_are_deltas_and_never_replace_the_shared_object() {
        let desired = ["notes".to_string(), "odd\"name".to_string()];
        let sql = membership_delta_sql("__zs_pub_datastore", "db_one", &desired, &[]);
        assert_eq!(
            sql,
            "ALTER PUBLICATION \"__zs_pub_datastore\" ADD TABLE \"db_one\".\"notes\", \"db_one\".\"odd\"\"name\""
        );
        assert!(!sql.contains("SET TABLE"));
        assert!(!sql.contains("FOR TABLES IN SCHEMA"));

        let removal = membership_delta_sql("__zs_pub_datastore", "db_one", &[], &desired);
        assert_eq!(
            removal,
            "ALTER PUBLICATION \"__zs_pub_datastore\" DROP TABLE \"db_one\".\"notes\", \"db_one\".\"odd\"\"name\""
        );
        assert!(!removal.contains("SET TABLE"));
        assert!(!removal.contains("DROP PUBLICATION"));
    }

    /// A converged database writes nothing. That is what keeps a busy cluster
    /// from serialising every apply behind a write to one catalog row.
    #[test]
    fn a_converged_database_issues_no_statement() {
        let members = ["notes".to_string()];
        assert_eq!(
            membership_delta_sql("__zs_pub_datastore", "db_one", &members, &members),
            ""
        );
    }

    #[compio::test]
    async fn one_database_s_reconciliation_leaves_a_co_tenant_s_members_alone() {
        let client = crate::test_database::connect().await;

        let first = DatabaseId::mint();
        let second = DatabaseId::mint();
        let first_schema = database_derivation::schema_name(&first);
        let second_schema = database_derivation::schema_name(&second);
        let first_q = quote_ident(&first_schema);
        let second_q = quote_ident(&second_schema);
        client
            .batch_execute(&format!(
                "CREATE SCHEMA {first_q};
                 CREATE SCHEMA {second_q};
                 CREATE TABLE {first_q}.notes (id bigint PRIMARY KEY);
                 CREATE TABLE {first_q}.__zeroship_journal (id bigint PRIMARY KEY);
                 CREATE TABLE {first_q}.events (id bigint, bucket integer) PARTITION BY LIST (bucket);
                 CREATE TABLE {first_q}.events_default PARTITION OF {first_q}.events DEFAULT;
                 CREATE VIEW {first_q}.note_ids AS SELECT id FROM {first_q}.notes;
                 CREATE TABLE {second_q}.orders (id bigint PRIMARY KEY);"
            ))
            .await
            .expect("create publication fixtures");

        reconcile_database_publication(&client, &first)
            .await
            .expect("reconcile the first database");
        reconcile_database_publication(&client, &second)
            .await
            .expect("reconcile the second database");

        assert_eq!(
            published(&client, &first_schema).await,
            ["__zeroship_journal", "events", "notes"],
            "membership must follow schema and relation kind only"
        );
        assert_eq!(
            published(&client, &second_schema).await,
            ["orders"],
            "the second database contributes its own entries to the same object"
        );

        // THE SHARED-OBJECT ARM. Re-reconciling the first database after it
        // grew a table must not disturb the second's entries. `SET TABLE` here
        // would empty them with no error.
        client
            .batch_execute(&format!(
                "CREATE TABLE {first_q}.__zeroship_late (id bigint PRIMARY KEY)"
            ))
            .await
            .expect("create a table after the publication exists");
        reconcile_database_publication(&client, &first)
            .await
            .expect("re-reconcile the first database");
        assert_eq!(
            published(&client, &first_schema).await,
            ["__zeroship_journal", "__zeroship_late", "events", "notes"],
            "a later table joins without filtering prefixes"
        );
        assert_eq!(
            published(&client, &second_schema).await,
            ["orders"],
            "a co-tenant database's membership survives another database's apply"
        );

        let publishes_via_root = client
            .query_one_scalar::<bool, _>(
                "SELECT pubviaroot FROM pg_publication WHERE pubname = $1",
                &[&DATASTORE_PUBLICATION],
            )
            .await
            .expect("read publication partition behavior");
        assert!(
            publishes_via_root,
            "partition writes must be emitted under the declared root collection"
        );

        // An emptied schema withdraws its own entries and, again, only its own.
        client
            .batch_execute(&format!(
                "DROP VIEW {first_q}.note_ids;
                 DROP TABLE {first_q}.notes, {first_q}.__zeroship_journal,
                            {first_q}.__zeroship_late, {first_q}.events CASCADE;"
            ))
            .await
            .expect("empty the first database's schema");
        reconcile_database_publication(&client, &first)
            .await
            .expect("reconcile an empty database schema");
        assert!(
            published(&client, &first_schema).await.is_empty(),
            "an empty schema must carry no members"
        );
        assert_eq!(
            published(&client, &second_schema).await,
            ["orders"],
            "emptying one database must not empty the shared publication"
        );

        client
            .batch_execute(&format!(
                "DROP SCHEMA {first_q} CASCADE; DROP SCHEMA {second_q} CASCADE;"
            ))
            .await
            .expect("remove publication fixtures");
        // The publication itself is NOT dropped. It is the datastore's one
        // shared object and every other apply on this server publishes through
        // it; a teardown that removed it would be this test doing to its
        // neighbours exactly what the arm above proves an apply must not do.
    }

    /// The publication's members drawn from one schema, sorted.
    async fn published(client: &Client, schema: &str) -> Vec<String> {
        client
            .query_text_params(published_member_query(), &[DATASTORE_PUBLICATION, schema])
            .await
            .expect("read publication membership")
            .iter()
            .map(|row| row.get::<_, String>("relname"))
            .collect()
    }
}
