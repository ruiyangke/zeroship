import { t } from "@zeroship/migrate";
import { extension, raw, role, schema, sequence } from "@zeroship/migrate/pg";

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
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.account_state AS text\n\tCONSTRAINT account_state_check CHECK ((VALUE = ANY (ARRAY['active'::text, 'past_due'::text, 'suspended'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.billing_notification_kind AS text\n\tCONSTRAINT billing_notification_kind_check CHECK ((VALUE = ANY (ARRAY['payment_failed'::text, 'past_due'::text, 'suspended'::text, 'recovered'::text, 'invoice_finalized'::text, 'refunded'::text, 'disputed'::text, 'payout_failed'::text, 'checkout_failed'::text, 'spend_warn'::text, 'spend_degrade'::text, 'spend_block'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.billing_period AS date\n\tCONSTRAINT billing_period_check CHECK ((EXTRACT(day FROM VALUE) = (1)::numeric))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.credit_entry_kind AS text\n\tCONSTRAINT credit_entry_kind_check CHECK ((VALUE = ANY (ARRAY['grant'::text, 'promo'::text, 'goodwill'::text, 'refund_to_credit'::text, 'consumed'::text, 'void_reversal'::text, 'refund_clawback'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.dispute_status AS text\n\tCONSTRAINT dispute_status_check CHECK ((VALUE = ANY (ARRAY['open'::text, 'won'::text, 'lost'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.invoice_payment_kind AS text\n\tCONSTRAINT invoice_payment_kind_check CHECK ((VALUE = ANY (ARRAY['charge'::text, 'dispute_debit'::text, 'dispute_reversal'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.invoice_status AS text\n\tCONSTRAINT invoice_status_check CHECK ((VALUE = ANY (ARRAY['draft'::text, 'finalized'::text, 'void'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.metric_kind AS text\n\tCONSTRAINT metric_kind_check CHECK ((VALUE = ANY (ARRAY['platform'::text, 'primitive'::text, 'custom'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.notification_status AS text\n\tCONSTRAINT notification_status_check CHECK ((VALUE = ANY (ARRAY['pending'::text, 'sent'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.reconciliation_finding_kind AS text\n\tCONSTRAINT reconciliation_finding_kind_check CHECK ((VALUE = ANY (ARRAY['missed_invoice_payment'::text, 'invoice_status_drift'::text, 'refund_status_drift'::text, 'dispute_status_drift'::text, 'missing_dispute'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.reconciliation_finding_severity AS text\n\tCONSTRAINT reconciliation_finding_severity_check CHECK ((VALUE = ANY (ARRAY['low'::text, 'medium'::text, 'high'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.refund_destination AS text\n\tCONSTRAINT refund_destination_check CHECK ((VALUE = ANY (ARRAY['cash'::text, 'credit'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.refund_status AS text\n\tCONSTRAINT refund_status_check CHECK ((VALUE = ANY (ARRAY['pending'::text, 'issued'::text, 'failed'::text, 'canceled'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  // TODO(dsl-v2): add structural domain CHECK support for membership arrays and SQL date/extract predicates
  raw({ sql: "CREATE DOMAIN zeroship.spend_state AS text\n\tCONSTRAINT spend_state_check CHECK ((VALUE = ANY (ARRAY['allow'::text, 'warn'::text, 'degrade'::text, 'block'::text])))", reason: "domain checks require SQL constructs that the current expression DSL cannot render, such as ANY arrays or EXTRACT" });
  sequence("audit_events_id_seq").create({ schema: "zeroship", as: t.bigInt(), start: 1, increment: 1, minValue: null, maxValue: null, cache: 1 });
}

export function down() {

}
