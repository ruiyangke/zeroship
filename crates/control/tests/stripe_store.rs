//! Integration tests for `StripeStore` against a live Postgres.
//!
//! Set `CONTROL_TEST_DB` to run; tests silently skip otherwise.

use compio_postgres::{connect, NoTls};
use uuid::Uuid;
use zeroship_control::stripe_store::StripeError;
use zeroship_control::{Registry, StripeStore};

fn db_url() -> Option<String> { std::env::var("CONTROL_TEST_DB").ok() }

fn fresh_creator_id() -> Uuid { Uuid::new_v4() }

async fn pg(db_url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

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
    // Unique per test run — soft-delete preserves payouts so a fixed
    // string would collide with previous runs' rows.
    let evt = format!("evt_unique_{}", Uuid::new_v4());

    store.link_account(creator, "acct_idempotent123").await.unwrap();

    let rec = store
        .record_payout(
            creator,
            &evt,
            "invoice.paid",
            1000,
            150,
            "usd",
            1_777_017_600i64,
            None,
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
            &evt,
            "invoice.paid",
            9999,
            9999,
            "usd",
            1_777_021_200i64,
            None,
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
                &format!("evt_agg_{i}_{}", Uuid::new_v4()),
                "invoice.paid",
                *gross,
                *fee,
                "usd",
                1_777_024_800i64,
                None,
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
                &format!("evt_recent_{i}_{}", Uuid::new_v4()),
                "invoice.paid",
                100,
                15,
                "usd",
                *t,
                None,
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

    store.record_payout(a, &format!("evt_A_{}", Uuid::new_v4()), "invoice.paid", 1000, 150, "usd", 1_777_024_800i64, None).await.unwrap();
    store.record_payout(b, &format!("evt_B_{}", Uuid::new_v4()), "invoice.paid", 500, 75, "usd", 1_777_024_800i64, None).await.unwrap();


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
async fn payload_hash_mismatch_rejects_duplicate() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    store.link_account(creator, "acct_tamperCheck123").await.unwrap();
    let evt = format!("evt_tamper_{}", Uuid::new_v4());

    let hash_a: Vec<u8> = (0..32u8).collect();
    let hash_b: Vec<u8> = (100..132u8).collect();

    store
        .record_payout(
            creator, &evt, "invoice.paid", 1000, 150, "usd",
            1_777_024_800i64, Some(&hash_a),
        )
        .await
        .unwrap();

    // Honest Stripe retry: same hash → Duplicate.
    let dup_err = store
        .record_payout(
            creator, &evt, "invoice.paid", 1000, 150, "usd",
            1_777_024_800i64, Some(&hash_a),
        )
        .await
        .unwrap_err();
    assert!(matches!(dup_err, StripeError::Duplicate));

    // Tampered replay: same event_id, different hash → Validation error.
    let tamper_err = store
        .record_payout(
            creator, &evt, "invoice.paid", 9999, 999, "usd",
            1_777_024_800i64, Some(&hash_b),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(tamper_err, StripeError::Validation(_)),
        "expected Validation for tamper, got {tamper_err:?}",
    );

    store.unlink_account(creator).await.ok();
}

#[compio::test]
async fn payout_ledger_check_constraints_reject_impossible_rows() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();
    store.link_account(creator, "acct_checkConstraints").await.unwrap();

    let pg = pg(&url).await;
    let bad = pg
        .execute(
            "INSERT INTO zeroship.payouts
                (creator_id, event_id, event_type, gross_amount, platform_fee, net_amount, currency, occurred_at)
             VALUES ($1, $2, 'invoice.paid', 100, 500, -400, 'usd', NOW())",
            &[&creator, &format!("evt_bad_{}", Uuid::new_v4())],
        )
        .await;
    assert!(
        bad.is_err(),
        "zeroship.payouts CHECK constraints must reject impossible ledger rows"
    );

    store.unlink_account(creator).await.ok();
}

