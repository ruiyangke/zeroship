import {
  t,
  domain,
  extension,
  role,
  schema,
  sequence,
} from "@zeroship/migrate";

export const name = "schema_roles_extensions";

export function up() {
  schema("zeroship").create({ ifNotExists: true });
  extension("citext").create({ ifNotExists: true, schema: "public" });
  role("zeroship_auth").create({ login: true, password: "zeroship_auth", bypassRls: true, setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role("zeroship_control").create({ login: true, password: "zeroship_control", bypassRls: true, setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role("zeroship_gateway").create({ login: true, password: "zeroship_gateway", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role("zeroship_workflow_owner").create({ login: false, ifNotExists: true });
  role("zeroship_worker").create({ login: true, password: "zeroship_worker", bypassRls: true, inRole: ["zeroship_workflow_owner"], setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role("zeroship_app").create({ login: true, password: "zeroship_app", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role("sandbox_admin").create({ login: false, ifNotExists: true });
  role("sandbox_app").create({ login: true, password: "sandbox_app", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role("sandbox_audit").create({ login: true, password: "sandbox_audit", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role("sandbox_gdpr").create({ login: true, password: "sandbox_gdpr", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  domain("account_state").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["active", "past_due", "suspended"]) });
  domain("billing_notification_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["payment_failed", "past_due", "suspended", "recovered", "invoice_finalized", "refunded", "disputed", "payout_failed", "checkout_failed", "spend_warn", "spend_degrade", "spend_block"]) });
  domain("billing_period").create({ schema: "zeroship", as: t.date(), check: (v) => v.extract("day").eq(1) });
  domain("credit_entry_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["grant", "promo", "goodwill", "refund_to_credit", "consumed", "void_reversal", "refund_clawback"]) });
  domain("dispute_status").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["open", "won", "lost"]) });
  domain("invoice_payment_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["charge", "dispute_debit", "dispute_reversal"]) });
  domain("invoice_status").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["draft", "finalized", "void"]) });
  domain("metric_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["platform", "primitive", "custom"]) });
  domain("notification_status").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["pending", "sent"]) });
  domain("reconciliation_finding_kind").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["missed_invoice_payment", "invoice_status_drift", "refund_status_drift", "dispute_status_drift", "missing_dispute", "provider_reject"]) });
  domain("reconciliation_finding_severity").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["low", "medium", "high"]) });
  domain("refund_destination").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["cash", "credit"]) });
  domain("refund_status").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["pending", "issued", "failed", "canceled"]) });
  domain("spend_state").create({ schema: "zeroship", as: t.text(), check: (v) => v.in(["allow", "warn", "degrade", "block"]) });
  sequence("audit_events_id_seq").create({ schema: "zeroship", as: t.bigInt(), start: 1, increment: 1, minValue: null, maxValue: null, cache: 1 });
}

export function down() {

}
