//! The MONEY rule, against the real billing tables.
//!
//! An organization that still owes cannot be closed, and the last human who
//! names it cannot be erased. That is one operator requirement with one
//! predicate behind it - [`zeroship_control::billing_read::outstanding_billing`]
//! and more than one place that asks it. This file rules on the rule at the
//! places that ask:
//!
//!   * `organizations::dissolve_organization`, the door,
//!   * `erasure::preflight`, the fence the account-deletion request runs,
//!   * the same fence re-asked over an ALREADY DISSOLVED organization, which is
//!     the state only the reaper's re-check can meet: the billing sweep
//!     finalizes the previous month whether or not its subject was closed, so a
//!     debt can appear INSIDE the grace window, after the organization is gone
//!     and after the request-time preflight said yes.
//!
//! The reaper's own decision - it asks control over HTTP, refuses on the money
//! rule, and leaves the user pending with a `billing` stage rather than
//! half-erased - is bound where the reaper lives, in
//! `crates/zeroship-auth/tests/account_deletion_test.rs`. It cannot be bound
//! here: `zeroship_auth` holds no privilege on these tables, which is the whole
//! reason the question crosses the boundary as a call.
//!
//! # Every case is paired
//!
//! A guard that refuses everything passes every refusal test ever written, so
//! nothing below is a lone refusal. Each case runs two organizations that
//! differ in ONE variable and asserts opposite answers. The pairing is what
//! makes a refusal evidence about the rule rather than about the fixture.
//!
//! # What is asserted, and what is deliberately not
//!
//! The assertions are about the RULE - does this organization owe money - not
//! about today's SQL. So the unbilled cases are written in terms of what a
//! closed period PRICED TO: one that prices to zero must not block however much
//! metered volume it carries, one that priced to money and reached no invoice
//! must block, and an invoice STATUS decides only whether the period was
//! accounted for at all.

#![allow(clippy::future_not_send)]

use compio_postgres::{connect, Client, NoTls};
use uuid::Uuid;

use zeroship_control::billing_read::{BillingRemedy, LocalInvoicing};
use zeroship_control::erasure::preflight;
use zeroship_control::organizations::{self, CreateOrganizationBody, OrganizationError};
use zeroship_control::Registry;
use zeroship_core::{AppId, UserId};

use crate::common;

/// An invoice that has not been finalized. Every amount on it is zero, and not
/// by convention: `invoice_total_balances` requires
/// `total = subtotal - credit + tax`, so a draft carrying a subtotal would have
/// to carry a total as well - and a total is exactly the claim a draft does not
/// yet make. The reconciler inserts the same all-zero row.
const DRAFT: &str = "draft";
/// A finalized invoice: the claim exists and only a void releases it.
const FINALIZED: &str = "finalized";

/// Cash collected against an invoice.
const CHARGE: &str = "charge";
/// Cash clawed back by the card network. Negative by construction, so it
/// RE-OPENS a debt an earlier charge had settled.
const DISPUTE_DEBIT: &str = "dispute_debit";

/// The metered volume every usage case puts in its closed period.
///
/// The number has to clear a threshold to mean anything. Pricing is
/// `round_half_up(billable_units x fx / 1e12)` and [`Fx::plan`] sets the FX so
/// one cent costs a thousand units, so a volume below that rounds to zero cents
/// no matter what the quota says - and a pair of organizations that BOTH price
/// to nothing proves nothing about a rule that is supposed to separate them.
const METERED_UNITS: i64 = 100_000;

/// A quota that leaves [`METERED_UNITS`] entirely uncovered, so the period
/// prices to real money.
const NO_QUOTA: i64 = 0;

/// A quota that swallows [`METERED_UNITS`] whole, so the same usage prices to
/// nothing. This is the free tier's ordinary month.
const COVERING_QUOTA: i64 = 1_000_000;

/// What [`METERED_UNITS`] comes to under a [`NO_QUOTA`] plan, in cents.
///
/// Derived from the two constants above and [`Fx::plan`]'s FX, not measured: a
/// thousand units cost a cent, and none of the volume is covered. Naming it is
/// what lets a refusal be asserted on the AMOUNT it quotes rather than on the
/// bare fact that something was reported - a predicate that reported every
/// period at zero cents would pass the second and fail the first.
const PRICED_PERIOD_CENTS: i64 = 100;

/// A standing charge, in cents. An app on a plan carrying one owes it whether
/// or not it served a single request, which is the whole content of the roster
/// case below.
const BASE_FEE_CENTS: i64 = 500;

struct Fx {
    registry: Registry,
    pg: Client,
}

impl Fx {
    async fn new() -> Self {
        let url = common::require_control_db();
        let (pg, conn) = connect(&url, NoTls).await.expect("control-pg connect");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        let registry = Registry::new(&url).await.expect("registry");
        Self { registry, pg }
    }

    async fn seed_user(&self, label: &str) -> UserId {
        let id = UserId::mint();
        let email = format!("{label}-{}@zeroship.test", id.as_str());
        self.pg
            .execute(
                "INSERT INTO zeroship.users (id, email, name, email_verified_at) \
                 VALUES ($1, $2::citext, $3, NOW())",
                &[&id.as_str(), &email, &label],
            )
            .await
            .expect("insert user");
        id
    }

