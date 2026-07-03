import { membership, t } from "@zeroship/migrate";
import { domain, extension, role, schema, sequence } from "@zeroship/migrate/pg";

export const name = "schema_roles_extensions";

export function up() {
  schema({ name: "zeroship", ifNotExists: true });
  extension({ name: "citext", ifNotExists: true, schema: "public" });
  role({ name: "zeroship_auth", login: true, password: "zeroship_auth", bypassRls: true, setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role({ name: "zeroship_control", login: true, password: "zeroship_control", bypassRls: true, setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role({ name: "zeroship_gateway", login: true, password: "zeroship_gateway", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role({ name: "zeroship_worker", login: true, password: "zeroship_worker", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role({ name: "zeroship_app", login: true, password: "zeroship_app", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role({ name: "sandbox_admin", login: false, ifNotExists: true });
  role({ name: "sandbox_app", login: true, password: "sandbox_app", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role({ name: "sandbox_audit", login: true, password: "sandbox_audit", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  role({ name: "sandbox_gdpr", login: true, password: "sandbox_gdpr", setSearchPath: ["zeroship", "public"], ifNotExists: true });
  domain("account_state").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["active", "past_due", "suspended"]) });
  domain("billing_notification_kind").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["payment_failed", "past_due", "suspended", "recovered", "invoice_finalized", "refunded", "disputed", "payout_failed", "checkout_failed", "spend_warn", "spend_degrade", "spend_block"]) });
  domain("billing_period").create({ schema: "zeroship", as: t.date(), check: (c) => c.pg.extract("day", c("VALUE")).eq(1) });
  domain("credit_entry_kind").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["grant", "promo", "goodwill", "refund_to_credit", "consumed", "void_reversal", "refund_clawback"]) });
  domain("dispute_status").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["open", "won", "lost"]) });
  domain("invoice_payment_kind").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["charge", "dispute_debit", "dispute_reversal"]) });
  domain("invoice_status").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["draft", "finalized", "void"]) });
  domain("metric_kind").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["platform", "primitive", "custom"]) });
  domain("notification_status").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["pending", "sent"]) });
  domain("reconciliation_finding_kind").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["missed_invoice_payment", "invoice_status_drift", "refund_status_drift", "dispute_status_drift", "missing_dispute"]) });
  domain("reconciliation_finding_severity").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["low", "medium", "high"]) });
  domain("refund_destination").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["cash", "credit"]) });
  domain("refund_status").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["pending", "issued", "failed", "canceled"]) });
  domain("spend_state").create({ schema: "zeroship", as: t.text(), check: (c) => membership(c("VALUE"), ["allow", "warn", "degrade", "block"]) });
  sequence("audit_events_id_seq").create({ schema: "zeroship", as: t.bigInt(), start: 1, increment: 1, minValue: null, maxValue: null, cache: 1 });
}

export function down() {

}
