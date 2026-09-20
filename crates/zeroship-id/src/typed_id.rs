//! Typed IDs with an entity prefix and fixed-width lowercase Base36 body.
//!
//! Format: `{prefix}_{base36(uuid)}` — e.g.
//! `usr_0000000002e4nenowz3qmamtd`.
//!
//! - Minted IDs use UUIDv7 and sort by creation time under bytewise collation
//! - Base36: `0-9a-z`, 25 chars for 128 bits, single-case
//! - Stored values use the complete typed-ID string rather than a raw UUID

/// Base36 alphabet — ascending in byte value, so under a BYTE-ordering
/// collation lexicographic order matches numeric order for the high bits
/// (the UUIDv7 timestamp) and ids sort by creation time.
///
/// # It is single-case on purpose, and that is a correctness property
///
/// An app id IS a PostgreSQL schema name, a DNS label, and a scope segment in
/// the migration policy language. Every one of those folds case: PostgreSQL
/// lowercases an unquoted identifier, DNS is case-insensitive, and the policy
/// language models PostgreSQL. A mixed-case body therefore has two spellings
/// wherever it is written unquoted, and the two are compared in different
/// places by different rules.
///
/// That is not hypothetical. Under a mixed-case body the migration policy
/// folded the scope while the grant lookup used raw bytes, so the two never
/// matched and EVERY creator `createTable` was denied - measured at both
/// spellings of one id. A single-case alphabet removes the class rather than
/// patching the sites: there is one spelling, so folding is the identity.
///
/// # The collation contract still stands
///
/// Byte-ascending is necessary but not sufficient: it holds under SQLite
/// BINARY and PostgreSQL `COLLATE "C"`, and NOT under a locale default like
/// `en_US.utf8`, which does not order ASCII bytewise. Every column holding one
/// of these ids therefore pins `COLLATE "C"`, including the foreign-key copies
/// nothing orders - a join against a collated id cannot use a copy's index
/// when the two collations differ, and that degrades silently rather than
/// erroring. The encoder alone cannot give a database this guarantee.
const BASE36: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";

/// Body width of an encoded 128-bit id.
///
/// Padding to a fixed width preserves numeric order under bytewise collation.
pub const BODY_LEN: usize = 25;
/// Reverse lookup table: ASCII byte → base36 digit (255 = invalid)
const fn build_decode_table() -> [u8; 128] {
    let mut table = [255u8; 128];
    let mut i = 0;
    while i < 36 {
        table[BASE36[i] as usize] = i as u8;
        i += 1;
    }
    table
}

const DECODE: [u8; 128] = build_decode_table();