    /// Mint an organization through the production path, and give it the
    /// billing row every invoice FKs into. `create_organization` does not write
    /// that row - the billing setup route does - so a fixture that wants an
    /// invoice has to say so.
    async fn organization(&self, owner: &UserId, label: &str) -> String {
        let id = organizations::create_organization(
            &self.registry,
            owner,
            &CreateOrganizationBody {
                name: format!("{label} {}", Uuid::new_v4().simple()),
                slug: Some(format!("{label}-{}", Uuid::new_v4().simple())),
                billing_email: None,
            },
            None,
        )
        .await
        .expect("create organization")
        .id;
        self.pg
            .execute(
                "INSERT INTO zeroship.organization_billing (organization_id) VALUES ($1) \
                 ON CONFLICT DO NOTHING",
                &[&id],
            )
            .await
            .expect("seed the organization billing row");
        id
    }

    /// A fresh organization owns a default project, and `dissolve_organization`
    /// refuses while one remains. Every case that expects the money rule to be
    /// the ONLY thing standing in the way empties it first.
    async fn drop_projects(&self, organization: &str) {
        self.pg
            .execute(
                "DELETE FROM zeroship.projects WHERE organization_id = $1",
                &[&organization],
            )
            .await
            .expect("drop projects");
    }

    /// A plan whose `included_units` decides whether a period's usage prices to
    /// anything. `base_fee_cents` is zero so the included quota is the only
    /// term, which is what makes "prices to zero" a statement about the usage
    /// rather than about a standing charge.
    ///
    /// The FX is `1e9` pico-cents per compute unit, so one cent costs `1e3`
    /// units. [`METERED_UNITS`] is sized against that: a quota below it prices
    /// the period to real money, a quota above it prices the same usage to
    /// nothing. Neither figure means anything without the other, which is why
    /// they are named together rather than spelled at the call sites.
    async fn plan(&self, included_units: i64) -> String {
        self.plan_priced(0, included_units).await
    }

    /// The same plan with a standing charge on it. A base fee is the one term
    /// that prices a period for an app which served nothing, so it is the only
    /// way to tell "the whole roster was priced" from "only the accruers were".
    async fn plan_priced(&self, base_fee_cents: i64, included_units: i64) -> String {
        let id = format!("owes-{}", Uuid::new_v4().simple());
        self.pg
            .execute(
                "INSERT INTO zeroship.plans \
                   (id, name, base_fee_cents, included_units, fx_pico_cents_per_unit, \
                    spend_limit_default_cents, runtime_limits_json) \
                 VALUES ($1, $1, $2, $3, 1000000000, 0, '{}'::jsonb)",
                &[&id, &base_fee_cents, &included_units],
            )
            .await
            .expect("insert plan");
        id
    }

    /// One app on `plan`, in the organization's default project, plus a metered
    /// period. `period_months_ago` is how far BACK the period sits: the
    /// predicate only looks at closed periods, and the current month is
    /// excluded on purpose because it always has accrued usage.
    async fn app_with_usage(
        &self,
        organization: &str,
        plan: &str,
        units: i64,
        period_months_ago: i32,
    ) -> AppId {
        let app = self.app(organization, plan).await;
        let metric = format!("owes_{}", Uuid::new_v4().simple());
        self.pg
            .execute(
                "INSERT INTO zeroship.billing_metrics (metric, kind, unit) \
                 VALUES ($1, 'custom', 'unit')",
                &[&metric],
            )
            .await
            .expect("declare the metric");
        // A metric with no weight contributes ZERO compute units - "unweighted
        // is free" is the pricer's rule, not an oversight - so without this row
        // every organization here would price to nothing and the two halves of
        // each pair would agree for the wrong reason.
        self.pg
            .execute(
                "INSERT INTO zeroship.metric_weights (metric, units_per_op, per_units) \
                 VALUES ($1, 1, 1)",
                &[&metric],
            )
            .await
            .expect("weight the metric");
        let months = i64::from(period_months_ago);
        self.pg
            .execute(
                "INSERT INTO zeroship.usage_aggregates (app_id, period, metric, total) \
                 VALUES ($1, (date_trunc('month', NOW()) \
                              - make_interval(months => $2::int))::date, $3, $4)",
                &[&app.as_str(), &period_months_ago, &metric, &units],
            )
            .await
            .unwrap_or_else(|e| panic!("insert usage ({months} months back): {e}"));
        app
    }

    /// One app on `plan`, in the organization's default project, that never
    /// served a request. It contributes no `usage_aggregates` row at all, so it
    /// is invisible to any read that starts from usage - which is exactly the
    /// property the roster case turns on.
    async fn app(&self, organization: &str, plan: &str) -> AppId {
        let project: String = self
            .pg
            .query_one(
                "SELECT id FROM zeroship.projects WHERE organization_id = $1 LIMIT 1",
                &[&organization],
            )
            .await
            .expect("the organization's default project")
            .get("id");
        let app = AppId::mint();
        self.pg
            .execute(
                "INSERT INTO zeroship.apps (id, name, organization_id, project_id, plan_id) \
                 VALUES ($1, $2, $3, $4, $5)",
                &[
                    &app.as_str(),
                    &format!("owes-{}", app.as_str()),
                    &organization,
                    &project,
                    &plan,
                ],
            )
            .await
            .expect("insert app");
        app
    }

