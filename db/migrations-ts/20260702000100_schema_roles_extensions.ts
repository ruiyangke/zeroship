import {
  t,
  domain,
  extension,
  raw,
  role,
  schema,
  sequence,
} from "@zeroship/migrate";

export default {
  name: "schema_roles_extensions",
  schema() {
    schema("zeroship").create({ ifNotExists: true });
    extension("citext").create({ ifNotExists: true, schema: "public" });
    role("zeroship_auth").create({ login: true, password: "zeroship_auth", bypassRls: true, setSearchPath: ["zeroship", "public"], ifNotExists: true });
    role("zeroship_control").create({ login: true, password: "zeroship_control", bypassRls: true, setSearchPath: ["zeroship", "public"], ifNotExists: true });
    role("zeroship_gateway").create({ login: true, password: "zeroship_gateway", setSearchPath: ["zeroship", "public"], ifNotExists: true });
    role("zeroship_worker").create({ login: true, password: "zeroship_worker", setSearchPath: ["zeroship", "public"], ifNotExists: true });
    role("zeroship_app").create({ login: true, password: "zeroship_app", setSearchPath: ["zeroship", "public"], ifNotExists: true });
    // Logical decoding belongs to the relay process. Workers execute creator code
    // and authenticate relay subscriptions with their enrolled instance identity.
    role("zeroship_cdc").create({ login: true, password: "zeroship_cdc", ifNotExists: true });
    raw({
      sql: "ALTER ROLE zeroship_cdc WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOINHERIT REPLICATION NOBYPASSRLS",
      reason: "the relay owns logical decoding without ordinary tenant table privileges",
    });
    raw({
      sql: "ALTER ROLE zeroship_worker WITH NOREPLICATION NOBYPASSRLS",
      reason: "workers receive committed invalidations from the relay",
    });
    domain("account_state").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["active", "past_due", "suspended"]) });
    domain("billing_notification_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["payment_failed", "past_due", "suspended", "recovered", "invoice_finalized", "refunded", "disputed", "payout_failed", "checkout_failed", "spend_warn", "spend_degrade", "spend_block"]) });
    domain("billing_period").create({ schema: "zeroship", as: t.date(), check: (v) => v.extract("day").eq(1) });
    domain("credit_entry_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["grant", "promo", "goodwill", "refund_to_credit", "consumed", "void_reversal", "refund_clawback"]) });
    domain("dispute_status").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["open", "won", "lost"]) });
    domain("invoice_payment_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["charge", "dispute_debit", "dispute_reversal"]) });
    domain("invoice_status").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["draft", "finalized", "void"]) });
    domain("metric_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["platform", "primitive", "custom"]) });
    domain("notification_status").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["pending", "sent"]) });
    domain("reconciliation_finding_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["missed_invoice_payment", "invoice_status_drift", "refund_status_drift", "dispute_status_drift", "missing_dispute", "provider_reject", "provider_meter_drift", "late_period_adjustment", "correction_unpriceable", "forwarder_down_exceeds_retention", "subject_attribution_mismatch", "terminal_period_trueup"]) });
    domain("reconciliation_finding_severity").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["low", "medium", "high"]) });
    domain("refund_destination").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["cash", "credit"]) });
    domain("refund_status").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["pending", "issued", "failed", "canceled"]) });
    domain("spend_state").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["allow", "warn", "degrade", "block"]) });
    sequence("audit_events_id_seq").create({ schema: "zeroship", as: t.bigInt(), start: 1, increment: 1, minValue: null, maxValue: null, cache: 1 });
  },
};
