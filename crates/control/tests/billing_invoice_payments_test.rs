//! PR-1 regression tests for billing-ops gap #26: the `0042` partial-unique-index
//! reshape + the `0046 invoice_payments` append-only side table + the cash-collected
//! helper.
//!
//! FAITHFUL by construction: every assertion runs against a live, migrated Postgres
//! (the REAL `invoices` partial unique index + the REAL `invoice_payments` table /
//! immutability trigger + the REAL `cash_collected` Rust helper). Gated on
//! `CONTROL_TEST_DB`; silent skip otherwise.
//!
//! These FAIL against the pre-reshape schema:
//!   (a) `invoice_payments` UPDATE/DELETE is rejected by the immutability trigger
//!       (no trigger pre-fix → the UPDATE would succeed).
//!   (b) the partial unique index lets a 2nd `(creator, period)` invoice exist ONLY
//!       when the first is `void`; two non-void are still blocked (pre-fix the
//!       unconditional UNIQUE blocks BOTH the void-then-reissue AND the second
//!       non-void, so the reissue assertion fails).
//!   (c) `cash_collected` = Σ(invoice_payments) reflects appended charges, and the
//!       finalized invoice the charge is recorded against is NEVER mutated (a direct
//!       finalized→finalized UPDATE still RAISEs — proving the side table sidesteps
//!       the immutability trigger). Pre-fix there is no table to sum.

#![allow(clippy::future_not_send)]

use compio_postgres::{connect, NoTls};
use uuid::Uuid;

use zeroship_control::invoice_payments::{append_charge, cash_collected};

fn db_url() -> Option<String> {
    std::env::var("CONTROL_TEST_DB").ok()
}

async fn pg(db_url: &str) -> compio_postgres::Client {
    let (client, conn) = connect(db_url, NoTls).await.expect("pg connect");
    compio::runtime::spawn(async move {
        let _ = conn.run().await;
    })
    .detach();
    client
}

fn first_of_this_month() -> chrono::NaiveDate {
    use chrono::Datelike;
    let now = chrono::Utc::now().date_naive();
    chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap()
}

/// Seed a user → creator_billing, returning the creator id.
async fn seed_creator(client: &compio_postgres::Client) -> Uuid {
    let email = format!("ip-{}@test.invalid", Uuid::new_v4().simple());
    let creator: Uuid = client
        .query(
            "INSERT INTO zeroship.users (email, name) VALUES ($1, 'invoice-payments') RETURNING id",
            &[&email],
        )
        .await
        .expect("insert user")[0]
        .get("id");
    client
        .execute(
            "INSERT INTO zeroship.creator_billing (creator_id) VALUES ($1) \
             ON CONFLICT (creator_id) DO NOTHING",
            &[&creator],
        )
        .await
        .expect("insert creator_billing");
    creator
}

/// Claim a draft invoice for `(creator, period)`; uniquify the period per call so
/// independent tests never collide on the (creator, period) claim.
async fn claim_draft(client: &compio_postgres::Client, creator: Uuid, period: chrono::NaiveDate) -> String {
    let id = zeroship_core::typed_id::new_invoice_id();
    client
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&id, &creator, &period],
        )
        .await
        .expect("claim draft");
    id
}

/// Atomically finalize an invoice (one-statement money write, balance CHECK holds).
async fn finalize(client: &compio_postgres::Client, inv: &str, total: i64) {
    client
        .execute(
            "UPDATE zeroship.invoices \
             SET subtotal_cents = $2, credit_cents = 0, tax_cents = 0, total_cents = $2, \
                 status = 'finalized', finalized_at = NOW() \
             WHERE id = $1",
            &[&inv, &total],
        )
        .await
        .expect("atomic finalize");
}

// ---------------------------------------------------------------------------
// (a) invoice_payments is append-only: the immutability trigger rejects UPDATE/DELETE.
// ---------------------------------------------------------------------------