#[compio::test]
async fn unlink_is_soft_delete_payouts_preserved() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    store.link_account(creator, "acct_softDelete12345").await.unwrap();
    store.record_payout(
        creator, &format!("evt_soft_{}", Uuid::new_v4()), "invoice.paid", 100, 15, "usd",
        1_777_024_800i64, None,
    ).await.unwrap();
    assert_eq!(store.recent_payouts(creator, 10).await.unwrap().len(), 1);

    // Soft-delete: account becomes invisible via get_account but the
    // ledger row survives.
    assert!(store.unlink_account(creator).await.unwrap());
    assert!(store.get_account(creator).await.unwrap().is_none(),
        "soft-deleted account must not be visible via get_account");
    assert_eq!(store.recent_payouts(creator, 10).await.unwrap().len(), 1,
        "ledger must survive soft-delete");
    let totals = store.total_earnings(creator).await.unwrap();
    assert_eq!(totals.gross, 100);
}

#[compio::test]
async fn double_unlink_returns_false_second_time() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    store.link_account(creator, "acct_doubleUnlink12").await.unwrap();
    assert!(store.unlink_account(creator).await.unwrap());
    assert!(!store.unlink_account(creator).await.unwrap(),
        "second unlink must be a no-op (already soft-deleted)");
}

#[compio::test]
async fn same_account_link_is_idempotent_no_history_pollution() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    // Three back-to-back links of the SAME account — creator double-
    // clicked "Connect Stripe" or a script retried.
    store.link_account(creator, "acct_idempotentLink123").await.unwrap();
    store.link_account(creator, "acct_idempotentLink123").await.unwrap();
    store.link_account(creator, "acct_idempotentLink123").await.unwrap();

    // Exactly ONE history row (idempotent re-links don't pollute).
    let h = store.account_history(creator).await.unwrap();
    assert_eq!(h.len(), 1, "same-account relinks must not append history");
    assert_eq!(h[0].stripe_account_id, "acct_idempotentLink123");
    assert!(h[0].unlinked_at.is_none());
}

#[compio::test]
async fn creator_history_allows_only_one_open_row_per_creator() {
    let Some(url) = db_url() else { return; };
    Registry::new(&url).await.expect("registry");
    let pg = pg(&url).await;
    let creator = fresh_creator_id();

    pg.execute(
        "INSERT INTO zeroship.creator_account_history (creator_id, stripe_account_id)
         VALUES ($1, 'acct_openHistoryA12')",
        &[&creator],
    )
    .await
    .expect("insert first open history row");
    let duplicate = pg
        .execute(
            "INSERT INTO zeroship.creator_account_history (creator_id, stripe_account_id)
             VALUES ($1, 'acct_openHistoryB34')",
            &[&creator],
        )
        .await;
    assert!(
        duplicate.is_err(),
        "schema must reject a second open creator_account_history row"
    );

    pg.execute("DELETE FROM zeroship.creator_account_history WHERE creator_id = $1", &[&creator])
        .await
        .ok();
}

#[compio::test]
async fn relink_clears_unlinked_at_and_records_history() {
    let Some(url) = db_url() else { return; };
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = fresh_creator_id();

    store.link_account(creator, "acct_firstAccount12").await.unwrap();
    store.unlink_account(creator).await.unwrap();
    assert!(store.get_account(creator).await.unwrap().is_none());

    // Re-link with a different account id.
    store.link_account(creator, "acct_secondAccount34").await.unwrap();
    let acc = store.get_account(creator).await.unwrap().expect("re-linked");
    assert_eq!(acc.stripe_account_id, "acct_secondAccount34");

    // History shows newest-first with the OLD link closed and a new
    // open one.
    let h = store.account_history(creator).await.unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].stripe_account_id, "acct_secondAccount34");
    assert!(h[0].unlinked_at.is_none(), "new link is open");
    assert_eq!(h[1].stripe_account_id, "acct_firstAccount12");
    assert!(h[1].unlinked_at.is_some(), "old link is closed");
}