    /// One invoice in a closed period. `status` is spliced from the constants
    /// above rather than bound, because the column is a domain and the fixture
    /// has no reason to accept a value the constants do not name.
    async fn invoice(
        &self,
        organization: &str,
        status: &'static str,
        subtotal_cents: i64,
        credit_cents: i64,
    ) -> String {
        assert!(
            matches!(status, DRAFT | FINALIZED),
            "an invoice is born draft or finalized; VOID is reached by transition"
        );
        let id = zeroship_core::typed_id::new_invoice_id();
        // A draft carries no amounts AT ALL, and the schema is what says so:
        // `invoice_total_balances` is `total = subtotal - credit + tax` on
        // EVERY row, so a draft holding a subtotal with a zero total is not a
        // representable state. The reconciler claims the row with nothing but
        // its key and writes every amount in the single finalize UPDATE; the
        // fixture does the same, which is why the draft's requested amounts are
        // carried to `finalize` rather than written here.
        let (subtotal, credit, total, finalized_at) = if status == FINALIZED {
            (
                subtotal_cents,
                credit_cents,
                subtotal_cents - credit_cents,
                "NOW()",
            )
        } else {
            (0, 0, 0, "NULL")
        };
        let sql = format!(
            "INSERT INTO zeroship.invoices \
               (id, organization_id, period, status, currency, \
                subtotal_cents, credit_cents, tax_cents, total_cents, finalized_at) \
             VALUES ($1, $2, (date_trunc('month', NOW()) - interval '2 months')::date, \
                     '{status}', 'usd', $3, $4, 0, $5, {finalized_at})"
        );
        self.pg
            .execute(&sql, &[&id, &organization, &subtotal, &credit, &total])
            .await
            .expect("insert invoice");
        id
    }

    /// Finalize a draft. This is the UPDATE that turns a not-yet-claim into a
    /// claim, and it is the one variable the draft pair below changes. It
    /// writes the amounts in the SAME statement as the status, exactly as
    /// `billing_reconcile` does, because the balance CHECK never sees a
    /// half-written row.
    async fn finalize(&self, invoice: &str, subtotal_cents: i64) {
        self.pg
            .execute(
                "UPDATE zeroship.invoices \
                    SET status = 'finalized', \
                        subtotal_cents = $2, \
                        total_cents = $2 - credit_cents + tax_cents, \
                        finalized_at = NOW() \
                  WHERE id = $1",
                &[&invoice, &subtotal_cents],
            )
            .await
            .expect("finalize the invoice");
    }

    /// Void a finalized invoice, which RELEASES the claim. Every amount is
    /// carried across unchanged because that is the only shape
    /// `invoices_immutable` accepts.
    async fn void(&self, invoice: &str) {
        self.pg
            .execute(
                "UPDATE zeroship.invoices SET status = 'void', voided_at = NOW() WHERE id = $1",
                &[&invoice],
            )
            .await
            .expect("void the invoice");
    }

    async fn pay(&self, invoice: &str, amount_cents: i64, kind: &'static str) {
        let id = format!("pay_{}", Uuid::new_v4().simple());
        self.pg
            .execute(
                "INSERT INTO zeroship.invoice_payments \
                   (id, invoice_id, amount_cents, currency, kind, provider_ref) \
                 VALUES ($1, $2, $3, 'usd', $4, $1)",
                &[&id, &invoice, &amount_cents, &kind],
            )
            .await
            .expect("record the payment");
    }

    /// Give the organization a provider customer. Presence is the whole of what
    /// the predicate reads - never the id - and it is what decides whether the
    /// creator has an action to take at all.
    async fn payment_identity(&self, organization: &str) {
        self.pg
            .execute(
                "INSERT INTO zeroship.billing_customer_refs \
                   (organization_id, provider, external_id) \
                 VALUES ($1, 'stripe', $2)",
                &[&organization, &format!("cus_{}", Uuid::new_v4().simple())],
            )
            .await
            .expect("attach the payment identity");
    }

    /// The remedy the erasure fence names for one organization.
    async fn remedy(&self, principal: &UserId, organization: &str) -> BillingRemedy {
        preflight(&self.pg, principal, LocalInvoicing::Yes)
            .await
            .expect("preflight")
            .billing_blockers
            .iter()
            .find(|b| b.organization_id == organization)
            .unwrap_or_else(|| panic!("no money blocker for {organization}"))
            .remedy
    }

    /// The erasure fence's answer for one principal, reduced to the money rule.
    /// The OWNERSHIP rule is not read here: it has its own file, and mixing the
    /// two would let an ownership blocker stand in for a money one.
    async fn money_blockers(&self, principal: &UserId) -> Vec<String> {
        preflight(&self.pg, principal, LocalInvoicing::Yes)
            .await
            .expect("preflight")
            .billing_blockers
            .iter()
            .map(|b| b.organization_id.clone())
            .collect()
    }

    async fn dissolve(
        &self,
        principal: &UserId,
        organization: &str,
    ) -> Result<(), OrganizationError> {
        organizations::dissolve_organization(
            &self.registry,
            principal,
            organization,
            LocalInvoicing::Yes,
            None,
        )
        .await
        .map(|_| ())
    }
}

