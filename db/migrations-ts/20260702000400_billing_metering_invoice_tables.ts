import { table, t } from "@zeroship/migrate";
import { raw } from "@zeroship/migrate/pg";

export const name = "billing_metering_invoice_tables";

export function up() {
  table("app_spend_limit", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      spend_limit_cents: t.bigInt(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id"],
  });
  // TODO(dsl-v2): CHECK constraint app_spend_limit.app_spend_limit_spend_limit_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_limit ADD CONSTRAINT app_spend_limit_spend_limit_cents_check CHECK (((spend_limit_cents IS NULL) OR (spend_limit_cents >= 0)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("app_spend_state", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      state: t.text().notNull().default("allow"),
      spend_cents: t.bigInt().notNull().default(0),
      eval_limit_cents: t.bigInt().notNull().default(0),
      period: t.text().notNull(),
      evaluated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.spend_state
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_state ALTER COLUMN state TYPE zeroship.spend_state USING state::zeroship.spend_state", reason: "column app_spend_state.state uses PostgreSQL type zeroship.spend_state, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for zeroship.billing_period
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_state ALTER COLUMN period TYPE zeroship.billing_period USING period::zeroship.billing_period", reason: "column app_spend_state.period uses PostgreSQL type zeroship.billing_period, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint app_spend_state.app_spend_state_eval_limit_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_state ADD CONSTRAINT app_spend_state_eval_limit_cents_check CHECK ((eval_limit_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint app_spend_state.app_spend_state_spend_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.app_spend_state ADD CONSTRAINT app_spend_state_spend_cents_check CHECK ((spend_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("billing_customer_refs", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      provider: t.text().notNull(),
      external_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["creator_id", "provider"],
  });
  table("billing_disputes", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      invoice_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.text().notNull(),
      status: t.text().notNull().default("open"),
      reason: t.text(),
      evidence_due_at: t.timestamp(),
      provider_dispute_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      resolved_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for character(3)
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_disputes ALTER COLUMN currency TYPE character(3) USING currency::character(3)", reason: "column billing_disputes.currency uses PostgreSQL type character(3), which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_disputes ALTER COLUMN currency SET DEFAULT 'usd'::bpchar", reason: "column billing_disputes.currency requires exact default 'usd'::bpchar, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): add structural column type support for zeroship.dispute_status
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_disputes ALTER COLUMN status TYPE zeroship.dispute_status USING status::zeroship.dispute_status", reason: "column billing_disputes.status uses PostgreSQL type zeroship.dispute_status, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint billing_disputes.billing_disputes_amount_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_disputes ADD CONSTRAINT billing_disputes_amount_cents_check CHECK ((amount_cents > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint billing_disputes.billing_disputes_currency_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_disputes ADD CONSTRAINT billing_disputes_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("billing_line_provider_refs", { schema: "zeroship" }).create({
    columns: {
      invoice_id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      provider: t.text().notNull(),
      ref_kind: t.text().notNull(),
      external_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      segment_no: t.integer().notNull().default(0),
    },
    primaryKey: ["invoice_id", "app_id", "segment_no", "provider", "ref_kind"],
  });
  // TODO(dsl-v2): add structural column type support for smallint
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_line_provider_refs ALTER COLUMN segment_no TYPE smallint USING segment_no::smallint", reason: "column billing_line_provider_refs.segment_no uses PostgreSQL type smallint, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint billing_line_provider_refs.billing_line_provider_refs_segment_no_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_line_provider_refs ADD CONSTRAINT billing_line_provider_refs_segment_no_check CHECK ((segment_no >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("billing_metrics", { schema: "zeroship" }).create({
    columns: {
      metric: t.text().notNull(),
      kind: t.text().notNull(),
      unit: t.text().notNull(),
      archived: t.boolean().notNull().default(false),
      last_seen_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      owner_app: t.uuid(),
    },
    primaryKey: ["metric"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.metric_kind
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_metrics ALTER COLUMN kind TYPE zeroship.metric_kind USING kind::zeroship.metric_kind", reason: "column billing_metrics.kind uses PostgreSQL type zeroship.metric_kind, which is not in the current closed column lexicon/lowerer use-site set" });
  table("billing_notifications", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      kind: t.text().notNull(),
      transition_id: t.text().notNull(),
      status: t.text().notNull().default("pending"),
      claimed_at: t.timestamp().notNull().default({ fn: "now" }),
      sent_at: t.timestamp(),
    },
    primaryKey: ["creator_id", "kind", "transition_id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.billing_notification_kind
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_notifications ALTER COLUMN kind TYPE zeroship.billing_notification_kind USING kind::zeroship.billing_notification_kind", reason: "column billing_notifications.kind uses PostgreSQL type zeroship.billing_notification_kind, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for zeroship.notification_status
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_notifications ALTER COLUMN status TYPE zeroship.notification_status USING status::zeroship.notification_status", reason: "column billing_notifications.status uses PostgreSQL type zeroship.notification_status, which is not in the current closed column lexicon/lowerer use-site set" });
  table("billing_provider_refs", { schema: "zeroship" }).create({
    columns: {
      invoice_id: t.text().notNull(),
      provider: t.text().notNull(),
      ref_kind: t.text().notNull(),
      external_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["invoice_id", "provider", "ref_kind"],
  });
  table("billing_reconciliation_findings", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      kind: t.text().notNull(),
      severity: t.text().notNull().default("medium"),
      entity_id: t.text().notNull(),
      our_value: t.json(),
      stripe_value: t.json(),
      dedup_key: t.text().notNull(),
      detected_at: t.timestamp().notNull().default({ fn: "now" }),
      resolved_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.reconciliation_finding_kind
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_reconciliation_findings ALTER COLUMN kind TYPE zeroship.reconciliation_finding_kind USING kind::zeroship.reconciliation_finding_kind", reason: "column billing_reconciliation_findings.kind uses PostgreSQL type zeroship.reconciliation_finding_kind, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for zeroship.reconciliation_finding_severity
  raw({ sql: "ALTER TABLE ONLY zeroship.billing_reconciliation_findings ALTER COLUMN severity TYPE zeroship.reconciliation_finding_severity USING severity::zeroship.reconciliation_finding_severity", reason: "column billing_reconciliation_findings.severity uses PostgreSQL type zeroship.reconciliation_finding_severity, which is not in the current closed column lexicon/lowerer use-site set" });
  table("connect_checkout_failures", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      provider_payment_intent_id: t.text().notNull(),
      stripe_account_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.text().notNull(),
      failure_code: t.text(),
      failure_message: t.text(),
      occurred_at: t.timestamp().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for character(3)
  raw({ sql: "ALTER TABLE ONLY zeroship.connect_checkout_failures ALTER COLUMN currency TYPE character(3) USING currency::character(3)", reason: "column connect_checkout_failures.currency uses PostgreSQL type character(3), which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.connect_checkout_failures ALTER COLUMN currency SET DEFAULT 'usd'::bpchar", reason: "column connect_checkout_failures.currency requires exact default 'usd'::bpchar, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): CHECK constraint connect_checkout_failures.connect_checkout_failures_amount_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.connect_checkout_failures ADD CONSTRAINT connect_checkout_failures_amount_cents_check CHECK ((amount_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint connect_checkout_failures.connect_checkout_failures_currency_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.connect_checkout_failures ADD CONSTRAINT connect_checkout_failures_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("creator_billing", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      default_pm_set: t.boolean().notNull().default(false),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["creator_id"],
  });
  table("creator_billing_status", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      state: t.text().notNull().default("active"),
      past_due_since: t.timestamp(),
      suspended_at: t.timestamp(),
      last_payment_failure_at: t.timestamp(),
      failed_invoice_id: t.text(),
      last_event_at: t.timestamp(),
      last_recovered_at: t.timestamp(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["creator_id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.account_state
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing_status ALTER COLUMN state TYPE zeroship.account_state USING state::zeroship.account_state", reason: "column creator_billing_status.state uses PostgreSQL type zeroship.account_state, which is not in the current closed column lexicon/lowerer use-site set" });
  table("creator_billing_status_history", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      from_state: t.text().notNull(),
      to_state: t.text().notNull(),
      reason: t.text(),
      at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.account_state
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing_status_history ALTER COLUMN from_state TYPE zeroship.account_state USING from_state::zeroship.account_state", reason: "column creator_billing_status_history.from_state uses PostgreSQL type zeroship.account_state, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for zeroship.account_state
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_billing_status_history ALTER COLUMN to_state TYPE zeroship.account_state USING to_state::zeroship.account_state", reason: "column creator_billing_status_history.to_state uses PostgreSQL type zeroship.account_state, which is not in the current closed column lexicon/lowerer use-site set" });
  table("creator_fee_policy", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      kind: t.text().notNull(),
      amount_cents: t.bigInt(),
      percent_bps: t.integer(),
      cap_cents: t.bigInt(),
      floor_cents: t.bigInt(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["creator_id"],
  });
  // TODO(dsl-v2): CHECK constraint creator_fee_policy.creator_fee_policy_cap_nonneg needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_fee_policy ADD CONSTRAINT creator_fee_policy_cap_nonneg CHECK (((cap_cents IS NULL) OR (cap_cents >= 0)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint creator_fee_policy.creator_fee_policy_floor_le_cap needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_fee_policy ADD CONSTRAINT creator_fee_policy_floor_le_cap CHECK (((floor_cents IS NULL) OR (cap_cents IS NULL) OR (floor_cents <= cap_cents)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint creator_fee_policy.creator_fee_policy_floor_nonneg needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_fee_policy ADD CONSTRAINT creator_fee_policy_floor_nonneg CHECK (((floor_cents IS NULL) OR (floor_cents >= 0)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint creator_fee_policy.creator_fee_policy_kind_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_fee_policy ADD CONSTRAINT creator_fee_policy_kind_check CHECK ((kind = ANY (ARRAY['fixed'::text, 'percent'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint creator_fee_policy.creator_fee_policy_shape needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.creator_fee_policy ADD CONSTRAINT creator_fee_policy_shape CHECK ((((kind = 'fixed'::text) AND (amount_cents IS NOT NULL) AND (amount_cents >= 0)) OR ((kind = 'percent'::text) AND (percent_bps IS NOT NULL) AND ((percent_bps >= 0) AND (percent_bps <= 10000)))))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("credit_ledger", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      kind: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.text().notNull(),
      applied_invoice_id: t.text(),
      consumed_from_grant_id: t.text(),
      expires_at: t.timestamp(),
      note: t.text(),
      idempotency_key: t.text(),
      request_fingerprint: t.text(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.credit_entry_kind
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger ALTER COLUMN kind TYPE zeroship.credit_entry_kind USING kind::zeroship.credit_entry_kind", reason: "column credit_ledger.kind uses PostgreSQL type zeroship.credit_entry_kind, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for character(3)
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger ALTER COLUMN currency TYPE character(3) USING currency::character(3)", reason: "column credit_ledger.currency uses PostgreSQL type character(3), which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger ALTER COLUMN currency SET DEFAULT 'usd'::bpchar", reason: "column credit_ledger.currency requires exact default 'usd'::bpchar, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): CHECK constraint credit_ledger.credit_ledger_amount_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger ADD CONSTRAINT credit_ledger_amount_cents_check CHECK ((amount_cents <> 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint credit_ledger.credit_ledger_currency_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger ADD CONSTRAINT credit_ledger_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint credit_ledger.credit_ledger_grant_ref needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger ADD CONSTRAINT credit_ledger_grant_ref CHECK (((((kind)::text = ANY (ARRAY['consumed'::text, 'void_reversal'::text, 'refund_clawback'::text])) AND (consumed_from_grant_id IS NOT NULL)) OR (((kind)::text <> ALL (ARRAY['consumed'::text, 'void_reversal'::text, 'refund_clawback'::text])) AND (consumed_from_grant_id IS NULL))))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint credit_ledger.credit_ledger_kind_sign needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.credit_ledger ADD CONSTRAINT credit_ledger_kind_sign CHECK (((((kind)::text = ANY (ARRAY['consumed'::text, 'refund_clawback'::text])) AND (amount_cents < 0)) OR (((kind)::text <> ALL (ARRAY['consumed'::text, 'refund_clawback'::text])) AND (amount_cents > 0))))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("invoice_lines", { schema: "zeroship" }).create({
    columns: {
      invoice_id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      included_units: t.bigInt().notNull(),
      fx_pico_cents_per_unit: t.bigInt().notNull(),
      base_fee_cents: t.bigInt().notNull().default(0),
      amount_cents: t.bigInt().notNull(),
      usage_snapshot: t.json().notNull(),
      weights_snapshot: t.json().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      segment_no: t.integer().notNull().default(0),
      plan_id: t.text().notNull(),
    },
    primaryKey: ["invoice_id", "app_id", "segment_no"],
  });
  // TODO(dsl-v2): add structural column type support for smallint
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines ALTER COLUMN segment_no TYPE smallint USING segment_no::smallint", reason: "column invoice_lines.segment_no uses PostgreSQL type smallint, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint invoice_lines.invoice_lines_amount_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines ADD CONSTRAINT invoice_lines_amount_cents_check CHECK ((amount_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoice_lines.invoice_lines_base_fee_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines ADD CONSTRAINT invoice_lines_base_fee_cents_check CHECK ((base_fee_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoice_lines.invoice_lines_fx_pico_cents_per_unit_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines ADD CONSTRAINT invoice_lines_fx_pico_cents_per_unit_check CHECK ((fx_pico_cents_per_unit >= 1000))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoice_lines.invoice_lines_included_units_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines ADD CONSTRAINT invoice_lines_included_units_check CHECK ((included_units >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoice_lines.invoice_lines_segment_no_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_lines ADD CONSTRAINT invoice_lines_segment_no_check CHECK ((segment_no >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("invoice_payments", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      invoice_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.text().notNull(),
      kind: t.text().notNull(),
      provider_ref: t.text(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for character(3)
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_payments ALTER COLUMN currency TYPE character(3) USING currency::character(3)", reason: "column invoice_payments.currency uses PostgreSQL type character(3), which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_payments ALTER COLUMN currency SET DEFAULT 'usd'::bpchar", reason: "column invoice_payments.currency requires exact default 'usd'::bpchar, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): add structural column type support for zeroship.invoice_payment_kind
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_payments ALTER COLUMN kind TYPE zeroship.invoice_payment_kind USING kind::zeroship.invoice_payment_kind", reason: "column invoice_payments.kind uses PostgreSQL type zeroship.invoice_payment_kind, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint invoice_payments.invoice_payments_amount_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_payments ADD CONSTRAINT invoice_payments_amount_cents_check CHECK ((amount_cents <> 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoice_payments.invoice_payments_currency_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoice_payments ADD CONSTRAINT invoice_payments_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("invoices", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      period: t.text().notNull(),
      status: t.text().notNull().default("draft"),
      currency: t.text().notNull(),
      subtotal_cents: t.bigInt().notNull().default(0),
      credit_cents: t.bigInt().notNull().default(0),
      tax_cents: t.bigInt().notNull().default(0),
      total_cents: t.bigInt().notNull().default(0),
      finalized_at: t.timestamp(),
      voided_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.billing_period
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ALTER COLUMN period TYPE zeroship.billing_period USING period::zeroship.billing_period", reason: "column invoices.period uses PostgreSQL type zeroship.billing_period, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for zeroship.invoice_status
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ALTER COLUMN status TYPE zeroship.invoice_status USING status::zeroship.invoice_status", reason: "column invoices.status uses PostgreSQL type zeroship.invoice_status, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for character(3)
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ALTER COLUMN currency TYPE character(3) USING currency::character(3)", reason: "column invoices.currency uses PostgreSQL type character(3), which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ALTER COLUMN currency SET DEFAULT 'usd'::bpchar", reason: "column invoices.currency requires exact default 'usd'::bpchar, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): CHECK constraint invoices.invoice_total_balances needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ADD CONSTRAINT invoice_total_balances CHECK ((total_cents = ((subtotal_cents - credit_cents) + tax_cents)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoices.invoices_credit_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ADD CONSTRAINT invoices_credit_cents_check CHECK ((credit_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoices.invoices_currency_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ADD CONSTRAINT invoices_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoices.invoices_subtotal_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ADD CONSTRAINT invoices_subtotal_cents_check CHECK ((subtotal_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoices.invoices_tax_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ADD CONSTRAINT invoices_tax_cents_check CHECK ((tax_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint invoices.invoices_total_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.invoices ADD CONSTRAINT invoices_total_cents_check CHECK ((total_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("metering_exports", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      period: t.text().notNull(),
      exported_units: t.bigInt().notNull().default(0),
      consecutive_failures: t.integer().notNull().default(0),
      last_error: t.text(),
      last_attempt_at: t.timestamp(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["creator_id", "period"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.billing_period
  raw({ sql: "ALTER TABLE ONLY zeroship.metering_exports ALTER COLUMN period TYPE zeroship.billing_period USING period::zeroship.billing_period", reason: "column metering_exports.period uses PostgreSQL type zeroship.billing_period, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint metering_exports.metering_exports_consecutive_failures_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.metering_exports ADD CONSTRAINT metering_exports_consecutive_failures_check CHECK ((consecutive_failures >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint metering_exports.metering_exports_exported_units_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.metering_exports ADD CONSTRAINT metering_exports_exported_units_check CHECK ((exported_units >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("metric_weights", { schema: "zeroship" }).create({
    columns: {
      metric: t.text().notNull(),
      units_per_op: t.bigInt().notNull(),
      per_units: t.bigInt().notNull(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["metric"],
  });
  // TODO(dsl-v2): CHECK constraint metric_weights.metric_weights_per_units_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.metric_weights ADD CONSTRAINT metric_weights_per_units_check CHECK ((per_units > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint metric_weights.metric_weights_units_per_op_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.metric_weights ADD CONSTRAINT metric_weights_units_per_op_check CHECK ((units_per_op >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("payout_failures", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      provider_payout_id: t.text().notNull(),
      stripe_account_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.text().notNull(),
      failure_code: t.text(),
      failure_message: t.text(),
      occurred_at: t.timestamp().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for character(3)
  raw({ sql: "ALTER TABLE ONLY zeroship.payout_failures ALTER COLUMN currency TYPE character(3) USING currency::character(3)", reason: "column payout_failures.currency uses PostgreSQL type character(3), which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.payout_failures ALTER COLUMN currency SET DEFAULT 'usd'::bpchar", reason: "column payout_failures.currency requires exact default 'usd'::bpchar, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): CHECK constraint payout_failures.payout_failures_amount_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.payout_failures ADD CONSTRAINT payout_failures_amount_cents_check CHECK ((amount_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint payout_failures.payout_failures_currency_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.payout_failures ADD CONSTRAINT payout_failures_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("pending_disputes", { schema: "zeroship" }).create({
    columns: {
      provider_dispute_id: t.text().notNull(),
      payment_intent: t.text(),
      charge: t.text(),
      amount_cents: t.bigInt().notNull(),
      currency: t.text().notNull(),
      reason: t.text(),
      evidence_due_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["provider_dispute_id"],
  });
  // TODO(dsl-v2): add structural column type support for character(3)
  raw({ sql: "ALTER TABLE ONLY zeroship.pending_disputes ALTER COLUMN currency TYPE character(3) USING currency::character(3)", reason: "column pending_disputes.currency uses PostgreSQL type character(3), which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.pending_disputes ALTER COLUMN currency SET DEFAULT 'usd'::bpchar", reason: "column pending_disputes.currency requires exact default 'usd'::bpchar, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): CHECK constraint pending_disputes.pending_disputes_amount_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.pending_disputes ADD CONSTRAINT pending_disputes_amount_cents_check CHECK ((amount_cents > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint pending_disputes.pending_disputes_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.pending_disputes ADD CONSTRAINT pending_disputes_check CHECK (((payment_intent IS NOT NULL) OR (charge IS NOT NULL)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint pending_disputes.pending_disputes_currency_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.pending_disputes ADD CONSTRAINT pending_disputes_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("plan_change_events", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      period: t.text().notNull(),
      from_plan_id: t.text(),
      to_plan_id: t.text().notNull(),
      effective_at: t.timestamp().notNull().default({ fn: "now" }),
      usage_at_change: t.json().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.billing_period
  raw({ sql: "ALTER TABLE ONLY zeroship.plan_change_events ALTER COLUMN period TYPE zeroship.billing_period USING period::zeroship.billing_period", reason: "column plan_change_events.period uses PostgreSQL type zeroship.billing_period, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.plan_change_events ALTER COLUMN usage_at_change SET DEFAULT '{}'::jsonb", reason: "column plan_change_events.usage_at_change requires exact default '{}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  table("plans", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      name: t.text().notNull(),
      base_fee_cents: t.bigInt().notNull().default(0),
      included_units: t.bigInt().notNull().default(0),
      fx_pico_cents_per_unit: t.bigInt(),
      spend_limit_default_cents: t.bigInt().notNull().default(0),
      assignable_by_creator: t.boolean().notNull().default(false),
      runtime_limits_json: t.json().notNull(),
      archived: t.boolean().notNull().default(false),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
      net_policy_limits_json: t.json().notNull(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.plans ALTER COLUMN net_policy_limits_json SET DEFAULT '{\"max_sockets\": 4, \"egress_ceiling_bytes\": 10485760}'::jsonb", reason: "column plans.net_policy_limits_json requires exact default '{\"max_sockets\": 4, \"egress_ceiling_bytes\": 10485760}'::jsonb, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): CHECK constraint plans.plans_base_fee_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.plans ADD CONSTRAINT plans_base_fee_cents_check CHECK ((base_fee_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint plans.plans_fx_pico_cents_per_unit_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.plans ADD CONSTRAINT plans_fx_pico_cents_per_unit_check CHECK (((fx_pico_cents_per_unit IS NULL) OR (fx_pico_cents_per_unit >= 1000)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint plans.plans_included_units_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.plans ADD CONSTRAINT plans_included_units_check CHECK ((included_units >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint plans.plans_spend_limit_default_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.plans ADD CONSTRAINT plans_spend_limit_default_cents_check CHECK ((spend_limit_default_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("pricing_config", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull().default("global"),
      fx_pico_cents_per_unit: t.bigInt().notNull(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): CHECK constraint pricing_config.pricing_config_fx_pico_cents_per_unit_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.pricing_config ADD CONSTRAINT pricing_config_fx_pico_cents_per_unit_check CHECK ((fx_pico_cents_per_unit >= 1000))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint pricing_config.pricing_config_id_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.pricing_config ADD CONSTRAINT pricing_config_id_check CHECK ((id = 'global'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("refund_provider_refs", { schema: "zeroship" }).create({
    columns: {
      refund_id: t.text().notNull(),
      provider: t.text().notNull(),
      ref_kind: t.text().notNull(),
      external_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["refund_id", "provider", "ref_kind"],
  });
  table("refunds", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      invoice_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      subtotal_cents: t.bigInt().notNull(),
      tax_cents: t.bigInt().notNull().default(0),
      currency: t.text().notNull(),
      destination: t.text().notNull(),
      reason: t.text(),
      idempotency_key: t.text().notNull(),
      request_fingerprint: t.text().notNull(),
      status: t.text().notNull().default("pending"),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      issued_at: t.timestamp(),
      failed_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for character(3)
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ALTER COLUMN currency TYPE character(3) USING currency::character(3)", reason: "column refunds.currency uses PostgreSQL type character(3), which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): default expression is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ALTER COLUMN currency SET DEFAULT 'usd'::bpchar", reason: "column refunds.currency requires exact default 'usd'::bpchar, which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): add structural column type support for zeroship.refund_destination
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ALTER COLUMN destination TYPE zeroship.refund_destination USING destination::zeroship.refund_destination", reason: "column refunds.destination uses PostgreSQL type zeroship.refund_destination, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for zeroship.refund_status
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ALTER COLUMN status TYPE zeroship.refund_status USING status::zeroship.refund_status", reason: "column refunds.status uses PostgreSQL type zeroship.refund_status, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint refunds.refund_amount_split needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ADD CONSTRAINT refund_amount_split CHECK ((amount_cents = (subtotal_cents + tax_cents)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint refunds.refunds_amount_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ADD CONSTRAINT refunds_amount_cents_check CHECK ((amount_cents > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint refunds.refunds_currency_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ADD CONSTRAINT refunds_currency_check CHECK ((currency ~ '^[a-z]{3}$'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint refunds.refunds_subtotal_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ADD CONSTRAINT refunds_subtotal_cents_check CHECK ((subtotal_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint refunds.refunds_tax_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.refunds ADD CONSTRAINT refunds_tax_cents_check CHECK ((tax_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("spend_state_history", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      period: t.text().notNull(),
      from_state: t.text().notNull(),
      to_state: t.text().notNull(),
      spend_cents: t.bigInt().notNull(),
      limit_cents: t.bigInt(),
      at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.billing_period
  raw({ sql: "ALTER TABLE ONLY zeroship.spend_state_history ALTER COLUMN period TYPE zeroship.billing_period USING period::zeroship.billing_period", reason: "column spend_state_history.period uses PostgreSQL type zeroship.billing_period, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for zeroship.spend_state
  raw({ sql: "ALTER TABLE ONLY zeroship.spend_state_history ALTER COLUMN from_state TYPE zeroship.spend_state USING from_state::zeroship.spend_state", reason: "column spend_state_history.from_state uses PostgreSQL type zeroship.spend_state, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for zeroship.spend_state
  raw({ sql: "ALTER TABLE ONLY zeroship.spend_state_history ALTER COLUMN to_state TYPE zeroship.spend_state USING to_state::zeroship.spend_state", reason: "column spend_state_history.to_state uses PostgreSQL type zeroship.spend_state, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint spend_state_history.spend_state_history_limit_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.spend_state_history ADD CONSTRAINT spend_state_history_limit_cents_check CHECK (((limit_cents IS NULL) OR (limit_cents >= 0)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint spend_state_history.spend_state_history_spend_cents_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.spend_state_history ADD CONSTRAINT spend_state_history_spend_cents_check CHECK ((spend_cents >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("stripe_events_seen", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      event_type: t.text().notNull(),
      seen_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["event_id"],
  });
  table("usage_aggregates", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      period: t.text().notNull(),
      metric: t.text().notNull(),
      total: t.bigInt().notNull().default(0),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id", "period", "metric"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.billing_period
  raw({ sql: "ALTER TABLE ONLY zeroship.usage_aggregates ALTER COLUMN period TYPE zeroship.billing_period USING period::zeroship.billing_period", reason: "column usage_aggregates.period uses PostgreSQL type zeroship.billing_period, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint usage_aggregates.usage_aggregates_total_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.usage_aggregates ADD CONSTRAINT usage_aggregates_total_check CHECK ((total >= 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("usage_reports_seen", { schema: "zeroship" }).create({
    columns: {
      worker_id: t.text().notNull(),
      sequence: t.bigInt().notNull(),
      period: t.text(),
      seen_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["worker_id", "sequence"],
  });
  // TODO(dsl-v2): add structural column type support for zeroship.billing_period
  raw({ sql: "ALTER TABLE ONLY zeroship.usage_reports_seen ALTER COLUMN period TYPE zeroship.billing_period USING period::zeroship.billing_period", reason: "column usage_reports_seen.period uses PostgreSQL type zeroship.billing_period, which is not in the current closed column lexicon/lowerer use-site set" });
}

export function down() {

}