// ---------------------------------------------------------------------------
// Redesign regression (change 4): the customer id is RELOCATED to
// `billing_customer_refs` (a real-FK side table). `set_customer` round-trips
// through `get_creator_by_customer` (the providerless reverse probe backed by
// UNIQUE(external_id)), and the id is ABSENT from `creator_billing` (which is
// identity-only now). `set_customer` also creates the FK parent (creator_billing).
// ---------------------------------------------------------------------------

/// Insert a real `users` row (the FK parent of `creator_billing`). Returns its id.
async fn make_real_user(client: &compio_postgres::Client) -> Uuid {
    let email = format!("ss-cust-{}@test.invalid", Uuid::new_v4().simple());
    client
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, 'ss-cust') RETURNING id",
            &[&email],
        )
        .await
        .expect("insert user")[0]
        .get("id")
}

#[compio::test]
async fn set_customer_relocates_to_refs_and_reverse_lookup_round_trips() {
    let Some(url) = db_url() else { return; };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = make_real_user(&client).await;
    let cus = format!("cus_relocate_{}", Uuid::new_v4().simple());

    // set_customer creates the creator_billing parent + the side-table ref.
    store.set_customer(creator, &cus).await.expect("set_customer");

    // Forward read resolves the id from the side table.
    assert_eq!(
        store.get_customer(creator).await.unwrap().as_deref(),
        Some(cus.as_str()),
        "get_customer reads the id from billing_customer_refs",
    );
    // Reverse (providerless) lookup resolves the creator — UNIQUE(external_id).
    assert_eq!(
        store.get_creator_by_customer(&cus).await.unwrap(),
        Some(creator),
        "get_creator_by_customer round-trips via UNIQUE(external_id)",
    );

    // The id is in billing_customer_refs…
    let ref_rows = client
        .query(
            "SELECT external_id FROM zeroship.billing_customer_refs \
             WHERE creator_id = $1 AND provider = 'stripe'",
            &[&creator],
        )
        .await
        .unwrap();
    assert_eq!(ref_rows.len(), 1, "exactly one stripe customer ref");
    assert_eq!(ref_rows[0].get::<_, String>("external_id"), cus);

    // …and creator_billing carries NO stripe_customer_id column (fully relocated):
    // a query referencing that column must ERROR (the column no longer exists).
    let no_col = client
        .query(
            "SELECT stripe_customer_id FROM zeroship.creator_billing WHERE creator_id = $1",
            &[&creator],
        )
        .await;
    assert!(
        no_col.is_err(),
        "creator_billing has NO stripe_customer_id column — the id is fully relocated to billing_customer_refs",
    );

    // The identity (FK parent) row exists.
    let parent = client
        .query("SELECT 1 FROM zeroship.creator_billing WHERE creator_id = $1", &[&creator])
        .await
        .unwrap();
    assert_eq!(parent.len(), 1, "set_customer created the creator_billing identity (FK parent)");
}

#[compio::test]
async fn set_customer_is_idempotent_on_reset() {
    let Some(url) = db_url() else { return; };
    let client = pg(&url).await;
    let registry = Registry::new(&url).await.expect("registry");
    let store = StripeStore::new(registry);
    let creator = make_real_user(&client).await;
    let cus = format!("cus_idem_{}", Uuid::new_v4().simple());

    store.set_customer(creator, &cus).await.unwrap();
    // Re-setting the SAME id is a no-op write (ON CONFLICT (creator_id, provider)).
    store.set_customer(creator, &cus).await.expect("re-set same id is idempotent");
    let rows = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM zeroship.billing_customer_refs WHERE creator_id = $1",
            &[&creator],
        )
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, i64>("n"), 1, "still exactly one customer ref after re-set");
}