/// Encode 128-bit UUID bytes as a fixed-width 25-char base36 string.
///
/// Twenty-five is the minimum width that holds 128 bits: 36^24 < 2^128 <= 36^25.
/// Fixed width is what makes the encoding order-preserving - a shorter id would
/// sort before a longer one whatever their numeric values.
pub fn uuid_to_base36(uuid: &uuid::Uuid) -> String {
    let bytes = uuid.as_bytes();
    // Treat as a 128-bit big-endian integer and repeatedly divide by 36.
    let mut n = u128::from_be_bytes(*bytes);
    let mut buf = [0u8; BODY_LEN];
    for i in (0..BODY_LEN).rev() {
        buf[i] = BASE36[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8(buf.to_vec()).expect("base36 chars are valid UTF-8")
}

/// Encode an arbitrary byte slice as a base36 string by treating it as a
/// big-endian integer and repeatedly dividing by 36.
///
/// Unlike [`uuid_to_base36`] (fixed 25-char width for a 128-bit UUID), this
/// handles inputs of any length, so it can encode an HMAC tag. The output
/// length is not fixed; callers that want a bounded id should truncate the
/// returned string (e.g. the pairwise-subject derivation takes the first 20
/// chars). Empty input yields an empty string.
#[must_use]
pub fn base36_encode_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    // Big-endian byte-array long division by 36, collecting remainders.
    let mut digits = bytes.to_vec();
    let mut out = Vec::new();
    // Strip leading zero bytes only after the loop preserves value; we loop
    // until the running number is zero.
    loop {
        let mut rem: u16 = 0;
        let mut all_zero = true;
        for d in &mut digits {
            let cur = (rem << 8) | u16::from(*d);
            let q = cur / 36;
            rem = cur % 36;
            *d = u8::try_from(q).unwrap_or(0);
            if *d != 0 {
                all_zero = false;
            }
        }
        out.push(BASE36[rem as usize]);
        if all_zero {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("base36 chars are valid UTF-8")
}

/// Decode a 25-char base36 string to UUID bytes.
pub fn base36_to_uuid(s: &str) -> Result<uuid::Uuid, String> {
    if s.len() != BODY_LEN {
        return Err(format!("expected {BODY_LEN} base36 chars, got {}", s.len()));
    }
    let mut n: u128 = 0;
    for &b in s.as_bytes() {
        if b >= 128 {
            return Err(format!("invalid base36 character: {}", b as char));
        }
        let digit = DECODE[b as usize];
        if digit == 255 {
            return Err(format!("invalid base36 character: {}", b as char));
        }
        n = n.checked_mul(36)
            .and_then(|n| n.checked_add(digit as u128))
            .ok_or_else(|| "base36 overflow".to_string())?;
    }
    Ok(uuid::Uuid::from_bytes(n.to_be_bytes()))
}

/// Generate a new UUIDv7 (timestamp-ordered).
pub fn new_v7() -> uuid::Uuid {
    uuid::Uuid::now_v7()
}

/// Generate a typed ID: `{prefix}_{base36(uuidv7)}`
pub fn generate(prefix: &str) -> String {
    let uuid = new_v7();
    format!("{}_{}", prefix, uuid_to_base36(&uuid))
}

/// Parse a typed ID: extract the prefix and decode to UUID.
pub fn parse(typed_id: &str) -> Result<(&str, uuid::Uuid), String> {
    let (prefix, encoded) = typed_id
        .split_once('_')
        .ok_or_else(|| format!("invalid typed ID (no prefix): {typed_id}"))?;
    let uuid = base36_to_uuid(encoded)?;
    Ok((prefix, uuid))
}

/// Parse error for [`parse_with_prefix`]. Distinguishes a wrong-prefix
/// boundary check from a malformed-id parse error so a caller can map
/// the two onto distinct error variants — a prefix mismatch is a
/// rejected boundary crossing, a malformed id is bad input — without
/// losing the underlying detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// The id parsed cleanly but its prefix did not match the expected
    /// entity-type prefix. Used by `parse_with_prefix` as the
    /// path-traversal-hardening boundary check.
    WrongPrefix { expected: String, got: String },
    /// The id failed to parse — wrong shape, invalid base36, missing
    /// underscore, etc. Carries the same string the underlying [`parse`]
    /// would have returned.
    Malformed(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongPrefix { expected, got } => {
                write!(f, "expected prefix '{expected}', got '{got}'")
            }
            Self::Malformed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a typed ID and assert its prefix matches `expected_prefix`.
///
/// Layered safety check on top of [`parse`]. A caller that knows the
/// entity type of the id it is about to receive uses this helper to
/// refuse mismatched prefixes BEFORE the value reaches any downstream
/// wire (SQL, filesystem path, HTTP header). The path-traversal
/// hardening rule this encodes: an id-shaped parameter is never trusted
/// as a path segment until its prefix has been checked, so a caller
/// cannot smuggle `..` — or another entity type's id — through it.
///
/// Returns the embedded UUID on success.
pub fn parse_with_prefix(
    typed_id: &str,
    expected_prefix: &str,
) -> Result<uuid::Uuid, ParseError> {
    let (got, uuid) = parse(typed_id).map_err(ParseError::Malformed)?;
    if got != expected_prefix {
        return Err(ParseError::WrongPrefix {
            expected: expected_prefix.to_string(),
            got: got.to_string(),
        });
    }
    Ok(uuid)
}

/// Strip the prefix and decode to UUID string (hyphenated).
pub fn to_uuid_string(typed_id: &str) -> Result<String, String> {
    let (_, uuid) = parse(typed_id)?;
    Ok(uuid.to_string())
}

/// Encode a UUID string (hyphenated) to typed ID with the given prefix.
pub fn from_uuid_string(prefix: &str, uuid_str: &str) -> Result<String, String> {
    let uuid = uuid::Uuid::parse_str(uuid_str)
        .map_err(|e| format!("invalid UUID: {e}"))?;
    Ok(format!("{}_{}", prefix, uuid_to_base36(&uuid)))
}

// ---------------------------------------------------------------------------
// Well-known prefixes
// ---------------------------------------------------------------------------

pub const USER_PREFIX: &str = "usr";
/// App entity typed-id prefix. There is no `new_app_id` free function beside
/// the other `new_*_id` minters: an app id's ONLY minter is
/// [`crate::app_id::AppId::mint`], because the value seeds the per-app schema
/// name, both role names, the publication digest and the encryption salt, and a
/// `String` returned from here would reach all of them with no type saying
/// which of those it was.
pub const APP_PREFIX: &str = "app";

/// Immutable normal app deployment identity.
pub const DEPLOYMENT_PREFIX: &str = "dep";

/// Normal deploy command identity.
///
/// The client sends it as the deploy request's `Idempotency-Key`; retries reuse
/// it and a new deploy, including a rollback to an earlier artifact, mints
/// another. Minted only by [`crate::deploy_command::DeployCommandId::mint`].
pub const DEPLOY_COMMAND_PREFIX: &str = "dcm";

/// Control's lifecycle publication intent: one row per app lifecycle revision,
/// delivered to the workflow manager in revision order.
pub const LIFECYCLE_INTENT_PREFIX: &str = "lci";

/// Generate a new lifecycle intent ID: `lci_{base36(uuidv7)}`.
#[must_use]
pub fn new_lifecycle_intent_id() -> String {
    generate(LIFECYCLE_INTENT_PREFIX)
}

pub const SESSION_PREFIX: &str = "ses";

/// Grant typed-id prefix: one row per (person, audience) in `zeroship.grants`,
/// the object a session hangs off and the place the person's subject for that
/// audience is stored.
///
/// `grt`, not `grn`: three characters like every other prefix, and the three
/// that read as the word. It is disjoint from every prefix above and below,
/// including `org`/`prj`, which is what keeps a mis-typed id unresolvable
/// rather than resolvable against the wrong table.
pub const GRANT_PREFIX: &str = "grt";

/// Organization entity typed-id prefix: the ownership root and the billing
/// subject. Three chars like every other prefix, and deliberately the
/// abbreviation even though the COLUMN name is spelled out in full
/// (`organization_id`, never `org_id`): the abbreviation lives inside an opaque
/// value nobody reads as a word, while a column name is prose read constantly.
///
/// Minted only by [`crate::organization_id::OrganizationId::mint`]; there is no
/// `new_organization_id` free function, for the reason [`APP_PREFIX`] gives.
pub const ORGANIZATION_PREFIX: &str = "org";

/// Project entity typed-id prefix: the shared-infrastructure boundary and, under
/// the auth foundation's audience sum, the unit an end-user subject is scoped
/// to.
///
/// Minted only by [`crate::project_id::ProjectId::mint`].
pub const PROJECT_PREFIX: &str = "prj";

/// Organization-invite typed-id prefix. The invite row is addressed by this id;
/// the SECRET a recipient presents is a separate high-entropy token stored only
/// as a hash, never this value.
pub const INVITE_PREFIX: &str = "ivt";

/// Datastore typed-id prefix: one `PostgreSQL` cluster, operator-owned, in one
/// execution zone. Creators never name one and never see one.
///
/// The ROW is keyed on the cluster's own `pg_control_system().system_identifier`
/// rather than on this id, so that two services configured against one cluster
/// converge on one row. This id is the stable handle everything else references.
pub const DATASTORE_PREFIX: &str = "dst";

/// Database typed-id prefix: one schema inside a [`DATASTORE_PREFIX`] cluster,
/// owned by a project. The unit that is migrated, bound and dropped.
///
/// It names the physical schema (`db_<id>`), the migrator and capability roles,
/// the apply lock and the encryption salt - which is why it must not be reused
/// for anything whose lifetime differs from the schema's.
pub const DATABASE_PREFIX: &str = "dbs";

/// Binding typed-id prefix: the edge joining one app to one database with one
/// capability.
///
/// `bnd`, NOT `grt`. [`GRANT_PREFIX`] already means one row per (person,
/// audience) in `zeroship.grants`, and putting a data-access edge in that
/// namespace is exactly what the disjointness rule exists to prevent.
///
/// The edge carries its own id because the `PostgreSQL` role name is derived
/// from it (`zs_bind_<id>_e<epoch>`); a composite natural key would put two ids
/// in one identifier.
pub const BINDING_PREFIX: &str = "bnd";

/// Per-app OAuth `client_id` prefix: the
/// deterministic, stable-for-app-life OAuth client id is `oac_<base36-app-id>`.
/// Distinct from [`APP_PREFIX`] (the app *entity* typed_id) on purpose — the
/// OAuth `client_id` is a derived identifier, not a typed_id.
///
/// This is the SINGLE source of truth for the prefix string. The control plane
/// mints the id ([`app_oauth_client_id`]) and the auth consent classifier
/// decodes it back to the app UUID ([`app_id_from_oauth_client_id`]); both go
/// through this constant so they can never drift.
pub const APP_OAUTH_CLIENT_PREFIX: &str = "oac";

/// Pricing-plan typed-id prefix. Three chars to preserve the common
/// `^[a-z]{3}_[0-9a-z]{25}$` shape. The `zeroship.plans.id` column stores
/// the full typed-id string (`pln_<base36>`); `apps.plan_id` is an FK into it,
/// preventing free-text self-escalation. The catalog mints ids via
/// [`new_plan_id`] and the built-in tiers are seeded with real `pln_…` ids
/// at control bootstrap.
pub const PLAN_PREFIX: &str = "pln";

/// Invoice typed-id prefix (billing schema redesign). Three chars to match the
/// global `^[a-z]{3}_[0-9a-z]{25}$` shape every other entity uses
/// (R16-API2). The `zeroship.invoices.id` column stores the full typed-id
/// string (`inv_<base36>`); the provider-ref + line side tables FK into it.
pub const INVOICE_PREFIX: &str = "inv";

/// Invoice-payment typed-id prefix. Three chars to
/// match the global `^[a-z]{3}_[0-9a-z]{25}$` shape every other entity uses
/// (R16-API2). `ipy` (NOT the design's 4-char `ipay`, which would break the
/// 3-char invariant) and disjoint from `inv` so a payment id can never be
/// confused with the invoice it FKs into. The `zeroship.invoice_payments.id`
/// column stores the full typed-id string (`ipy_<base36>`), minted in Rust by
/// the payment-confirmation webhook (no SQL `DEFAULT` — there is no in-DB base36
/// generator).
pub const INVOICE_PAYMENT_PREFIX: &str = "ipy";

/// Credit-ledger typed-id prefix. Three chars to
/// match the global `^[a-z]{3}_[0-9a-z]{25}$` shape every other entity uses
/// (R16-API2), and disjoint from `inv`/`ipy` so a credit-entry id can never be
/// confused with the invoice it relates to. The `zeroship.credit_ledger.id`
/// column stores the full typed-id string (`crd_<base36>`), minted in Rust by the
/// operator grant endpoint and the reconciler's per-grant `consumed` writes (no
/// SQL `DEFAULT` — there is no in-DB base36 generator).
pub const CREDIT_PREFIX: &str = "crd";

/// Refund typed-id prefix. Three chars to match the
/// global `^[a-z]{3}_[0-9a-z]{25}$` shape every other entity uses
/// (R16-API2), and disjoint from `inv`/`ipy`/`crd` so a refund id can never be
/// confused with the invoice it FKs into or the credit grant a `refund_to_credit`
/// refund appends. The `zeroship.refunds.id` column stores the full typed-id
/// string (`ref_<base36>`), minted in Rust by the operator `POST
/// /invoices/{id}/refunds` endpoint + the void+reissue true-up bridge (no SQL
/// `DEFAULT` — there is no in-DB base36 generator).
pub const REFUND_PREFIX: &str = "ref";

/// Plan-change-event typed-id prefix (full usage-segment
/// proration). Three chars to match the global `^[a-z]{3}_[0-9a-z]{25}$` shape
/// every other entity uses (R16-API2), and disjoint from `inv`/`ipy`/`crd`/`ref`/`pln`
/// so a plan-change-event id can never be confused with the plan it names or the
/// invoice line its segment becomes. The `zeroship.plan_change_events.id` column
/// stores the full typed-id string (`pce_<base36>`), minted in Rust by `set_plan`
/// when it appends a proration-timeline row (no SQL `DEFAULT` — there is no in-DB
/// base36 generator).
pub const PLAN_CHANGE_EVENT_PREFIX: &str = "pce";

/// Spend-state-history surrogate-id prefix.
/// Three chars to match the global `^[a-z]{3}_[0-9a-z]{25}$` shape (R16-API2). The
/// `zeroship.spend_state_history.id` column stores the full typed-id string
/// (`she_<base36>`), minted in Rust by `spend.rs::persist_transition` (no SQL
/// `DEFAULT` — there is no in-DB base36 generator, and a bare `gen_random_uuid()`
/// would not carry the `she_` prefix the notify dedup key relies on).
///
/// The surrogate id IS the `billing_notifications.transition_id` for spend-driven
/// notification kinds. Its prefix MUST be pairwise-disjoint from every other
/// notification source (`obh`/`inv`/`ref`/`dsp`) so a `transition_id` from one source
/// can never collide with another's in the send-ledger dedup key — asserted by
/// `tests::notification_source_prefixes_are_pairwise_disjoint`.
///
/// That name is a code span, not an intra-doc link, and must stay one: `tests`
/// is `#[cfg(test)]`, so rustdoc does not compile it when documenting and a
/// `[...]` link there resolves to nothing. Five of them did, and reported as
/// broken links on every doc build.
pub const SPEND_HISTORY_PREFIX: &str = "she";

/// Organization-billing-status-history surrogate-id prefix. Three chars to match the global `^[a-z]{3}_[0-9a-z]{25}$` shape
/// (R16-API2). The `zeroship.organization_billing_status_history.id` column stores the full
/// typed-id string (`obh_<base36>`), minted in Rust by `account_status.rs::append_history`
/// (no SQL `DEFAULT` — see [`SPEND_HISTORY_PREFIX`]).
///
/// The surrogate id IS the `billing_notifications.transition_id` for the dunning-driven
/// kinds (`payment_failed`/`past_due`/`suspended`/`recovered`). Its prefix MUST be
/// pairwise-disjoint from every other notification source — see [`SPEND_HISTORY_PREFIX`].
pub const ORGANIZATION_BILLING_HISTORY_PREFIX: &str = "obh";

/// Billing-dispute typed-id prefix.
/// Three chars to match the global `^[a-z]{3}_[0-9a-z]{25}$` shape (R16-API2). The
/// `zeroship.billing_disputes.id` column stores the full typed-id string (`dsp_<base36>`),
/// minted in Rust by the `charge.dispute.created` webhook branch.
///
/// The id IS the `billing_notifications.transition_id` for the `disputed` kind. Its
/// prefix MUST be pairwise-disjoint from every other notification source
/// (`she`/`obh`/`inv`/`ref`) so a `transition_id` from one source can never collide with
/// another's in the send-ledger dedup key — asserted by
/// `tests::notification_source_prefixes_are_pairwise_disjoint`. Distinct from the
/// Stripe-side dispute id (`du_…`/`dp_…`), which is a provider ref, not a typed_id.
pub const DISPUTE_PREFIX: &str = "dsp";

/// Mint the per-app OAuth `client_id` for an app: `oac_<body>`.
///
/// The client id carries the app id's OWN BODY, not a re-encoding of its bits.
/// That is what lets [`app_id_from_oauth_client_id`] hand back an `AppId` whose
/// printed form is byte-identical to the one the app was minted with - and a
/// derived audience (`app:<printed id>`) therefore agrees with whoever built it
/// from the app id directly. Deterministic and stable for the life of the app.
#[must_use]
pub fn app_oauth_client_id(app_id: &crate::app_id::AppId) -> String {
    let body = app_id.as_str().strip_prefix(APP_PREFIX).and_then(|s| s.strip_prefix('_'))
        .expect("an AppId always prints as app_<body>");
    format!("{APP_OAUTH_CLIENT_PREFIX}_{body}")
}

/// Decode a per-app OAuth `client_id` (`oac_<body>`) back to its
/// [`AppId`](crate::app_id::AppId).
///
/// Returns `None` for any client id that is not a per-app end-user client - a
/// missing `oac_` prefix, or a body that is not a legal app-id body - e.g. the
/// builder/console clients. The exact inverse of [`app_oauth_client_id`].
///
/// It returns the TYPED id rather than the bits, and that is the point: a caller
/// deriving an audience renders `app:<printed id>`, which is what every other
/// producer of that audience renders. Handing back a `Uuid` here is how the
/// gateway came to expect `app:<hyphenated uuid>` while the OP emitted
/// `app:app_<body>` - two spellings of one app, agreeing at compile time and
/// disagreeing only against a live token.
#[must_use]
pub fn app_id_from_oauth_client_id(client_id: &str) -> Option<crate::app_id::AppId> {
    let body = client_id.strip_prefix(APP_OAUTH_CLIENT_PREFIX)?.strip_prefix('_')?;
    crate::app_id::AppId::parse(&format!("{APP_PREFIX}_{body}")).ok()
}

/// Generate a new session ID: `ses_{base36(uuidv7)}`
pub fn new_session_id() -> String {
    generate(SESSION_PREFIX)
}

/// Generate a new grant ID: `grt_{base36(uuidv7)}`. One row per (person,
/// audience) in `zeroship.grants`; the `id` column stores the full typed-id
/// string under a `grants_id_shape` CHECK, with no SQL `DEFAULT` because there
/// is no in-database base36 generator.
pub fn new_grant_id() -> String {
    generate(GRANT_PREFIX)
}

/// Generate a new pricing-plan ID: `pln_{base36(uuidv7)}`. Minted by the
/// plan-catalog `upsert` and by the bootstrap seeder for the built-in
/// tiers.
pub fn new_plan_id() -> String {
    generate(PLAN_PREFIX)
}

/// Generate a new invoice ID: `inv_{base36(uuidv7)}`. Minted by the
/// billing reconciler when it claims a `(organization, period)` invoice row.
pub fn new_invoice_id() -> String {
    generate(INVOICE_PREFIX)
}

/// Generate a new invoice-payment ID: `ipy_{base36(uuidv7)}`. Minted by the
/// payment-confirmation webhook when it appends a `charge` row recording the
/// cash actually collected against a finalized invoice.
pub fn new_invoice_payment_id() -> String {
    generate(INVOICE_PAYMENT_PREFIX)
}

/// Generate a new credit-ledger entry ID: `crd_{base36(uuidv7)}`. Minted by the
/// operator `POST /billing/credit` grant endpoint and by the billing reconciler
/// when it appends a per-grant `consumed` entry at finalize.
pub fn new_credit_id() -> String {
    generate(CREDIT_PREFIX)
}

/// Generate a new refund ID: `ref_{base36(uuidv7)}`. Minted by the operator
/// `POST /api/invoices/{id}/refunds` endpoint and by the void+reissue true-up
/// bridge.
pub fn new_refund_id() -> String {
    generate(REFUND_PREFIX)
}

/// Generate a new plan-change-event ID: `pce_{base36(uuidv7)}`. Minted by
/// `api.rs::set_plan` when it appends a proration-timeline row recording a plan
/// change with its server-derived frozen base fees + cumulative `usage_at_change`
/// snapshot, for full usage-segment proration.
pub fn new_plan_change_event_id() -> String {
    generate(PLAN_CHANGE_EVENT_PREFIX)
}

/// Generate a new spend-state-history surrogate ID: `she_{base36(uuidv7)}`.
pub fn new_spend_history_id() -> String {
    generate(SPEND_HISTORY_PREFIX)
}

/// Generate a new organization-billing-status-history surrogate ID: `obh_{base36(uuidv7)}`.
/// Minted by `account_status.rs::append_history` when it appends a
/// `organization_billing_status_history` row; the value becomes the
/// `billing_notifications.transition_id` for the dunning-driven kinds.
pub fn new_organization_billing_history_id() -> String {
    generate(ORGANIZATION_BILLING_HISTORY_PREFIX)
}

/// Generate a new billing-dispute ID: `dsp_{base36(uuidv7)}`. Minted in Rust by the
/// `charge.dispute.created` webhook branch in `stripe_handlers` when it records a
/// chargeback. The `zeroship.billing_disputes.id` column
/// stores the full typed-id string; the value becomes the
/// `billing_notifications.transition_id` for the `disputed` notification kind (no SQL
/// `DEFAULT` — there is no in-DB base36 generator, and a bare `gen_random_uuid()` would
/// not carry the `dsp_` prefix the notify dedup key + disjointness assertion rely on).
///
/// Distinct from the Stripe-side dispute id (`du_…`/`dp_…`, stored separately in
/// `billing_disputes.provider_dispute_id`): the `dsp_…` is OUR typed id, the `du_…` is
/// Stripe's. Its prefix is pairwise-disjoint from every other notification source
/// (`she`/`obh`/`inv`/`ref`) — see `tests::notification_source_prefixes_are_pairwise_disjoint`.
pub fn new_dispute_id() -> String {
    generate(DISPUTE_PREFIX)
}

/// Payout-failure surrogate-id prefix (billing webhook follow-ups: `payout.failed`).
/// Three chars to match the global `^[a-z]{3}_[0-9a-z]{25}$` shape (R16-API2). The
/// `zeroship.payout_failures.id` column stores the full typed-id string (`pof_<base36>`),
/// minted in Rust by the `payout.failed` webhook branch in `stripe_handlers`.
///
/// The id IS the `billing_notifications.transition_id` for the `payout_failed` kind. Its
/// prefix MUST be pairwise-disjoint from every other notification source
/// (`she`/`obh`/`inv`/`ref`/`dsp`/`cof`) so a `transition_id` from one source can never
/// collide with another's in the send-ledger dedup key — asserted by
/// `tests::notification_source_prefixes_are_pairwise_disjoint`.
pub const PAYOUT_FAILURE_PREFIX: &str = "pof";

/// Generate a new payout-failure ID: `pof_{base36(uuidv7)}`.
pub fn new_payout_failure_id() -> String {
    generate(PAYOUT_FAILURE_PREFIX)
}

/// Connect-checkout-failure surrogate-id prefix (billing webhook follow-ups:
/// `payment_intent.payment_failed`). Three chars to match the global
/// `^[a-z]{3}_[0-9a-z]{25}$` shape (R16-API2). The
/// `zeroship.connect_checkout_failures.id` column stores the full typed-id string
/// (`cof_<base36>`), minted in Rust by the `payment_intent.payment_failed` webhook branch.
///
/// The id IS the `billing_notifications.transition_id` for the `checkout_failed` kind. Its
/// prefix MUST be pairwise-disjoint from every other notification source
/// (`she`/`obh`/`inv`/`ref`/`dsp`/`pof`) — see
/// `tests::notification_source_prefixes_are_pairwise_disjoint`.
pub const CHECKOUT_FAILURE_PREFIX: &str = "cof";

/// Generate a new connect-checkout-failure ID: `cof_{base36(uuidv7)}`.
pub fn new_checkout_failure_id() -> String {
    generate(CHECKOUT_FAILURE_PREFIX)
}

/// Stripe-reconciliation-finding surrogate-id prefix (#28 Stripe reconciliation cron).
/// Three chars to match the global `^[a-z]{3}_[0-9a-z]{25}$` shape (R16-API2). The
/// `zeroship.billing_reconciliation_findings.id` column stores the full typed-id string
/// (`rcf_<base36>`), minted in Rust by the `stripe_reconcile` cron when it records a
/// drift finding. NOT a notification source (it is never a `billing_notifications.
/// transition_id`), so it carries no pairwise-disjointness obligation against the notify
/// source prefixes — it is a plain audit row id.
pub const RECONCILE_FINDING_PREFIX: &str = "rcf";

/// Generate a new reconciliation-finding ID: `rcf_{base36(uuidv7)}`.
pub fn new_reconcile_finding_id() -> String {
    generate(RECONCILE_FINDING_PREFIX)
}

/// Workflow-run typed-id prefix. `zeroship.workflow_runs.id` stores the full
/// `run_<base36>` string because workflow runs are creator-visible handles.
pub const WORKFLOW_RUN_PREFIX: &str = "run";

/// Logical workflow queue job identity, independent of its delivery attempt.
pub const WORKFLOW_JOB_PREFIX: &str = "wjb";

/// Workflow-signal typed-id prefix. `zeroship.workflow_signals.id` stores the
/// full `sig_<base36>` string.
pub const WORKFLOW_SIGNAL_PREFIX: &str = "sig";

/// Workflow cron handle typed-id prefix, distinct from the workflow-schedule prefix.
pub const WORKFLOW_CRON_PREFIX: &str = "cron";

/// Workflow-schedule typed-id prefix: the logical schedule identity
/// ([`crate::workflow::ScheduleId`]) the manager retains across deployment
/// revisions.
pub const WORKFLOW_SCHEDULE_PREFIX: &str = "sch";

/// Workflow dispatch/batch typed-id prefix. Deliberately `wfd`, not `dsp`
/// (billing disputes).
pub const WORKFLOW_DISPATCH_PREFIX: &str = "wfd";

/// Workflow operation retry identity, distinct from a live business key.
pub const WORKFLOW_REQUEST_PREFIX: &str = "wreq";

/// Workflow capability identity for issuance and audit provenance.
pub const WORKFLOW_CAPABILITY_PREFIX: &str = "wcap";

/// A workflow-owned staged payload.
pub const WORKFLOW_PAYLOAD_PREFIX: &str = "wpl";

/// Workflow inbound-signal signing-key typed-id prefix.
pub const WORKFLOW_SIGNAL_KEY_PREFIX: &str = "wsk";

/// Workflow topic-subscription typed-id prefix. Deliberately `wsb`, not `sub`
/// (Stripe subscription ids).
pub const WORKFLOW_SUBSCRIPTION_PREFIX: &str = "wsb";

/// Workflow broadcast typed-id prefix.
pub const WORKFLOW_BROADCAST_PREFIX: &str = "wbc";

/// Creator-owned workflow dependency propagation obligation prefix.
pub const WORKFLOW_PROPAGATION_PREFIX: &str = "wdp";

/// Stateless per-run/per-topic signal capability token prefix. Unlike normal
/// row ids, `wst_…` is not UUID-backed; it encodes signed claims.
/// The claims, the signer and the verifier live in
/// `zeroship_core::workflow_signal_token`: they need HMAC, base64 and JSON,
/// and this crate is a leaf that carries `uuid` and `serde` only. The PREFIX
/// stays here so the reservation table below rules on it with every other one.
pub const WORKFLOW_SIGNAL_TOKEN_PREFIX: &str = "wst";
/// Generate a new workflow-run ID: `run_{base36(uuidv7)}`.
pub fn new_workflow_run_id() -> String {
    generate(WORKFLOW_RUN_PREFIX)
}

/// Generate a new workflow-signal ID: `sig_{base36(uuidv7)}`.
pub fn new_workflow_signal_id() -> String {
    generate(WORKFLOW_SIGNAL_PREFIX)
}

/// Generate a new workflow cron ID: `cron_{base36(uuidv7)}`.
pub fn new_workflow_cron_id() -> String {
    generate(WORKFLOW_CRON_PREFIX)
}

/// Generate a new workflow dispatch/batch ID: `wfd_{base36(uuidv7)}`.
pub fn new_workflow_dispatch_id() -> String {
    generate(WORKFLOW_DISPATCH_PREFIX)
}

/// Generate a new workflow signal-key ID: `wsk_{base36(uuidv7)}`.
pub fn new_workflow_signal_key_id() -> String {
    generate(WORKFLOW_SIGNAL_KEY_PREFIX)
}

/// Generate a new workflow subscription ID: `wsb_{base36(uuidv7)}`.
pub fn new_workflow_subscription_id() -> String {
    generate(WORKFLOW_SUBSCRIPTION_PREFIX)
}

/// Generate a new workflow broadcast ID: `wbc_{base36(uuidv7)}`.
pub fn new_workflow_broadcast_id() -> String {
    generate(WORKFLOW_BROADCAST_PREFIX)
}

/// Provider-dead-letter surrogate-id prefix. Dead-letter rows are operator
/// audit facts, not notification transition sources.
pub const PROVIDER_DEAD_LETTER_PREFIX: &str = "pdl";

/// Generate a new provider-dead-letter ID: `pdl_{base36(uuidv7)}`.
pub fn new_provider_dead_letter_id() -> String {
    generate(PROVIDER_DEAD_LETTER_PREFIX)
}

/// Worker-instance typed-id prefix: one row in `zeroship.worker_instances` per
/// live worker PROCESS. Three chars to match the global
/// `^[a-z]{3}_[0-9a-z]{25}$` shape, and disjoint from every prefix above —
/// notably the `w`-leading workflow family (`wfd`/`wsk`/`wsb`/`wbc`/`wst`).
///
/// MINTED BY CONTROL AT ENROLMENT, never by the registrant. The worker presents
/// its boot-generated Ed25519 public key and its listening port; control assigns
/// the id, the ring key and the address. An instance is a CHILD of the
/// `svc/worker` role: it mints under `svc/worker/<wkr_id>` and is addressed as
/// `svc/worker`.
///
/// Joining authenticates with a JOIN TOKEN a trusted signer (a
/// [`JOIN_SIGNER_PREFIX`] row) minted, plus a signature by the very keypair
/// being registered, so a token alone admits nobody. The id is minted per
/// process and the key never leaves memory, which is what makes per-instance
/// retirement a real boundary here rather than only attribution.
pub const WORKER_INSTANCE_PREFIX: &str = "wkr";

/// Generate a new worker-instance ID: `wkr_{base36(uuidv7)}`. Minted by the
/// control plane when it accepts an enrolment; the `zeroship.worker_instances.id`
/// column stores the full typed-id string under a `worker_instances_id_shape`
/// CHECK, with no SQL `DEFAULT` because there is no in-database base36 generator.
pub fn new_worker_instance_id() -> String {
    generate(WORKER_INSTANCE_PREFIX)
}

/// Join-signer typed-id prefix: one row in `zeroship.worker_join_signers` per
/// trusted signer, each permitted to mint join tokens for a declared set of
/// execution zones.
///
/// MINTED BY THE OPERATOR'S PROVISIONING STEP, never by a worker. The signer's
/// PRIVATE half stays with whoever decides a worker should exist; Control
/// learns the id, the public half and the zones from its import file at
/// startup. A signer mints under `svc/worker-join-signer/<wjs_id>`, and one
/// signer covers as many deployment units as its zones do, so adding a unit is
/// not a Control-side operation.
pub const JOIN_SIGNER_PREFIX: &str = "wjs";

/// Generate a new join-signer ID: `wjs_{base36(uuidv7)}`. The
/// `zeroship.worker_join_signers.id` column stores the full typed-id string
/// under a `worker_join_signers_id_shape` CHECK.
pub fn new_join_signer_id() -> String {
    generate(JOIN_SIGNER_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_base36() {
        let uuid = uuid::Uuid::now_v7();
        let encoded = uuid_to_base36(&uuid);
        assert_eq!(encoded.len(), 25);
        let decoded = base36_to_uuid(&encoded).unwrap();
        assert_eq!(uuid, decoded);
    }

    #[test]
    fn base36_encode_bytes_only_base36_chars() {
        // The HMAC-tag encoder must emit only base36 alphabet chars and be
        // deterministic + injective enough that distinct tags differ.
        let a = base36_encode_bytes(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x02]);
        let b = base36_encode_bytes(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x03]);
        assert_ne!(a, b);
        assert_eq!(a, base36_encode_bytes(&[0xde, 0xad, 0xbe, 0xef, 0x01, 0x02]));
        assert!(a.bytes().all(|c| BASE36.contains(&c)), "{a}");
        assert!(base36_encode_bytes(&[]).is_empty());
        // A full 32-byte HMAC tag yields >= 20 chars (enough to truncate to
        // the pairwise body length).
        let tag = [0xffu8; 32];
        assert!(base36_encode_bytes(&tag).len() >= 20);
    }

    #[test]
    fn roundtrip_typed_id() {
        let id = crate::UserId::mint();
        assert!(id.as_str().starts_with("usr_"));
        assert_eq!(id.as_str().len(), USER_PREFIX.len() + 1 + BODY_LEN);
        let (prefix, uuid) = parse(id.as_str()).unwrap();
        assert_eq!(prefix, "usr");
        let back = from_uuid_string("usr", &uuid.to_string()).unwrap();
        assert_eq!(id.as_str(), back);
    }

    #[test]
    fn app_oauth_client_id_round_trips() {
        let app = crate::app_id::AppId::mint();
        let client_id = app_oauth_client_id(&app);
        assert!(client_id.starts_with("oac_"), "got {client_id}");
        // oac_ + 25 base36 chars.
        assert_eq!(client_id.len(), 29, "got {client_id}");
        // Distinct from the app_ entity typed_id namespace.
        assert!(!client_id.starts_with("app_"));
        // The decode is the exact inverse of the mint — the single-source-of-
        // truth that keeps control (minter) and auth (decoder) from drifting.
        assert_eq!(app_id_from_oauth_client_id(&client_id), Some(app.clone()));
        // Non-per-app clients (builder/console) and malformed tails → None.
        assert_eq!(app_id_from_oauth_client_id("zeroship-builder-abc"), None);
        assert_eq!(app_id_from_oauth_client_id("oac_not-base36"), None);
        // The bare prefix with no underscore must not decode.
        assert_eq!(app_id_from_oauth_client_id("oacsomething"), None);
    }

    #[test]
    fn sort_order_preserved_under_byte_ordering() {
        // IDs generated later should sort after earlier ones.
        //
        // Rust's `>` on `String` is byte order, the same comparison that the
        // schema pins as BINARY on SQLite and C on PostgreSQL.
        let id1 = generate("usr");
        // Small delay to ensure different timestamp
        std::thread::sleep(std::time::Duration::from_millis(2));
        let id2 = generate("usr");
        assert!(id2 > id1, "id2 ({id2}) should sort after id1 ({id1})");
    }

    /// The companion to `sort_order_preserved_under_byte_ordering`: it pins the
    /// two properties of the alphabet everything else rests on.
    ///
    /// **Ascending in byte value**, so a bytewise collation sorts ids by
    /// creation time. This is necessary and not sufficient - it holds under
    /// SQLite BINARY and PostgreSQL `COLLATE "C"`, and NOT under a locale
    /// default, which is why every column holding one of these ids pins the
    /// collation. This test cannot prove what a database does; it proves the
    /// encoder gives the database something a bytewise collation can order.
    ///
    /// **Single-case**, which is a correctness property rather than a style
    /// choice. An app id IS a PostgreSQL schema name, a DNS label, and a scope
    /// segment in the migration policy language, and every one of those folds
    /// case. A mixed-case body therefore has two spellings wherever it is
    /// written unquoted, compared in different places by different rules - and
    /// under the previous mixed-case alphabet the policy scope folded while the
    /// grant lookup used raw bytes, so EVERY creator `createTable` was denied.
    /// With one case there is one spelling and folding is the identity.
    #[test]
    fn the_alphabet_is_ascending_and_single_case() {
        assert!(
            BASE36.windows(2).all(|w| w[0] < w[1]),
            "BASE36 must be ascending in byte value; database ordering relies on it"
        );

        let uppercase: Vec<char> = BASE36
            .iter()
            .filter(|c| c.is_ascii_uppercase())
            .map(|c| *c as char)
            .collect();
        assert!(
            uppercase.is_empty(),
            "BASE36 must contain no uppercase letter; found {uppercase:?}. \
             An id is a schema name, a DNS label and a policy scope segment, and \
             each of those folds case - a second spelling is a second identity."
        );

        // The property that follows, stated over a real id rather than over the
        // alphabet: folding a minted id changes nothing, so any consumer that
        // folds agrees with one that does not.
        let id = generate("usr");
        assert_eq!(
            id.to_ascii_lowercase(),
            id,
            "a minted id must be unchanged by a case fold"
        );
    }

    #[test]
    fn parse_invalid() {
        assert!(parse("nounderscore").is_err());
        assert!(base36_to_uuid("short").is_err());
        assert!(base36_to_uuid("!@#$%^&*()_+{}|:<>?!ab").is_err());
    }

    #[test]
    fn all_prefixes() {
        let u = crate::UserId::mint();
        // The app id has no `new_app_id` free function; its one minter is the
        // typed `AppId`. Swept here anyway so the registry arm stays complete
        // and so APP_PREFIX keeps a caller that proves what it spells.
        let a = crate::app_id::AppId::mint();
        let s = new_session_id();
        let p = new_plan_id();
        assert!(u.as_str().starts_with("usr_"));
        assert!(a.as_str().starts_with("app_"));
        assert!(s.starts_with("ses_"));
        assert!(p.starts_with("pln_"));
        // pln_ + 25 base36 chars = 29, and it round-trips through parse().
        assert_eq!(p.len(), 29);
        assert_eq!(PLAN_PREFIX.len(), 3, "plan prefix must be 3 chars (R16-API2)");
        let (prefix, _) = parse(&p).expect("new_plan_id must roundtrip");
        assert_eq!(prefix, "pln");
    }

    #[test]
    fn invoice_prefix_is_three_chars_and_roundtrips() {
        assert_eq!(INVOICE_PREFIX.len(), 3, "invoice prefix must be 3 chars (R16-API2)");
        let i = new_invoice_id();
        assert!(i.starts_with("inv_"));
        assert_eq!(i.len(), 29, "inv_ + 25 base36 = 29 chars");
        let (prefix, _) = parse(&i).expect("new_invoice_id must roundtrip");
        assert_eq!(prefix, "inv");
    }

    #[test]
    fn invoice_payment_prefix_is_three_chars_and_roundtrips() {
        assert_eq!(
            INVOICE_PAYMENT_PREFIX.len(),
            3,
            "invoice-payment prefix must be 3 chars (R16-API2)"
        );
        let p = new_invoice_payment_id();
        assert!(p.starts_with("ipy_"));
        assert_eq!(p.len(), 29, "ipy_ + 25 base36 = 29 chars");
        let (prefix, _) = parse(&p).expect("new_invoice_payment_id must roundtrip");
        assert_eq!(prefix, "ipy");
        assert_ne!(prefix, INVOICE_PREFIX, "payment id must be disjoint from invoice id");
    }

    #[test]
    fn refund_prefix_is_three_chars_and_disjoint() {
        assert_eq!(REFUND_PREFIX.len(), 3, "refund prefix must be 3 chars (R16-API2)");
        let r = new_refund_id();
        assert!(r.starts_with("ref_"));
        assert_eq!(r.len(), 29, "ref_ + 25 base36 = 29 chars");
        let (prefix, _) = parse(&r).expect("new_refund_id must roundtrip");
        assert_eq!(prefix, "ref");
        // Disjoint from every sibling money-record prefix so a refund id can never
        // be confused with the invoice it FKs into, a payment row, or a credit grant.
        assert_ne!(prefix, INVOICE_PREFIX);
        assert_ne!(prefix, INVOICE_PAYMENT_PREFIX);
        assert_ne!(prefix, CREDIT_PREFIX);
    }

    #[test]
    fn plan_change_event_prefix_is_three_chars_and_disjoint() {
        assert_eq!(
            PLAN_CHANGE_EVENT_PREFIX.len(),
            3,
            "plan-change-event prefix must be 3 chars (R16-API2)"
        );
        let p = new_plan_change_event_id();
        assert!(p.starts_with("pce_"));
        assert_eq!(p.len(), 29, "pce_ + 25 base36 = 29 chars");
        let (prefix, _) = parse(&p).expect("new_plan_change_event_id must roundtrip");
        assert_eq!(prefix, "pce");
        // Disjoint from every sibling money/plan-record prefix so a plan-change-event
        // id can never be confused with the plan it names or the invoice line its
        // segment becomes.
        assert_ne!(prefix, INVOICE_PREFIX);
        assert_ne!(prefix, INVOICE_PAYMENT_PREFIX);
        assert_ne!(prefix, CREDIT_PREFIX);
        assert_ne!(prefix, REFUND_PREFIX);
        assert_ne!(prefix, PLAN_PREFIX);
    }

    #[test]
    fn spend_and_organization_billing_history_prefixes_roundtrip() {
        for (mk, want) in [
            (new_spend_history_id as fn() -> String, "she"),
            (new_organization_billing_history_id as fn() -> String, "obh"),
        ] {
            let id = mk();
            assert!(id.starts_with(&format!("{want}_")), "got {id}");
            assert_eq!(id.len(), 29, "{want}_ + 25 base36 = 29 chars: {id}");
            let (prefix, _) = parse(&id).unwrap_or_else(|e| panic!("{want} id must roundtrip: {e}"));
            assert_eq!(prefix, want);
            assert_eq!(want.len(), 3, "{want} prefix must be 3 chars (R16-API2)");
        }
    }

    /// Regression: the `billing_notifications` dedup key is
    /// `(organization_id, kind, transition_id)`, where `transition_id` is the typed-id of the
    /// SOURCE row (a `she_…` spend-history id, a `obh_…` organization-billing-history id, an
    /// `inv_…` invoice id, a `ref_…` refund id, or a `dsp_…` dispute id). The design's
    /// Cross-source dedup correctness REQUIRES these prefixes be pairwise-disjoint
    /// Every ENTITY prefix is pairwise-disjoint, which several doc comments assert
    /// and nothing checked until this test.
    ///
    /// The property is what makes a mis-typed id UNRESOLVABLE rather than resolvable
    /// against the wrong table: `parse_with_prefix` compares the prefix and rejects a
    /// mismatch, so two entities sharing one would let an id decode successfully into
    /// the wrong type. `bnd` versus `grt` is the live case - the data-access edge and
    /// the auth (person, audience) grant are unrelated entities whose ids would
    /// otherwise be mutually parseable.
    ///
    /// ADDING A PREFIX MEANS ADDING IT HERE. The list is hand-maintained because the
    /// alternative is scanning this file's source for a spelling, which asserts the
    /// text rather than the behaviour. An omission makes this test weaker rather than
    /// red, so treat the list as part of declaring a prefix.
    #[test]
    fn entity_prefixes_are_pairwise_disjoint() {
        let prefixes = [
            ("user", USER_PREFIX),
            ("app", APP_PREFIX),
            ("deployment", DEPLOYMENT_PREFIX),
            ("deploy_command", DEPLOY_COMMAND_PREFIX),
            ("lifecycle_intent", LIFECYCLE_INTENT_PREFIX),
            ("session", SESSION_PREFIX),
            ("grant", GRANT_PREFIX),
            ("organization", ORGANIZATION_PREFIX),
            ("project", PROJECT_PREFIX),
            ("invite", INVITE_PREFIX),
            ("datastore", DATASTORE_PREFIX),
            ("database", DATABASE_PREFIX),
            ("binding", BINDING_PREFIX),
            ("app_oauth_client", APP_OAUTH_CLIENT_PREFIX),
            ("plan", PLAN_PREFIX),
            ("invoice", INVOICE_PREFIX),
            ("invoice_payment", INVOICE_PAYMENT_PREFIX),
            ("credit", CREDIT_PREFIX),
            ("refund", REFUND_PREFIX),
            ("plan_change_event", PLAN_CHANGE_EVENT_PREFIX),
            ("spend_history", SPEND_HISTORY_PREFIX),
            ("organization_billing_history", ORGANIZATION_BILLING_HISTORY_PREFIX),
            ("dispute", DISPUTE_PREFIX),
            ("payout_failure", PAYOUT_FAILURE_PREFIX),
            ("checkout_failure", CHECKOUT_FAILURE_PREFIX),
            ("reconcile_finding", RECONCILE_FINDING_PREFIX),
        ];
        for (index, (name_a, prefix_a)) in prefixes.iter().enumerate() {
            for (name_b, prefix_b) in &prefixes[index + 1..] {
                assert_ne!(
                    prefix_a, prefix_b,
                    "entity prefixes must be pairwise-disjoint: {name_a} and {name_b} both \
                     use '{prefix_a}', so an id of one would parse as the other"
                );
            }
        }
    }

    /// The three decoupling prefixes are the exact spellings the design names, and
    /// the binding edge is NOT the auth grant.
    ///
    /// Pinned separately from the disjointness sweep because that sweep only proves
    /// they differ from each other. These assert WHICH value each one is, so renaming
    /// one - which would rename every physical schema, role and encryption salt
    /// derived from it - fails here rather than in a migration.
    #[test]
    fn decoupling_prefixes_are_pinned_and_distinct_from_the_auth_grant() {
        assert_eq!(DATASTORE_PREFIX, "dst");
        assert_eq!(DATABASE_PREFIX, "dbs");
        assert_eq!(BINDING_PREFIX, "bnd");
        assert_ne!(
            BINDING_PREFIX, GRANT_PREFIX,
            "the data-access binding must not share the auth grant's prefix"
        );
    }

    /// so a `transition_id` from one source can NEVER collide with another's. This test is
    /// the typed-id-registry assertion the design names; it fails the day two sources
    /// share a prefix (which would let one source's id silently dedup against another's).
    #[test]
    fn notification_source_prefixes_are_pairwise_disjoint() {
        // `DISPUTE_PREFIX` MUST still equal the `"dsp"` literal this test reserved
        // before the const existed, so the dedup key stays disjoint and stable.
        assert_eq!(DISPUTE_PREFIX, "dsp", "DISPUTE_PREFIX must remain 'dsp' (notify dedup key)");
        let sources = [
            ("spend_state_history", SPEND_HISTORY_PREFIX),
            ("organization_billing_status_history", ORGANIZATION_BILLING_HISTORY_PREFIX),
            ("invoices", INVOICE_PREFIX),
            ("refunds", REFUND_PREFIX),
            ("disputes", DISPUTE_PREFIX),
            ("payout_failures", PAYOUT_FAILURE_PREFIX),
            ("connect_checkout_failures", CHECKOUT_FAILURE_PREFIX),
        ];
        for (i, (name_a, pa)) in sources.iter().enumerate() {
            assert_eq!(pa.len(), 3, "{name_a} prefix must be 3 chars (R16-API2)");
            for (name_b, pb) in &sources[i + 1..] {
                assert_ne!(
                    pa, pb,
                    "notification source prefixes must be pairwise-disjoint: \
                     {name_a} and {name_b} both use '{pa}' — the send-ledger dedup key \
                     would let one source collide with the other"
                );
            }
        }
    }

    #[test]
    fn workflow_prefixes_roundtrip() {
        let cases = [
            (new_workflow_run_id as fn() -> String, WORKFLOW_RUN_PREFIX),
            (new_workflow_signal_id as fn() -> String, WORKFLOW_SIGNAL_PREFIX),
            (new_workflow_cron_id as fn() -> String, WORKFLOW_CRON_PREFIX),
            (new_workflow_dispatch_id as fn() -> String, WORKFLOW_DISPATCH_PREFIX),
            (new_workflow_signal_key_id as fn() -> String, WORKFLOW_SIGNAL_KEY_PREFIX),
            (new_workflow_subscription_id as fn() -> String, WORKFLOW_SUBSCRIPTION_PREFIX),
            (new_workflow_broadcast_id as fn() -> String, WORKFLOW_BROADCAST_PREFIX),
        ];
        for (mk, want) in cases {
            let id = mk();
            assert!(id.starts_with(&format!("{want}_")), "got {id}");
            assert_eq!(
                id.len(),
                want.len() + 1 + BODY_LEN,
                "{want}_ + base36 length mismatch: {id}"
            );
            let (prefix, _) = parse(&id).unwrap_or_else(|e| panic!("{want} id must roundtrip: {e}"));
            assert_eq!(prefix, want);
        }
    }

    #[test]
    fn workflow_prefixes_are_pairwise_disjoint() {
        let prefixes = [
            ("workflow_runs", WORKFLOW_RUN_PREFIX),
            ("workflow_signals", WORKFLOW_SIGNAL_PREFIX),
            ("workflow_crons", WORKFLOW_CRON_PREFIX),
            ("workflow_schedules", WORKFLOW_SCHEDULE_PREFIX),
            ("workflow_dispatches", WORKFLOW_DISPATCH_PREFIX),
            ("workflow_signal_keys", WORKFLOW_SIGNAL_KEY_PREFIX),
            ("workflow_subscriptions", WORKFLOW_SUBSCRIPTION_PREFIX),
            ("workflow_broadcasts", WORKFLOW_BROADCAST_PREFIX),
            ("workflow_propagations", WORKFLOW_PROPAGATION_PREFIX),
            ("workflow_signal_tokens", WORKFLOW_SIGNAL_TOKEN_PREFIX),
        ];
        for (i, (name_a, pa)) in prefixes.iter().enumerate() {
            for (name_b, pb) in &prefixes[i + 1..] {
                assert_ne!(
                    pa, pb,
                    "workflow prefixes must be pairwise-disjoint: {name_a} and {name_b} both use {pa}"
                );
            }
        }
        assert_ne!(
            WORKFLOW_DISPATCH_PREFIX, DISPUTE_PREFIX,
            "workflow dispatch/batch id must not collide with billing dispute dsp_"
        );
        assert_ne!(
            WORKFLOW_SUBSCRIPTION_PREFIX, "sub",
            "workflow subscription id must not collide with Stripe subscription sub_"
        );
    }

    #[test]
    fn dispute_prefix_is_three_chars_and_disjoint() {
        assert_eq!(DISPUTE_PREFIX.len(), 3, "dispute prefix must be 3 chars (R16-API2)");
        let d = new_dispute_id();
        assert!(d.starts_with("dsp_"), "got {d}");
        assert_eq!(d.len(), 29, "dsp_ + 25 base36 = 29 chars");
        let (prefix, _) = parse(&d).expect("new_dispute_id must roundtrip");
        assert_eq!(prefix, "dsp");
        // Disjoint from every sibling money/notification-source prefix.
        assert_ne!(prefix, INVOICE_PREFIX);
        assert_ne!(prefix, INVOICE_PAYMENT_PREFIX);
        assert_ne!(prefix, CREDIT_PREFIX);
        assert_ne!(prefix, REFUND_PREFIX);
        assert_ne!(prefix, SPEND_HISTORY_PREFIX);
        assert_ne!(prefix, ORGANIZATION_BILLING_HISTORY_PREFIX);
    }

    /// `wkr` and `wjs` must collide with nothing, and the sweep is over the
    /// WHOLE registry rather than a family, because neither a worker instance
    /// nor the signer that admitted it is a member of one: both are addressed by the control
    /// plane and by nothing else. Each is also swept against the other.
    ///
    /// WHAT THIS DOES NOT CATCH: a prefix added to the module after this list
    /// was written is not in the list, so this test cannot see it. Adding a
    /// prefix means adding it here; the failure of that is silent.
    #[test]
    fn worker_instance_and_join_signer_prefixes_are_three_chars_and_disjoint() {
        for (prefix, minted) in [
            (WORKER_INSTANCE_PREFIX, new_worker_instance_id()),
            (JOIN_SIGNER_PREFIX, new_join_signer_id()),
        ] {
            assert_eq!(prefix.len(), 3, "{prefix} must be 3 chars (R16-API2)");
            assert!(minted.starts_with(&format!("{prefix}_")), "got {minted}");
            assert_eq!(minted.len(), 29, "{prefix}_ + 25 base36 = 29 chars");
            let (parsed, _) = parse(&minted).expect("a minted id must roundtrip");
            assert_eq!(parsed, prefix);
        }
        // The control: the two are not one prefix spelled twice.
        assert_ne!(WORKER_INSTANCE_PREFIX, JOIN_SIGNER_PREFIX);

        let registry = [
            ("deployments", DEPLOYMENT_PREFIX),
            ("workflow_jobs", WORKFLOW_JOB_PREFIX),
            ("workflow_requests", WORKFLOW_REQUEST_PREFIX),
            ("workflow_capabilities", WORKFLOW_CAPABILITY_PREFIX),
            ("workflow_payloads", WORKFLOW_PAYLOAD_PREFIX),
            ("users", USER_PREFIX),
            ("apps", APP_PREFIX),
            ("sessions", SESSION_PREFIX),
            ("grants", GRANT_PREFIX),
            ("organizations", ORGANIZATION_PREFIX),
            ("projects", PROJECT_PREFIX),
            ("organization_invites", INVITE_PREFIX),
            ("app_oauth_clients", APP_OAUTH_CLIENT_PREFIX),
            ("plans", PLAN_PREFIX),
            ("invoices", INVOICE_PREFIX),
            ("invoice_payments", INVOICE_PAYMENT_PREFIX),
            ("credit_ledger", CREDIT_PREFIX),
            ("refunds", REFUND_PREFIX),
            ("plan_change_events", PLAN_CHANGE_EVENT_PREFIX),
            ("spend_state_history", SPEND_HISTORY_PREFIX),
            ("organization_billing_status_history", ORGANIZATION_BILLING_HISTORY_PREFIX),
            ("billing_disputes", DISPUTE_PREFIX),
            ("payout_failures", PAYOUT_FAILURE_PREFIX),
            ("connect_checkout_failures", CHECKOUT_FAILURE_PREFIX),
            ("billing_reconciliation_findings", RECONCILE_FINDING_PREFIX),
            ("workflow_runs", WORKFLOW_RUN_PREFIX),
            ("workflow_signals", WORKFLOW_SIGNAL_PREFIX),
            ("workflow_crons", WORKFLOW_CRON_PREFIX),
            ("workflow_schedules", WORKFLOW_SCHEDULE_PREFIX),
            ("workflow_dispatches", WORKFLOW_DISPATCH_PREFIX),
            ("workflow_signal_keys", WORKFLOW_SIGNAL_KEY_PREFIX),
            ("workflow_subscriptions", WORKFLOW_SUBSCRIPTION_PREFIX),
            ("workflow_broadcasts", WORKFLOW_BROADCAST_PREFIX),
            ("workflow_propagations", WORKFLOW_PROPAGATION_PREFIX),
            ("workflow_signal_tokens", WORKFLOW_SIGNAL_TOKEN_PREFIX),
            ("provider_dead_letter", PROVIDER_DEAD_LETTER_PREFIX),
        ];
        for (owner, other) in registry {
            for prefix in [WORKER_INSTANCE_PREFIX, JOIN_SIGNER_PREFIX] {
                assert_ne!(
                    prefix, other,
                    "{prefix} must be disjoint from every registered prefix; {owner} already uses it"
                );
            }
        }
    }

    /// Deploy commands and lifecycle intents are named by Control and by the
    /// clients that retry deploys. The sweep covers the whole registry because a
    /// command id arrives from outside and must not parse as any other entity.
    #[test]
    fn deploy_publication_prefixes_are_three_chars_and_disjoint() {
        let intent = new_lifecycle_intent_id();
        assert!(intent.starts_with("lci_"), "got {intent}");
        assert_eq!(parse(&intent).expect("intent id must roundtrip").0, "lci");
        let registry = [
            ("users", USER_PREFIX),
            ("apps", APP_PREFIX),
            ("app_deploys", DEPLOYMENT_PREFIX),
            ("sessions", SESSION_PREFIX),
            ("grants", GRANT_PREFIX),
            ("organizations", ORGANIZATION_PREFIX),
            ("projects", PROJECT_PREFIX),
            ("organization_invites", INVITE_PREFIX),
            ("app_oauth_clients", APP_OAUTH_CLIENT_PREFIX),
            ("plans", PLAN_PREFIX),
            ("invoices", INVOICE_PREFIX),
            ("invoice_payments", INVOICE_PAYMENT_PREFIX),
            ("credit_ledger", CREDIT_PREFIX),
            ("refunds", REFUND_PREFIX),
            ("plan_change_events", PLAN_CHANGE_EVENT_PREFIX),
            ("spend_state_history", SPEND_HISTORY_PREFIX),
            ("organization_billing_status_history", ORGANIZATION_BILLING_HISTORY_PREFIX),
            ("billing_disputes", DISPUTE_PREFIX),
            ("payout_failures", PAYOUT_FAILURE_PREFIX),
            ("connect_checkout_failures", CHECKOUT_FAILURE_PREFIX),
            ("billing_reconciliation_findings", RECONCILE_FINDING_PREFIX),
            ("workflow_runs", WORKFLOW_RUN_PREFIX),
            ("workflow_jobs", WORKFLOW_JOB_PREFIX),
            ("workflow_signals", WORKFLOW_SIGNAL_PREFIX),
            ("workflow_crons", WORKFLOW_CRON_PREFIX),
            ("workflow_schedules", WORKFLOW_SCHEDULE_PREFIX),
            ("workflow_dispatches", WORKFLOW_DISPATCH_PREFIX),
            ("workflow_requests", WORKFLOW_REQUEST_PREFIX),
            ("workflow_capabilities", WORKFLOW_CAPABILITY_PREFIX),
            ("workflow_payloads", WORKFLOW_PAYLOAD_PREFIX),
            ("workflow_signal_keys", WORKFLOW_SIGNAL_KEY_PREFIX),
            ("workflow_subscriptions", WORKFLOW_SUBSCRIPTION_PREFIX),
            ("workflow_broadcasts", WORKFLOW_BROADCAST_PREFIX),
            ("workflow_propagations", WORKFLOW_PROPAGATION_PREFIX),
            ("workflow_signal_tokens", WORKFLOW_SIGNAL_TOKEN_PREFIX),
            ("provider_dead_letter", PROVIDER_DEAD_LETTER_PREFIX),
            ("worker_instances", WORKER_INSTANCE_PREFIX),
        ];
        for prefix in [DEPLOY_COMMAND_PREFIX, LIFECYCLE_INTENT_PREFIX] {
            assert_eq!(prefix.len(), 3, "{prefix} prefix must be 3 chars");
            for (owner, other) in registry {
                assert_ne!(
                    prefix, other,
                    "{prefix} must be disjoint from every registered prefix; {owner} uses it"
                );
            }
        }
        assert_ne!(DEPLOY_COMMAND_PREFIX, LIFECYCLE_INTENT_PREFIX);
    }

    #[test]
    fn parse_with_prefix_matches() {
        let id = generate("sbx");
        let uuid = parse_with_prefix(&id, "sbx").expect("matching prefix should parse");
        let (_, expected) = parse(&id).unwrap();
        assert_eq!(uuid, expected);
    }

    #[test]
    fn parse_with_prefix_rejects_wrong_prefix() {
        let id = crate::UserId::mint();
        let err =
            parse_with_prefix(id.as_str(), "sbx").expect_err("wrong prefix must error");
        match err {
            ParseError::WrongPrefix { expected, got } => {
                assert_eq!(expected, "sbx");
                assert_eq!(got, "usr");
            }
            ParseError::Malformed(_) => panic!("should not classify as malformed"),
        }
    }

    #[test]
    fn parse_with_prefix_rejects_malformed() {
        // No underscore → underlying parse fails first.
        let err = parse_with_prefix("garbage", "sbx").expect_err("malformed must error");
        assert!(matches!(err, ParseError::Malformed(_)));

        // Invalid base36 after a real prefix.
        let err = parse_with_prefix("sbx_!!!notbase36!!!", "sbx")
            .expect_err("invalid base36 must error");
        assert!(matches!(err, ParseError::Malformed(_)));
    }

    #[test]
    fn parse_with_prefix_rejects_empty() {
        let err =
            parse_with_prefix("", "sbx").expect_err("empty input must error");
        assert!(matches!(err, ParseError::Malformed(_)));
    }
}