#[compio::test]
async fn invoice_payments_is_append_only() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let creator = seed_creator(&client).await;
    let inv = claim_draft(&client, creator, first_of_this_month()).await;
    finalize(&client, &inv, 6000).await;

    // Append a charge row via the REAL helper.
    let pay_id = append_charge(&client, &inv, 6000, "usd", Some("in_appendonly"))
        .await
        .expect("append charge");

    // UPDATE is rejected by the immutability trigger (RED pre-fix: no trigger).
    let upd = client
        .execute(
            "UPDATE zeroship.invoice_payments SET amount_cents = 1 WHERE id = $1",
            &[&pay_id],
        )
        .await;
    assert!(upd.is_err(), "invoice_payments UPDATE must be rejected by the immutability trigger");

    // DELETE is rejected too.
    let del = client
        .execute("DELETE FROM zeroship.invoice_payments WHERE id = $1", &[&pay_id])
        .await;
    assert!(del.is_err(), "invoice_payments DELETE must be rejected by the immutability trigger");

    // The row is still intact and unchanged.
    let amt: i64 = client
        .query("SELECT amount_cents FROM zeroship.invoice_payments WHERE id = $1", &[&pay_id])
        .await
        .expect("read back")[0]
        .get("amount_cents");
    assert_eq!(amt, 6000, "the row must survive the rejected mutations unchanged");
}

// ---------------------------------------------------------------------------
// (b) the partial unique index: void releases the period claim for reissue,
//     but two NON-void invoices for the same (creator, period) are still blocked.
// ---------------------------------------------------------------------------

#[compio::test]
async fn partial_unique_index_releases_period_only_on_void() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let creator = seed_creator(&client).await;
    let period = first_of_this_month();

    // First invoice, finalized then voided (the legal correction transition).
    let inv1 = claim_draft(&client, creator, period).await;
    finalize(&client, &inv1, 5000).await;

    // While inv1 is NON-void (finalized), a SECOND non-void invoice for the same
    // (creator, period) is BLOCKED by the partial unique index.
    let inv2_id = zeroship_core::typed_id::new_invoice_id();
    let blocked = client
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv2_id, &creator, &period],
        )
        .await;
    assert!(
        blocked.is_err(),
        "a second NON-void invoice for the same (creator, period) must be rejected by the partial unique index",
    );

    // Void inv1 (finalized→void, money held equal — the only legal finalized
    // transition). This RELEASES the period claim.
    client
        .execute(
            "UPDATE zeroship.invoices SET status = 'void', voided_at = NOW() WHERE id = $1",
            &[&inv1],
        )
        .await
        .expect("void inv1");

    // NOW a fresh invoice can take the released period slot (RED pre-fix: the
    // unconditional UNIQUE would still block this).
    let inv_reissue = claim_draft(&client, creator, period).await;
    assert_ne!(inv_reissue, inv1, "the reissue must be a NEW invoice id");

    // And a SECOND non-void invoice on top of the live reissue is STILL blocked.
    let inv4_id = zeroship_core::typed_id::new_invoice_id();
    let blocked2 = client
        .execute(
            "INSERT INTO zeroship.invoices (id, creator_id, period, status) \
             VALUES ($1, $2, $3::date, 'draft')",
            &[&inv4_id, &creator, &period],
        )
        .await;
    assert!(
        blocked2.is_err(),
        "after reissue, a further non-void invoice must still be rejected (one live claim per period)",
    );

    // Exactly one non-void + one void row exist for this (creator, period).
    let counts = client
        .query(
            "SELECT \
               COUNT(*) FILTER (WHERE status <> 'void')::bigint AS active, \
               COUNT(*) FILTER (WHERE status = 'void')::bigint AS voided \
             FROM zeroship.invoices WHERE creator_id = $1 AND period = $2::date",
            &[&creator, &period],
        )
        .await
        .expect("count")[0]
        .clone();
    assert_eq!(counts.get::<_, i64>("active"), 1, "exactly one active claim");
    assert_eq!(counts.get::<_, i64>("voided"), 1, "the void row is retained as audit");
}

// ---------------------------------------------------------------------------
// (c) cash_collected = Σ(invoice_payments); appending a charge does NOT mutate the
//     finalized invoice (a direct finalized→finalized UPDATE still RAISEs).
// ---------------------------------------------------------------------------

