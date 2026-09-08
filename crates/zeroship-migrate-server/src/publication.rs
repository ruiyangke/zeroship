//! Migration-owned logical publication reconciliation.

use compio_postgres::Client;
use zeroship_core::app_derivation;
use zeroship_core::app_id::AppId;

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
        AND c.relname NOT LIKE '\\_\\_zeroship\\_%' ESCAPE '\\'
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
        (false, true) => format!("CREATE PUBLICATION {publication}"),
        (false, false) => format!("CREATE PUBLICATION {publication} FOR TABLE {members}"),
        (true, false) => format!("ALTER PUBLICATION {publication} SET TABLE {members}"),
        // PostgreSQL has no empty SET TABLE form. When the final creator table
        // is dropped, PostgreSQL removes that table's publication membership,
        // so an existing publication already has the desired empty set.
        (true, true) => String::new(),
    }
}

/// Reconcile an app's publication to exactly its creator-owned tables.
///
/// This runs on the privileged migration connection after a successful apply.
/// It excludes the entire reserved `__zeroship_` namespace, so current and
/// future platform journals cannot enter the worker-visible WAL feed.
///
/// # Two identities, two derivations, one argument
///
/// This function spends its input on BOTH of the app id's meanings: the
/// publication is TENANT-keyed (a change-stream subject, not a namespace) and
/// the catalog filter is SCHEMA-keyed (`pg_namespace.nspname`). It used to take
/// one `&str` and hand the same bytes to both, under a comment at its apply-path
/// caller flagging the ambiguity as unresolved. It now takes the tenant and asks
/// [`app_derivation`] for each derived name separately, so the day the two stop
/// being the same string this reads correctly instead of silently naming one of
/// them twice.
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

    #[test]
    fn catalog_query_selects_only_top_level_creator_tables() {
        let sql = creator_table_query();
        assert!(sql.contains("c.relkind IN ('r', 'p')"));
        assert!(sql.contains("NOT c.relispartition"));
        assert!(sql.contains("NOT LIKE '\\_\\_zeroship\\_%' ESCAPE '\\'"));
        assert!(sql.contains("ORDER BY c.relname"));
    }

    #[test]
    fn publication_ddl_names_each_creator_table_explicitly() {
        let tables = ["notes".to_string(), "odd\"name".to_string()];
        let sql = publication_membership_sql(
            "__zs_pub_deadbeef",
            "app-one",
            &tables,
            false,
        );
        assert_eq!(
            sql,
            "CREATE PUBLICATION \"__zs_pub_deadbeef\" FOR TABLE \"app-one\".\"notes\", \"app-one\".\"odd\"\"name\""
        );
        assert!(!sql.contains("FOR TABLES IN SCHEMA"));

        let alter = publication_membership_sql(
            "__zs_pub_deadbeef",
            "app-one",
            &tables,
            true,
        );
        assert_eq!(
            alter,
            "ALTER PUBLICATION \"__zs_pub_deadbeef\" SET TABLE \"app-one\".\"notes\", \"app-one\".\"odd\"\"name\""
        );
        assert_eq!(
            publication_membership_sql("__zs_pub_deadbeef", "app-one", &[], true),
            ""
        );
    }
}
