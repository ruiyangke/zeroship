//! Integration tests for `StripeStore` against a live Postgres.
//!
//! Set `CONTROL_TEST_DB` to run; tests silently skip otherwise.

use uuid::Uuid;
use zeroship_control::{Registry, StripeStore};
use zeroship_control::stripe_store::StripeError;

fn db_url() -> Option<String> { std::env::var("CONTROL_TEST_DB").ok() }

fn fresh_creator_id() -> Uuid { Uuid::new_v4() }

#[compio::test]
async fn link_account_roundtrip() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    assert!(store.get_account(creator).await.unwrap().is_none());

    store.link_account(creator, "acct_testCreator1AAA").await.unwrap();

    let acc = store.get_account(creator).await.unwrap().expect("linked");
    assert_eq!(acc.creator_id, creator);
    assert_eq!(acc.stripe_account_id, "acct_testCreator1AAA");

    // Relink overwrites.
    store.link_account(creator, "acct_testCreator2BBB").await.unwrap();
    assert_eq!(
        store.get_account(creator).await.unwrap().unwrap().stripe_account_id,
        "acct_testCreator2BBB",
    );

    // Unlink returns true, then false.
    assert!(store.unlink_account(creator).await.unwrap());
    assert!(!store.unlink_account(creator).await.unwrap());
}

#[compio::test]
async fn reject_bad_account_id_shape() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    let err = store.link_account(creator, "cus_wrong_prefix").await.unwrap_err();
    match err {
        StripeError::Validation(m) => assert!(m.contains("acct_")),
        _ => panic!("expected Validation error, got {err:?}"),
    }

    // Stripe IDs must be alphanumeric + reasonable length. Reject shapes
    // that slip past a naive `starts_with("acct_")` check.
    let bad_shapes = [
        "acct_",                            // no id part
        "acct_short",                       // too short
        "acct_has-dash-chars",              // non-alphanumeric
        "acct_; DROP TABLE payouts;--",     // SQL-ish
    ];
    for bad in bad_shapes {
        let err = store.link_account(creator, bad).await.unwrap_err();
        assert!(matches!(err, StripeError::Validation(_)),
            "expected Validation for '{bad}', got {err:?}");
    }
}

#[compio::test]
async fn record_payout_idempotent() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    store.link_account(creator, "acct_idempotent123").await.unwrap();

    let rec = store
        .record_payout(
            creator,
            "evt_unique_1",
            "invoice.paid",
            1000,
            150,
            "usd",
            1_777_017_600i64,
        )
        .await
        .unwrap();
    assert_eq!(rec.gross_amount, 1000);
    assert_eq!(rec.platform_fee, 150);
    assert_eq!(rec.net_amount, 850);

    // Second call with same event_id is a Duplicate — no row added.
    let err = store
        .record_payout(
            creator,
            "evt_unique_1",
            "invoice.paid",
            9999,
            9999,
            "usd",
            1_777_021_200i64,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StripeError::Duplicate));

    // Aggregate reflects only the first write.
    let totals = store.total_earnings(creator).await.unwrap();
    assert_eq!(totals.gross, 1000);
    assert_eq!(totals.fee, 150);
    assert_eq!(totals.net, 850);

    store.unlink_account(creator).await.ok();
}

#[compio::test]
async fn total_earnings_aggregates_correctly() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    store.link_account(creator, "acct_aggregate12345").await.unwrap();
    for (i, (gross, fee)) in [(500, 75), (1000, 150), (750, 112)].iter().enumerate() {
        store
            .record_payout(
                creator,
                &format!("evt_agg_{i}"),
                "invoice.paid",
                *gross,
                *fee,
                "usd",
                1_777_024_800i64,
            )
            .await
            .unwrap();
    }

    let totals = store.total_earnings(creator).await.unwrap();
    assert_eq!(totals.gross, 2250);
    assert_eq!(totals.fee, 337);
    assert_eq!(totals.net, 1913);

    store.unlink_account(creator).await.ok();
}

#[compio::test]
async fn recent_payouts_newest_first_with_limit() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    store.link_account(creator, "acct_recentPayouts").await.unwrap();
    // Chronological order — occurred_at is what recent_payouts sorts on.
    let times = [
        1_767_225_600i64,
        1_769_904_000i64,
        1_772_323_200i64,
        1_775_001_600i64,
    ];
    for (i, t) in times.iter().enumerate() {
        store
            .record_payout(
                creator,
                &format!("evt_recent_{i}"),
                "invoice.paid",
                100,
                15,
                "usd",
                *t,
            )
            .await
            .unwrap();
    }

    let recent = store.recent_payouts(creator, 10).await.unwrap();
    assert_eq!(recent.len(), 4);
    // Newest first.
    assert!(recent[0].occurred_at.starts_with("2026-04-01"));
    assert!(recent[3].occurred_at.starts_with("2026-01-01"));

    // Limit is respected.
    let two = store.recent_payouts(creator, 2).await.unwrap();
    assert_eq!(two.len(), 2);
    assert!(two[0].occurred_at.starts_with("2026-04-01"));

    // limit <= 0 is clamped to 1.
    let one = store.recent_payouts(creator, 0).await.unwrap();
    assert_eq!(one.len(), 1);

    store.unlink_account(creator).await.ok();
}

#[compio::test]
async fn per_creator_isolation() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let a = fresh_creator_id();
    let b = fresh_creator_id();

    store.link_account(a, "acct_isolationA1234").await.unwrap();
    store.link_account(b, "acct_isolationB1234").await.unwrap();

    store.record_payout(a, "evt_A_1", "invoice.paid", 1000, 150, "usd", 1_777_024_800i64).await.unwrap();
    store.record_payout(b, "evt_B_1", "invoice.paid", 500, 75, "usd", 1_777_024_800i64).await.unwrap();

    let ta = store.total_earnings(a).await.unwrap();
    let tb = store.total_earnings(b).await.unwrap();
    assert_eq!(ta.gross, 1000);
    assert_eq!(tb.gross, 500);

    // Recent lists don't leak either.
    let ra = store.recent_payouts(a, 50).await.unwrap();
    let rb = store.recent_payouts(b, 50).await.unwrap();
    assert_eq!(ra.len(), 1);
    assert_eq!(rb.len(), 1);
    assert_eq!(ra[0].creator_id, a);
    assert_eq!(rb[0].creator_id, b);

    store.unlink_account(a).await.ok();
    store.unlink_account(b).await.ok();
}

#[compio::test]
async fn empty_creator_totals_are_zero() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    // No link, no payouts — SUM returns zero across the board.
    let totals = store.total_earnings(creator).await.unwrap();
    assert_eq!(totals.gross, 0);
    assert_eq!(totals.fee, 0);
    assert_eq!(totals.net, 0);

    assert!(store.recent_payouts(creator, 10).await.unwrap().is_empty());
}

#[compio::test]
async fn unlink_cascades_payouts() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    store.link_account(creator, "acct_cascade12345").await.unwrap();
    store.record_payout(creator, "evt_cascade_1", "invoice.paid", 100, 15, "usd", 1_777_024_800i64).await.unwrap();

    assert_eq!(store.recent_payouts(creator, 10).await.unwrap().len(), 1);

    // Dropping the creator_accounts row should cascade via FK to payouts.
    store.unlink_account(creator).await.unwrap();
    assert!(store.recent_payouts(creator, 10).await.unwrap().is_empty());
}