#[compio::test]
async fn cash_collected_sums_payments_without_touching_finalized_invoice() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;
    let creator = seed_creator(&client).await;
    let inv = claim_draft(&client, creator, first_of_this_month()).await;
    finalize(&client, &inv, 6000).await;

    // No payments yet ⇒ cash-collected is 0.
    assert_eq!(cash_collected(&client, &inv).await.expect("cash 0"), 0);

    // Append two partial charges; the sum reflects both.
    append_charge(&client, &inv, 4000, "usd", Some("in_part1")).await.expect("charge 1");
    append_charge(&client, &inv, 2000, "usd", Some("in_part2")).await.expect("charge 2");
    assert_eq!(
        cash_collected(&client, &inv).await.expect("cash sum"),
        6000,
        "cash-collected must be Σ(invoice_payments.amount_cents)",
    );

    // The FINALIZED invoice was never touched: still finalized, totals unchanged.
    let row = client
        .query(
            "SELECT status, subtotal_cents, total_cents FROM zeroship.invoices WHERE id = $1",
            &[&inv],
        )
        .await
        .expect("read invoice")[0]
        .clone();
    assert_eq!(row.get::<_, String>("status"), "finalized");
    assert_eq!(row.get::<_, i64>("subtotal_cents"), 6000);
    assert_eq!(row.get::<_, i64>("total_cents"), 6000);

    // PROOF the side table is necessary: a DIRECT finalized→finalized money UPDATE on
    // the invoice still RAISEs (the immutability trigger), which is exactly why payment
    // tracking lives in a side table and never on the frozen invoice.
    let direct = client
        .execute(
            "UPDATE zeroship.invoices SET subtotal_cents = 7000, total_cents = 7000 WHERE id = $1",
            &[&inv],
        )
        .await;
    assert!(
        direct.is_err(),
        "a direct finalized-invoice money UPDATE must still be rejected by invoices_immutable()",
    );

    // append_charge rejects a non-positive amount (a $0 invoice records no row).
    assert!(append_charge(&client, &inv, 0, "usd", None).await.is_err());
    assert!(append_charge(&client, &inv, -1, "usd", None).await.is_err());
}

// ---------------------------------------------------------------------------
// (d) the PR-1 schema objects exist on the migrated DB (a Rust-visible guard that
//     the full changelog migrated clean through the new changesets).
// ---------------------------------------------------------------------------

#[compio::test]
async fn pr1_schema_objects_present() {
    let Some(url) = db_url() else {
        eprintln!("skip: CONTROL_TEST_DB not set");
        return;
    };
    let client = pg(&url).await;

    // The partial unique index replaced the unconditional UNIQUE.
    let partial: i64 = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_indexes \
             WHERE schemaname = 'zeroship' AND indexname = 'invoices_active_period_claim'",
            &[],
        )
        .await
        .expect("idx")[0]
        .get("n");
    assert_eq!(partial, 1, "the partial unique index must exist");

    let old_unique: i64 = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_constraint \
             WHERE conname = 'invoices_creator_id_period_key'",
            &[],
        )
        .await
        .expect("old unique")[0]
        .get("n");
    assert_eq!(old_unique, 0, "the unconditional UNIQUE must be gone");

    // The named line-provider-refs FK exists (so 0054 can DROP it by name).
    let named_fk: i64 = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_constraint \
             WHERE conname = 'billing_line_provider_refs_line_fk'",
            &[],
        )
        .await
        .expect("named fk")[0]
        .get("n");
    assert_eq!(named_fk, 1, "the line-provider-refs composite FK must be explicitly named");

    // The invoice_payments table + immutability trigger exist.
    let tbl: i64 = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_tables \
             WHERE schemaname = 'zeroship' AND tablename = 'invoice_payments'",
            &[],
        )
        .await
        .expect("table")[0]
        .get("n");
    assert_eq!(tbl, 1, "invoice_payments table must exist");

    let trg: i64 = client
        .query(
            "SELECT COUNT(*)::bigint AS n FROM pg_trigger \
             WHERE tgname = 'invoice_payments_immutable_trg'",
            &[],
        )
        .await
        .expect("trigger")[0]
        .get("n");
    assert_eq!(trg, 1, "invoice_payments immutability trigger must exist");
}
