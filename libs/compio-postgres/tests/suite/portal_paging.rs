//! Boundary coverage for PostgreSQL extended-protocol portal paging.

use compio_postgres::{Client, Error, Portal, Transaction};
use futures_util::TryStreamExt;

#[allow(unused_imports)]
use crate::common;

const ORDERED_ROWS: &str =
    "SELECT value::int4 FROM generate_series(1, 10) AS value ORDER BY value";

#[derive(Debug, PartialEq, Eq)]
struct Page {
    values: Vec<i32>,
    command_rows: Option<u64>,
}

fn test_url() -> String {
    common::test_url()
}

async fn connect(url: &str) -> Result<Client, Error> {
    let (client, connection) = compio_postgres::connect(url, common::suite_tls()).await?;
    compio::runtime::spawn(async move {
        if let Err(error) = connection.run().await {
            eprintln!("connection error: {}", common::error_chain(&error));
        }
    })
    .detach();
    Ok(client)
}

async fn client() -> Client {
    let url = test_url();
    connect(&url)
        .await
        .unwrap_or_else(|error| common::postgres_unreachable(&url, &error))
}

async fn bind_ordered_rows(transaction: &Transaction<'_>) -> Portal {
    let statement = transaction.prepare(ORDERED_ROWS).await.unwrap();
    transaction.bind(&statement, &[]).await.unwrap()
}

async fn fetch_page(transaction: &Transaction<'_>, portal: &Portal, max_rows: i32) -> Page {
    let stream = transaction
        .query_portal_raw(portal, max_rows)
        .await
        .unwrap();
    let mut stream = Box::pin(stream);
    let mut values = Vec::new();
    while let Some(row) = stream.as_mut().try_next().await.unwrap() {
        values.push(row.get::<_, i32>(0));
    }

    Page {
        values,
        command_rows: stream.as_ref().get_ref().rows_affected(),
    }
}

fn expected_rows() -> Vec<i32> {
    (1..=10).collect()
}

#[compio::test]
async fn portal_paging_returns_every_row_once_in_order() {
    let mut client = client().await;
    let transaction = client.transaction().await.unwrap();
    let portal = bind_ordered_rows(&transaction).await;

    let mut values = Vec::new();
    let mut page_sizes = Vec::new();
    let mut command_rows = Vec::new();
    for _ in 0..4 {
        let page = fetch_page(&transaction, &portal, 3).await;
        page_sizes.push(page.values.len());
        command_rows.push(page.command_rows);
        values.extend(page.values);
    }

    assert_eq!(page_sizes, [3, 3, 3, 1]);
    assert_eq!(command_rows, [None, None, None, Some(1)]);
    assert_eq!(values, expected_rows());
    transaction.rollback().await.unwrap();
}

#[compio::test]
async fn a_limit_equal_to_the_row_count_needs_one_more_page_to_finish() {
    let mut client = client().await;
    let transaction = client.transaction().await.unwrap();
    let portal = bind_ordered_rows(&transaction).await;

    let full_page = fetch_page(&transaction, &portal, 10).await;
    assert_eq!(
        full_page,
        Page {
            values: expected_rows(),
            command_rows: None,
        },
        "ten rows at a limit of ten must suspend rather than claim exhaustion"
    );

    let terminal_page = fetch_page(&transaction, &portal, 10).await;
    assert_eq!(
        terminal_page,
        Page {
            values: Vec::new(),
            command_rows: Some(0),
        },
        "the required follow-up must reveal exhaustion without repeating a row"
    );
    transaction.rollback().await.unwrap();
}

#[compio::test]
async fn a_limit_greater_than_the_row_count_finishes_on_the_first_page() {
    let mut client = client().await;
    let transaction = client.transaction().await.unwrap();
    let portal = bind_ordered_rows(&transaction).await;

    assert_eq!(
        fetch_page(&transaction, &portal, 11).await,
        Page {
            values: expected_rows(),
            command_rows: Some(10),
        }
    );
    transaction.rollback().await.unwrap();
}

