import { t } from "@zeroship/migrate";
import { domain, extension, raw, role, schema, sequence } from "@zeroship/migrate/pg";

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
  domain("account_state").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.account_state ADD CONSTRAINT account_state_check CHECK ((VALUE = ANY (ARRAY['active'::text, 'past_due'::text, 'suspended'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("billing_notification_kind").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.billing_notification_kind ADD CONSTRAINT billing_notification_kind_check CHECK ((VALUE = ANY (ARRAY['payment_failed'::text, 'past_due'::text, 'suspended'::text, 'recovered'::text, 'invoice_finalized'::text, 'refunded'::text, 'disputed'::text, 'payout_failed'::text, 'checkout_failed'::text, 'spend_warn'::text, 'spend_degrade'::text, 'spend_block'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  // TODO(dsl-v2): add a structural date column/domain type plus EXTRACT predicate rendering
  raw({ sql: "CREATE DOMAIN zeroship.billing_period AS date\n\tCONSTRAINT billing_period_check CHECK ((EXTRACT(day FROM VALUE) = (1)::numeric))", reason: "billing_period is a date-backed domain and its EXTRACT(day FROM VALUE) CHECK is outside the current structural type/expression surface" });
  domain("credit_entry_kind").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.credit_entry_kind ADD CONSTRAINT credit_entry_kind_check CHECK ((VALUE = ANY (ARRAY['grant'::text, 'promo'::text, 'goodwill'::text, 'refund_to_credit'::text, 'consumed'::text, 'void_reversal'::text, 'refund_clawback'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("dispute_status").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.dispute_status ADD CONSTRAINT dispute_status_check CHECK ((VALUE = ANY (ARRAY['open'::text, 'won'::text, 'lost'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("invoice_payment_kind").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.invoice_payment_kind ADD CONSTRAINT invoice_payment_kind_check CHECK ((VALUE = ANY (ARRAY['charge'::text, 'dispute_debit'::text, 'dispute_reversal'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("invoice_status").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.invoice_status ADD CONSTRAINT invoice_status_check CHECK ((VALUE = ANY (ARRAY['draft'::text, 'finalized'::text, 'void'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("metric_kind").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.metric_kind ADD CONSTRAINT metric_kind_check CHECK ((VALUE = ANY (ARRAY['platform'::text, 'primitive'::text, 'custom'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("notification_status").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.notification_status ADD CONSTRAINT notification_status_check CHECK ((VALUE = ANY (ARRAY['pending'::text, 'sent'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("reconciliation_finding_kind").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.reconciliation_finding_kind ADD CONSTRAINT reconciliation_finding_kind_check CHECK ((VALUE = ANY (ARRAY['missed_invoice_payment'::text, 'invoice_status_drift'::text, 'refund_status_drift'::text, 'dispute_status_drift'::text, 'missing_dispute'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("reconciliation_finding_severity").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.reconciliation_finding_severity ADD CONSTRAINT reconciliation_finding_severity_check CHECK ((VALUE = ANY (ARRAY['low'::text, 'medium'::text, 'high'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("refund_destination").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.refund_destination ADD CONSTRAINT refund_destination_check CHECK ((VALUE = ANY (ARRAY['cash'::text, 'credit'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("refund_status").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.refund_status ADD CONSTRAINT refund_status_check CHECK ((VALUE = ANY (ARRAY['pending'::text, 'issued'::text, 'failed'::text, 'canceled'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  domain("spend_state").create({ schema: "zeroship", as: t.text() });
  // TODO(dsl-v2): domain CHECK predicates need ANY(ARRAY[...]) expression rendering
  raw({ sql: "ALTER DOMAIN zeroship.spend_state ADD CONSTRAINT spend_state_check CHECK ((VALUE = ANY (ARRAY['allow'::text, 'warn'::text, 'degrade'::text, 'block'::text])))", reason: "domain membership checks remain raw until the Expr renderer can emit ANY(ARRAY[...]) predicates" });
  sequence("audit_events_id_seq").create({ schema: "zeroship", as: t.bigInt(), start: 1, increment: 1, minValue: null, maxValue: null, cache: 1 });
}

export function down() {

}