/// Both enforcement points that read the tables refuse, and the refusal names
/// the money rather than something else that could also have refused.
async fn assert_refused(fx: &Fx, owner: &UserId, organization: &str, owed_cents: i64) {
    let report = preflight(&fx.pg, owner, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    let blocker = report
        .billing_blockers
        .iter()
        .find(|b| b.organization_id == organization)
        .unwrap_or_else(|| panic!("no money blocker for {organization}: {report:?}"));
    assert_eq!(
        blocker.owed_cents, owed_cents,
        "the refusal has to quote what is actually owed"
    );

    match fx.dissolve(owner, organization).await {
        Err(OrganizationError::OrganizationOwesBilling(outstanding)) => {
            assert_eq!(outstanding.owed_cents(), owed_cents);
        }
        other => panic!("dissolve did not refuse on the money rule: {other:?}"),
    }
}

/// Both enforcement points refuse over a closed period that priced to money and
/// was never billed, and the refusal QUOTES that price.
///
/// The claimed-cash figure is asserted to be zero on purpose. An unbilled period
/// carries a price and no claim, so `owed_cents` is the wrong number to reach
/// for and a refusal that offered only that one said "you owe" and "you owe
/// nothing" in the same breath. Both figures are checked here so neither can
/// quietly become the other.
async fn assert_unbilled_refused(fx: &Fx, owner: &UserId, organization: &str, unbilled_cents: i64) {
    let report = preflight(&fx.pg, owner, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    let blocker = report
        .billing_blockers
        .iter()
        .find(|b| b.organization_id == organization)
        .unwrap_or_else(|| panic!("no money blocker for {organization}: {report:?}"));
    assert_eq!(
        blocker.owed_cents, 0,
        "an unbilled period is not a claim, so no cash is owed on it yet"
    );
    assert_eq!(
        blocker.unbilled_period_count, 1,
        "one closed period is unaccounted for"
    );
    assert_eq!(
        blocker.outstanding.unbilled_cents(),
        unbilled_cents,
        "the refusal has to quote what the unbilled period priced to"
    );

    match fx.dissolve(owner, organization).await {
        Err(OrganizationError::OrganizationOwesBilling(outstanding)) => {
            assert_eq!(outstanding.owed_cents(), 0);
            assert_eq!(outstanding.unbilled_cents(), unbilled_cents);
        }
        other => panic!("dissolve did not refuse on the money rule: {other:?}"),
    }
}

/// Neither enforcement point cites MONEY over this organization.
///
/// Weaker than [`assert_allowed`] and deliberately so: every organization here
/// still owns an app, so `dissolve` answers the projects rider and can never
/// succeed. What is asserted is which refusal comes out, which is the whole of
/// what the money rule decides.
async fn assert_money_clear(fx: &Fx, owner: &UserId, organization: &str) {
    let blockers = fx.money_blockers(owner).await;
    assert!(
        !blockers.iter().any(|id| id == organization),
        "{organization} owes nothing and must not be a money blocker: {blockers:?}"
    );
    if let Err(OrganizationError::OrganizationOwesBilling(outstanding)) =
        fx.dissolve(owner, organization).await
    {
        panic!("dissolve cited the money rule over a settled organization: {outstanding:?}");
    }
}

/// Neither point holds this organization back. `dissolve` is destructive, so it
/// runs last and its success is the strongest form of "allowed" available.
async fn assert_allowed(fx: &Fx, owner: &UserId, organization: &str) {
    let blockers = fx.money_blockers(owner).await;
    assert!(
        !blockers.iter().any(|id| id == organization),
        "{organization} is settled and must not be a money blocker: {blockers:?}"
    );
    fx.dissolve(owner, organization)
        .await
        .expect("a settled organization closes");
    let blockers = fx.money_blockers(owner).await;
    assert!(
        !blockers.iter().any(|id| id == organization),
        "closing a settled organization must not create a debt: {blockers:?}"
    );
}

/// The base case, and the one every other case is measured against: an unpaid
/// finalized invoice refuses, and paying THE SAME invoice in full clears it.
///
/// One variable moves between the two organizations - whether the cash arrived.
#[compio::test]
async fn an_unpaid_invoice_refuses_and_paying_it_clears_the_refusal() {
    let fx = Fx::new().await;

    let debtor = fx.seed_user("owes-unpaid").await;
    let owing = fx.organization(&debtor, "owes-unpaid").await;
    fx.drop_projects(&owing).await;
    fx.invoice(&owing, FINALIZED, 1_500, 0).await;
    assert_refused(&fx, &debtor, &owing, 1_500).await;

    let payer = fx.seed_user("owes-paid").await;
    let settled = fx.organization(&payer, "owes-paid").await;
    fx.drop_projects(&settled).await;
    let invoice = fx.invoice(&settled, FINALIZED, 1_500, 0).await;
    fx.pay(&invoice, 1_500, CHARGE).await;
    assert_allowed(&fx, &payer, &settled).await;

    common::drain_pg().await;
}

/// A void RELEASES the claim, so it must not refuse - and a credit that covers
/// the whole invoice must not either, by arithmetic rather than by a case of
/// its own. The refusing control is the same invoice left standing.
#[compio::test]
async fn a_void_and_a_fully_credited_invoice_are_both_settled() {
    let fx = Fx::new().await;

    let voider = fx.seed_user("owes-void").await;
    let voided = fx.organization(&voider, "owes-void").await;
    fx.drop_projects(&voided).await;
    let invoice = fx.invoice(&voided, FINALIZED, 2_000, 0).await;
    // Standing, it refuses. This is the control: one transition separates the
    // two halves of this test.
    assert_refused(&fx, &voider, &voided, 2_000).await;
    fx.void(&invoice).await;
    assert_allowed(&fx, &voider, &voided).await;

    let credited_owner = fx.seed_user("owes-credit").await;
    let credited = fx.organization(&credited_owner, "owes-credit").await;
    fx.drop_projects(&credited).await;
    // Subtotal fully covered by credit: total_cents is zero, no payment row
    // exists, and nothing is owed.
    fx.invoice(&credited, FINALIZED, 2_000, 2_000).await;
    assert_allowed(&fx, &credited_owner, &credited).await;

    common::drain_pg().await;
}

/// Part of the cash is not all of it. The pair differs in the AMOUNT collected
/// and in nothing else, so a predicate that tested "any payment row exists"
/// rather than the balance would fail exactly here.
#[compio::test]
async fn a_partially_collected_invoice_still_owes_the_remainder() {
    let fx = Fx::new().await;

    let owner = fx.seed_user("owes-partial").await;
    let partial = fx.organization(&owner, "owes-partial").await;
    fx.drop_projects(&partial).await;
    let invoice = fx.invoice(&partial, FINALIZED, 1_000, 0).await;
    fx.pay(&invoice, 400, CHARGE).await;
    assert_refused(&fx, &owner, &partial, 600).await;

    let full_owner = fx.seed_user("owes-full").await;
    let full = fx.organization(&full_owner, "owes-full").await;
    fx.drop_projects(&full).await;
    let paid = fx.invoice(&full, FINALIZED, 1_000, 0).await;
    fx.pay(&paid, 1_000, CHARGE).await;
    assert_allowed(&fx, &full_owner, &full).await;

    common::drain_pg().await;
}

/// A chargeback claws the cash back, so an invoice that WAS settled owes again.
/// The pair is a paid invoice with and without the debit: nothing else differs,
/// and the debt the network created is the whole of the difference.
#[compio::test]
async fn a_chargeback_reopens_a_paid_invoice() {
    let fx = Fx::new().await;

    let owner = fx.seed_user("owes-chargeback").await;
    let disputed = fx.organization(&owner, "owes-chargeback").await;
    fx.drop_projects(&disputed).await;
    let invoice = fx.invoice(&disputed, FINALIZED, 1_000, 0).await;
    fx.pay(&invoice, 1_000, CHARGE).await;
    fx.pay(&invoice, -400, DISPUTE_DEBIT).await;
    assert_refused(&fx, &owner, &disputed, 400).await;

    let clean_owner = fx.seed_user("owes-undisputed").await;
    let clean = fx.organization(&clean_owner, "owes-undisputed").await;
    fx.drop_projects(&clean).await;
    let paid = fx.invoice(&clean, FINALIZED, 1_000, 0).await;
    fx.pay(&paid, 1_000, CHARGE).await;
    assert_allowed(&fx, &clean_owner, &clean).await;

    common::drain_pg().await;
}

/// A DRAFT invoice is not a CLAIM, so the unpaid-invoice arm must not refuse on
/// it.
///
/// This is a decision, not an accident, and it is worth stating: a draft is the
/// reconciler's scratch row. Its `total_cents` is zero until the finalize
/// UPDATE writes one, so there is no amount that arm could quote and no claim
/// anybody could settle - "pay this" would name a number that does not exist
/// yet. The claim begins at finalize, and finalizing the SAME row is the one
/// variable this pair moves.
///
/// This organization carries NO usage, which is what keeps the case about the
/// invoice arm alone. A draft over a period that did accrue is the other arm's
/// question and has its own pair below: not being a claim is not the same as
/// having settled the period, and reading it as both is how a real debt escaped
/// every enforcement point at once.
#[compio::test]
async fn a_draft_invoice_is_not_yet_a_claim_and_finalizing_it_makes_one() {
    let fx = Fx::new().await;

    let owner = fx.seed_user("owes-draft").await;
    let drafted = fx.organization(&owner, "owes-draft").await;
    fx.drop_projects(&drafted).await;
    let invoice = fx.invoice(&drafted, DRAFT, 3_000, 0).await;
    let blockers = fx.money_blockers(&owner).await;
    assert!(
        !blockers.iter().any(|id| id == &drafted),
        "a draft is not a claim: {blockers:?}"
    );

    fx.finalize(&invoice, 3_000).await;
    assert_refused(&fx, &owner, &drafted, 3_000).await;

    common::drain_pg().await;
}

/// An organization that owes nothing at all is allowed at every point, and the
/// paired debtor proves the fixture can produce a refusal on the same shape.
#[compio::test]
async fn an_organization_owing_nothing_is_allowed_everywhere() {
    let fx = Fx::new().await;

    let owner = fx.seed_user("owes-clear").await;
    let clear = fx.organization(&owner, "owes-clear").await;
    fx.drop_projects(&clear).await;
    assert_allowed(&fx, &owner, &clear).await;

    let debtor = fx.seed_user("owes-control").await;
    let owing = fx.organization(&debtor, "owes-control").await;
    fx.drop_projects(&owing).await;
    fx.invoice(&owing, FINALIZED, 700, 0).await;
    assert_refused(&fx, &debtor, &owing, 700).await;

    common::drain_pg().await;
}

/// The reaper's re-check, in the state only the reaper can meet.
///
/// The organization is closed FIRST, while it is settled, and the invoice is
/// finalized afterwards - which is what the billing sweep does to a subject
/// that was dissolved mid-month. The ownership rule reports nothing about a
/// dissolved organization by design, so if the money rule did not cover them
/// too, "dissolve, then delete the account" would walk away from the debt
/// through the one door left open.
///
/// The control is the same closed organization with no invoice behind it: a
/// dissolved organization is not a blocker for being dissolved.
#[compio::test]
async fn a_debt_that_appears_after_the_dissolve_still_blocks_the_erasure() {
    let fx = Fx::new().await;

    let owner = fx.seed_user("owes-reaped").await;
    let closed = fx.organization(&owner, "owes-reaped").await;
    fx.drop_projects(&closed).await;
    fx.dissolve(&owner, &closed)
        .await
        .expect("a settled organization closes");
    let blockers = fx.money_blockers(&owner).await;
    assert!(
        !blockers.iter().any(|id| id == &closed),
        "closed and settled: nothing to refuse yet: {blockers:?}"
    );

    // The sweep bills the month the organization was open for.
    fx.invoice(&closed, FINALIZED, 900, 0).await;
    let report = preflight(&fx.pg, &owner, LocalInvoicing::Yes)
        .await
        .expect("preflight");
    assert!(
        report.blockers.is_empty(),
        "the ownership rule must stay silent about a dissolved organization, \
         so this arm rules on the money rule alone: {report:?}"
    );
    let blocker = report
        .billing_blockers
        .iter()
        .find(|b| b.organization_id == closed)
        .unwrap_or_else(|| panic!("a debt after the dissolve must block: {report:?}"));
    assert!(
        blocker.dissolved,
        "the blocker says the organization is closed"
    );
    assert_eq!(blocker.owed_cents, 900);

    common::drain_pg().await;
}

/// Closed-period usage that was never invoiced.
///
/// The rule is about MONEY: usage that prices to nothing is not a debt, and
/// usage that priced to something and never reached an invoice is. The two
/// organizations differ only in the included quota of the plan their app is on,
/// which is exactly the term that decides whether the period is worth anything.
///
/// The zero-priced half is the one a predicate keyed on raw metered UNITS gets
/// wrong: it sees usage, finds no invoice, and refuses to close an account that
/// owes nobody anything.
#[compio::test]
async fn closed_period_usage_blocks_only_when_it_priced_to_money() {
    let fx = Fx::new().await;

    let free_owner = fx.seed_user("owes-usage-free").await;
    let free = fx.organization(&free_owner, "owes-usage-free").await;
    let generous = fx.plan(COVERING_QUOTA).await;
    fx.app_with_usage(&free, &generous, METERED_UNITS, 2).await;
    let blockers = fx.money_blockers(&free_owner).await;
    assert!(
        !blockers.iter().any(|id| id == &free),
        "usage inside the included quota prices to zero and owes nobody: {blockers:?}"
    );

    let billed_owner = fx.seed_user("owes-usage-paid").await;
    let billable = fx.organization(&billed_owner, "owes-usage-paid").await;
    let metered = fx.plan(NO_QUOTA).await;
    fx.app_with_usage(&billable, &metered, METERED_UNITS, 2)
        .await;
    let blockers = fx.money_blockers(&billed_owner).await;
    assert!(
        blockers.iter().any(|id| id == &billable),
        "priced usage that never reached an invoice is a debt: {blockers:?}"
    );

    // Both organizations still own an app, so `dissolve` answers the PROJECTS
    // rider for them. What this asserts at that point is which refusal comes
    // out: the money rule is asked first, so the billable one must cite money
    // and the free one must not.
    match fx.dissolve(&billed_owner, &billable).await {
        Err(OrganizationError::OrganizationOwesBilling(_)) => {}
        other => panic!("dissolve must cite the money rule: {other:?}"),
    }
    if let Err(OrganizationError::OrganizationOwesBilling(o)) =
        fx.dissolve(&free_owner, &free).await
    {
        panic!("a zero-priced period must not refuse the close: {o:?}")
    }

    common::drain_pg().await;
}

/// Invoicing the period clears it, which is what makes the usage arm a claim
/// about BILLING rather than about usage existing. The pair differs in whether
/// an invoice covers the period.
#[compio::test]
async fn invoicing_the_period_clears_the_unbilled_usage_blocker() {
    let fx = Fx::new().await;
    let metered = fx.plan(NO_QUOTA).await;

    let uninvoiced_owner = fx.seed_user("owes-usage-open").await;
    let uninvoiced = fx.organization(&uninvoiced_owner, "owes-usage-open").await;
    fx.app_with_usage(&uninvoiced, &metered, METERED_UNITS, 2)
        .await;
    let blockers = fx.money_blockers(&uninvoiced_owner).await;
    assert!(
        blockers.iter().any(|id| id == &uninvoiced),
        "no invoice covers the period: {blockers:?}"
    );

    let invoiced_owner = fx.seed_user("owes-usage-billed").await;
    let invoiced = fx.organization(&invoiced_owner, "owes-usage-billed").await;
    fx.app_with_usage(&invoiced, &metered, METERED_UNITS, 2)
        .await;
    // The invoice sits in the SAME period the usage is in, and is paid, so the
    // invoice arm is silent too.
    let invoice = fx.invoice(&invoiced, FINALIZED, 500, 0).await;
    fx.pay(&invoice, 500, CHARGE).await;
    let blockers = fx.money_blockers(&invoiced_owner).await;
    assert!(
        !blockers.iter().any(|id| id == &invoiced),
        "the period was billed and the invoice paid: {blockers:?}"
    );

    common::drain_pg().await;
}

/// The unbilled arm is asked only of a stack that owns the local invoice rail.
/// Under `No` there is no local invoice that could be missing, so the same
/// organization that blocks under `Yes` must not block - and the invoice arm,
/// which reads rows rather than an absence, must keep blocking under both.
#[compio::test]
async fn the_unbilled_arm_belongs_to_the_local_invoicer_and_the_invoice_arm_to_both() {
    let fx = Fx::new().await;
    let metered = fx.plan(NO_QUOTA).await;

    let usage_owner = fx.seed_user("owes-stack-usage").await;
    let usage_only = fx.organization(&usage_owner, "owes-stack-usage").await;
    fx.app_with_usage(&usage_only, &metered, METERED_UNITS, 2)
        .await;
    assert!(
        preflight(&fx.pg, &usage_owner, LocalInvoicing::Yes)
            .await
            .expect("preflight")
            .billing_blockers
            .iter()
            .any(|b| b.organization_id == usage_only),
        "the local invoicer asks about unbilled usage"
    );
    assert!(
        preflight(&fx.pg, &usage_owner, LocalInvoicing::No)
            .await
            .expect("preflight")
            .billing_blockers
            .is_empty(),
        "a stack that does not own the invoice rail cannot miss an invoice"
    );

    let invoice_owner = fx.seed_user("owes-stack-invoice").await;
    let invoice_only = fx.organization(&invoice_owner, "owes-stack-invoice").await;
    fx.drop_projects(&invoice_only).await;
    fx.invoice(&invoice_only, FINALIZED, 250, 0).await;
    for invoicing in [LocalInvoicing::Yes, LocalInvoicing::No] {
        assert!(
            preflight(&fx.pg, &invoice_owner, invoicing)
                .await
                .expect("preflight")
                .billing_blockers
                .iter()
                .any(|b| b.organization_id == invoice_only),
            "an unpaid invoice owes under {invoicing:?} too"
        );
    }

    common::drain_pg().await;
}

/// A VOID over a period that carries usage answers the same as a plain void.
///
/// The two arms used to contradict each other over one act. Voiding releases
/// the claim, so the invoice arm falls silent - and the unbilled arm, which
/// looked for a NON-VOID invoice, then saw a closed period with usage and no
/// invoice it would count, and refused. The organization moved from owing on an
/// invoice to owing on unbilled usage, and no act of anyone's could clear it.
///
/// A void is a deliberate statement that the period is done, not evidence that
/// it was never billed. The pair below differs in the void alone: the same
/// organization, the same usage, the same finalized invoice.
#[compio::test]
async fn a_void_over_a_period_with_usage_agrees_with_the_plain_void() {
    let fx = Fx::new().await;
    let metered = fx.plan(NO_QUOTA).await;

    let standing_owner = fx.seed_user("owes-void-usage-standing").await;
    let standing = fx
        .organization(&standing_owner, "owes-void-usage-standing")
        .await;
    fx.app_with_usage(&standing, &metered, METERED_UNITS, 2)
        .await;
    fx.invoice(&standing, FINALIZED, 800, 0).await;
    // The control: the invoice stands over the same usage, and it refuses.
    assert!(
        fx.money_blockers(&standing_owner)
            .await
            .iter()
            .any(|id| id == &standing),
        "a standing invoice over the period is a debt"
    );

    let voided_owner = fx.seed_user("owes-void-usage").await;
    let voided = fx.organization(&voided_owner, "owes-void-usage").await;
    fx.app_with_usage(&voided, &metered, METERED_UNITS, 2).await;
    let released = fx.invoice(&voided, FINALIZED, 800, 0).await;
    fx.void(&released).await;
    let blockers = fx.money_blockers(&voided_owner).await;
    assert!(
        !blockers.iter().any(|id| id == &voided),
        "a void releases the period; the usage arm must not re-open it: {blockers:?}"
    );

    common::drain_pg().await;
}

/// A DRAFT over a period that owed money must not settle it.
///
/// This is the shape a crashed reconcile leaves behind: the row is claimed
/// before any provider call, so a draft means billing STARTED and did not
/// finish. It is not a claim - every amount on it is zero by CHECK - so the
/// invoice arm cannot see it. If it also counted as "this period was billed",
/// the organization produced no blocker of any kind over a real debt and could
/// be closed and its sole owner erased.
///
/// One organization, one variable: the SAME row reaches finalize and is paid.
#[compio::test]
async fn a_draft_invoice_does_not_settle_the_period_it_claimed() {
    let fx = Fx::new().await;
    let metered = fx.plan(NO_QUOTA).await;

    let owner = fx.seed_user("owes-draft-stuck").await;
    let stuck = fx.organization(&owner, "owes-draft-stuck").await;
    fx.app_with_usage(&stuck, &metered, METERED_UNITS, 2).await;
    let claimed = fx.invoice(&stuck, DRAFT, PRICED_PERIOD_CENTS, 0).await;
    assert_unbilled_refused(&fx, &owner, &stuck, PRICED_PERIOD_CENTS).await;

    // Finalizing is what turns the claim into one, and the cash then clears it.
    // Nothing else about the organization moves.
    fx.finalize(&claimed, PRICED_PERIOD_CENTS).await;
    fx.pay(&claimed, PRICED_PERIOD_CENTS, CHARGE).await;
    assert_money_clear(&fx, &owner, &stuck).await;

    common::drain_pg().await;
}

/// A draft sends the period back to the PRICER, it does not assert a debt.
///
/// The opposite failure is the more expensive one: a predicate that refused
/// every organization holding any draft would make the ordinary free-tier month
/// permanently undeletable, since the reconciler claims a row for a period it
/// will price to nothing too. The pair differs in the included quota alone -
/// the one term that decides whether the usage was worth anything.
#[compio::test]
async fn a_draft_sends_the_period_to_the_pricer_rather_than_asserting_a_debt() {
    let fx = Fx::new().await;

    let free_owner = fx.seed_user("owes-draft-free").await;
    let free = fx.organization(&free_owner, "owes-draft-free").await;
    let generous = fx.plan(COVERING_QUOTA).await;
    fx.app_with_usage(&free, &generous, METERED_UNITS, 2).await;
    fx.invoice(&free, DRAFT, 0, 0).await;
    assert_money_clear(&fx, &free_owner, &free).await;

    let billable_owner = fx.seed_user("owes-draft-billable").await;
    let billable = fx
        .organization(&billable_owner, "owes-draft-billable")
        .await;
    let metered = fx.plan(NO_QUOTA).await;
    fx.app_with_usage(&billable, &metered, METERED_UNITS, 2)
        .await;
    fx.invoice(&billable, DRAFT, 0, 0).await;
    assert_unbilled_refused(&fx, &billable_owner, &billable, PRICED_PERIOD_CENTS).await;

    common::drain_pg().await;
}

/// The unbilled arm prices the organization's WHOLE app roster, not the apps
/// that accrued.
///
/// That is what the reconciler does - its per-organization path resolves apps
/// through `owned_app_ids` and its usage prefilter only picks which
/// organizations a sweep visits - and the claim was carried in prose alone, so
/// narrowing this read to accruers changed no test.
///
/// The organization below is built so the ACCRUING app contributes nothing: its
/// usage sits inside the included quota. Everything the period comes to is the
/// standing charge of an app that never served a request, and an app set
/// narrowed to accruers cannot see that app at all. The control differs in that
/// idle app's base fee and in nothing else.
#[compio::test]
async fn a_non_accruing_app_on_a_base_fee_plan_is_priced_into_the_period() {
    let fx = Fx::new().await;
    let covered = fx.plan(COVERING_QUOTA).await;
    let standing_charge = fx.plan_priced(BASE_FEE_CENTS, NO_QUOTA).await;
    let no_standing_charge = fx.plan_priced(0, NO_QUOTA).await;

    let owner = fx.seed_user("owes-roster-base").await;
    let charged = fx.organization(&owner, "owes-roster-base").await;
    fx.app_with_usage(&charged, &covered, METERED_UNITS, 2)
        .await;
    fx.app(&charged, &standing_charge).await;
    assert_unbilled_refused(&fx, &owner, &charged, BASE_FEE_CENTS).await;

    let control_owner = fx.seed_user("owes-roster-free").await;
    let uncharged = fx.organization(&control_owner, "owes-roster-free").await;
    fx.app_with_usage(&uncharged, &covered, METERED_UNITS, 2)
        .await;
    fx.app(&uncharged, &no_standing_charge).await;
    assert_money_clear(&fx, &control_owner, &uncharged).await;

    common::drain_pg().await;
}

/// Which remedy an unbilled period names, and the one variable it turns on.
///
/// A refusal has to name something that would change the answer. With no
/// payment identity the creator's attachment is the missing piece and nothing
/// bills without it. With one already on file, attaching a card again does
/// nothing: the period was simply never invoiced, and the automatic sweep only
/// ever bills the immediately previous month, so an operator has to reconcile
/// THAT period. Telling the second creator to add a card they already have
/// would read as progress and produce none.
#[compio::test]
async fn the_unbilled_remedy_turns_on_whether_a_payment_identity_exists() {
    let fx = Fx::new().await;
    let metered = fx.plan(NO_QUOTA).await;

    let cardless_owner = fx.seed_user("owes-remedy-cardless").await;
    let cardless = fx
        .organization(&cardless_owner, "owes-remedy-cardless")
        .await;
    fx.app_with_usage(&cardless, &metered, METERED_UNITS, 2)
        .await;
    assert_eq!(
        fx.remedy(&cardless_owner, &cardless).await,
        BillingRemedy::AttachPaymentMethod,
        "no customer: nothing bills or collects until one is attached"
    );

    let carded_owner = fx.seed_user("owes-remedy-carded").await;
    let carded = fx.organization(&carded_owner, "owes-remedy-carded").await;
    fx.app_with_usage(&carded, &metered, METERED_UNITS, 2).await;
    fx.payment_identity(&carded).await;
    assert_eq!(
        fx.remedy(&carded_owner, &carded).await,
        BillingRemedy::ReconcileClosedPeriod,
        "customer on file: the invoice is the only missing piece"
    );

    common::drain_pg().await;
}

/// A VOID row must not answer for a period that also holds a DRAFT.
///
/// This is the residual half of the draft defect and it is reachable rather
/// than theoretical: `void_reissue::void_and_reissue` commits the void in its
/// own transaction before writing the replacement, so a period legitimately
/// holds a void row and a draft at the same time. A settled-set test written as
/// one `NOT EXISTS` lets the void speak for the period and hides the draft's
/// money - the same shape as the bare row-existence test it replaced, one
/// status along.
///
/// The CONTROL is the same shape with the void alone. Void really does settle a
/// period, so a predicate that refused this pair by refusing every void would
/// pass the second half while breaking the operator's correction path.
#[compio::test]
async fn a_void_does_not_answer_for_a_period_that_still_holds_a_draft() {
    let fx = Fx::new().await;
    let metered = fx.plan(NO_QUOTA).await;

    // The CONTROL first: voided and nothing pending. Nothing is owed.
    let settled_owner = fx.seed_user("owes-void-only").await;
    let settled = fx.organization(&settled_owner, "owes-void-only").await;
    fx.app_with_usage(&settled, &metered, METERED_UNITS, 2)
        .await;
    let withdrawn = fx
        .invoice(&settled, FINALIZED, PRICED_PERIOD_CENTS, 0)
        .await;
    fx.void(&withdrawn).await;
    assert_money_clear(&fx, &settled_owner, &settled).await;

    // The reissue, caught mid-flight: the void has committed and the
    // replacement is still a draft, so the money is real and unclaimed.
    let owner = fx.seed_user("owes-void-then-draft").await;
    let reissuing = fx.organization(&owner, "owes-void-then-draft").await;
    fx.app_with_usage(&reissuing, &metered, METERED_UNITS, 2)
        .await;
    let wrong = fx
        .invoice(&reissuing, FINALIZED, PRICED_PERIOD_CENTS, 0)
        .await;
    fx.void(&wrong).await;
    fx.invoice(&reissuing, DRAFT, 0, 0).await;
    assert_unbilled_refused(&fx, &owner, &reissuing, PRICED_PERIOD_CENTS).await;

    common::drain_pg().await;
}