#[compio::test]
async fn an_empty_portal_finishes_with_no_rows_on_the_first_page() {
    let mut client = client().await;
    let transaction = client.transaction().await.unwrap();
    let statement = transaction
        .prepare("SELECT value::int4 FROM generate_series(1, 10) AS value WHERE false")
        .await
        .unwrap();
    let portal = transaction.bind(&statement, &[]).await.unwrap();

    assert_eq!(
        fetch_page(&transaction, &portal, 3).await,
        Page {
            values: Vec::new(),
            command_rows: Some(0),
        }
    );
    transaction.rollback().await.unwrap();
}

#[compio::test]
async fn a_zero_limit_returns_every_row_without_suspending() {
    let mut client = client().await;
    let transaction = client.transaction().await.unwrap();
    let portal = bind_ordered_rows(&transaction).await;

    assert_eq!(
        fetch_page(&transaction, &portal, 0).await,
        Page {
            values: expected_rows(),
            command_rows: Some(10),
        }
    );
    transaction.rollback().await.unwrap();
}

#[compio::test]
async fn dropping_a_suspended_portal_keeps_the_same_client_usable() {
    let mut client = client().await;
    let transaction = client.transaction().await.unwrap();
    let portal = bind_ordered_rows(&transaction).await;

    let first_page = transaction.query_portal(&portal, 3).await.unwrap();
    let first_values = first_page
        .iter()
        .map(|row| row.get::<_, i32>(0))
        .collect::<Vec<_>>();
    assert_eq!(first_values, [1, 2, 3]);
    drop(portal);

    let answer_in_transaction = transaction
        .query_one("SELECT 42::int4", &[])
        .await
        .expect("the transaction must remain usable immediately after portal drop");
    assert_eq!(answer_in_transaction.get::<_, i32>(0), 42);
    transaction.rollback().await.unwrap();

    let answer_on_client: i32 = client
        .query_one_scalar("SELECT 84::int4", &[])
        .await
        .expect("the client that abandoned the suspended portal must remain usable");
    assert_eq!(answer_on_client, 84);
}

#[compio::test]
async fn an_ordinary_query_returns_the_same_rows_once_in_order() {
    let client = client().await;
    let rows = client.query(ORDERED_ROWS, &[]).await.unwrap();
    let values = rows
        .iter()
        .map(|row| row.get::<_, i32>(0))
        .collect::<Vec<_>>();

    assert_eq!(values, expected_rows());
}

/// `rows_affected` distinguishes a suspended page from the final one, and the
/// count it reports belongs to that `Execute` rather than to the portal.
///
/// Both halves mislead if assumed. A caller who reads `Some(n)` as "the query
/// returned n rows" gets 1 for a 5-row portal; a caller who ignores `None`
/// loses the only signal that another page is waiting.
#[compio::test]
async fn rows_affected_marks_the_last_page_and_counts_only_that_execute() {
    let mut client = client().await;
    let transaction = client
        .transaction()
        .await
        .expect("begin a transaction for the portal");
    let portal = bind_ordered_rows(&transaction).await;

    let mut observed = Vec::new();
    loop {
        let page = fetch_page(&transaction, &portal, 4).await;
        let done = page.command_rows.is_some();
        observed.push((page.values.len(), page.command_rows));
        if done {
            break;
        }
    }

    let suspended = &observed[..observed.len() - 1];
    assert!(
        suspended.iter().all(|(_, affected)| affected.is_none()),
        "every page before the last must report None: {observed:?}"
    );
    let (last_len, last_affected) = observed[observed.len() - 1];
    assert_eq!(
        last_affected,
        Some(last_len as u64),
        "the final page reports ITS OWN row count, not the portal's: {observed:?}"
    );
    let total: usize = observed.iter().map(|(n, _)| n).sum();
    assert!(
        total > last_len,
        "the fixture must span more than one page or this proves nothing: {observed:?}"
    );
}
