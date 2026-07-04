import { and, membership, notMembership, or, table, t } from "@zeroship/migrate";
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
  table("app_spend_limit", { schema: "zeroship" }).addCheck("app_spend_limit_spend_limit_cents_check", (c) => or(c("spend_limit_cents").isNull(), c("spend_limit_cents").ge(0)));
  table("app_spend_state", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      state: t.domain("spend_state").notNull().default("allow"),
      spend_cents: t.bigInt().notNull().default(0),
      eval_limit_cents: t.bigInt().notNull().default(0),
      period: t.domain("billing_period").notNull(),
      evaluated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id"],
  });
  table("app_spend_state", { schema: "zeroship" }).addCheck("app_spend_state_eval_limit_cents_check", (c) => c("eval_limit_cents").ge(0));
  table("app_spend_state", { schema: "zeroship" }).addCheck("app_spend_state_spend_cents_check", (c) => c("spend_cents").ge(0));
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
      currency: t.char(3).notNull().default("usd"),
      status: t.domain("dispute_status").notNull().default("open"),
      reason: t.text(),
      evidence_due_at: t.timestamp(),
      provider_dispute_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      resolved_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("billing_disputes", { schema: "zeroship" }).addCheck("billing_disputes_amount_cents_check", (c) => c("amount_cents").gt(0));
  table("billing_disputes", { schema: "zeroship" }).addCheck("billing_disputes_currency_check", (c) => c("currency").matches("^[a-z]{3}$"));
  table("billing_line_provider_refs", { schema: "zeroship" }).create({
    columns: {
      invoice_id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      provider: t.text().notNull(),
      ref_kind: t.text().notNull(),
      external_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      segment_no: t.smallInt().notNull().default(0),
    },
    primaryKey: ["invoice_id", "app_id", "segment_no", "provider", "ref_kind"],
  });
  table("billing_line_provider_refs", { schema: "zeroship" }).addCheck("billing_line_provider_refs_segment_no_check", (c) => c("segment_no").ge(0));
  table("billing_metrics", { schema: "zeroship" }).create({
    columns: {
      metric: t.text().notNull(),
      kind: t.domain("metric_kind").notNull(),
      unit: t.text().notNull(),
      archived: t.boolean().notNull().default(false),
      last_seen_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      owner_app: t.uuid(),
    },
    primaryKey: ["metric"],
  });
  table("billing_notifications", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      kind: t.domain("billing_notification_kind").notNull(),
      transition_id: t.text().notNull(),
      status: t.domain("notification_status").notNull().default("pending"),
      claimed_at: t.timestamp().notNull().default({ fn: "now" }),
      sent_at: t.timestamp(),
    },
    primaryKey: ["creator_id", "kind", "transition_id"],
  });
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
      kind: t.domain("reconciliation_finding_kind").notNull(),
      severity: t.domain("reconciliation_finding_severity").notNull().default("medium"),
      entity_id: t.text().notNull(),
      our_value: t.json(),
      stripe_value: t.json(),
      dedup_key: t.text().notNull(),
      detected_at: t.timestamp().notNull().default({ fn: "now" }),
      resolved_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("connect_checkout_failures", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      provider_payment_intent_id: t.text().notNull(),
      stripe_account_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char(3).notNull().default("usd"),
      failure_code: t.text(),
      failure_message: t.text(),
      occurred_at: t.timestamp().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  table("connect_checkout_failures", { schema: "zeroship" }).addCheck("connect_checkout_failures_amount_cents_check", (c) => c("amount_cents").ge(0));
  table("connect_checkout_failures", { schema: "zeroship" }).addCheck("connect_checkout_failures_currency_check", (c) => c("currency").matches("^[a-z]{3}$"));
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
      state: t.domain("account_state").notNull().default("active"),
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
  table("creator_billing_status_history", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      from_state: t.domain("account_state").notNull(),
      to_state: t.domain("account_state").notNull(),
      reason: t.text(),
      at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
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
  table("creator_fee_policy", { schema: "zeroship" }).addCheck("creator_fee_policy_cap_nonneg", (c) => or(c("cap_cents").isNull(), c("cap_cents").ge(0)));
  table("creator_fee_policy", { schema: "zeroship" }).addCheck("creator_fee_policy_floor_le_cap", (c) => or(c("floor_cents").isNull(), c("cap_cents").isNull(), c("floor_cents").le(c("cap_cents"))));
  table("creator_fee_policy", { schema: "zeroship" }).addCheck("creator_fee_policy_floor_nonneg", (c) => or(c("floor_cents").isNull(), c("floor_cents").ge(0)));
  table("creator_fee_policy", { schema: "zeroship" }).addCheck("creator_fee_policy_kind_check", (c) => membership(c("kind"), ["fixed", "percent"]));
  table("creator_fee_policy", { schema: "zeroship" }).addCheck("creator_fee_policy_shape", (c) => or(and(c("kind").eq("fixed"), c("amount_cents").isNotNull(), c("amount_cents").ge(0)), and(c("kind").eq("percent"), c("percent_bps").isNotNull(), and(c("percent_bps").ge(0), c("percent_bps").le(10000)))));
  table("credit_ledger", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      kind: t.domain("credit_entry_kind").notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char(3).notNull().default("usd"),
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
  table("credit_ledger", { schema: "zeroship" }).addCheck("credit_ledger_amount_cents_check", (c) => c("amount_cents").ne(0));
  table("credit_ledger", { schema: "zeroship" }).addCheck("credit_ledger_currency_check", (c) => c("currency").matches("^[a-z]{3}$"));
  table("credit_ledger", { schema: "zeroship" }).addCheck("credit_ledger_grant_ref", (c) => or(and(membership(c("kind").cast("text"), ["consumed", "void_reversal", "refund_clawback"]), c("consumed_from_grant_id").isNotNull()), and(notMembership(c("kind").cast("text"), ["consumed", "void_reversal", "refund_clawback"]), c("consumed_from_grant_id").isNull())));
  table("credit_ledger", { schema: "zeroship" }).addCheck("credit_ledger_kind_sign", (c) => or(and(membership(c("kind").cast("text"), ["consumed", "refund_clawback"]), c("amount_cents").lt(0)), and(notMembership(c("kind").cast("text"), ["consumed", "refund_clawback"]), c("amount_cents").gt(0))));
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
      segment_no: t.smallInt().notNull().default(0),
      plan_id: t.text().notNull(),
    },
    primaryKey: ["invoice_id", "app_id", "segment_no"],
  });
  table("invoice_lines", { schema: "zeroship" }).addCheck("invoice_lines_amount_cents_check", (c) => c("amount_cents").ge(0));
  table("invoice_lines", { schema: "zeroship" }).addCheck("invoice_lines_base_fee_cents_check", (c) => c("base_fee_cents").ge(0));
  table("invoice_lines", { schema: "zeroship" }).addCheck("invoice_lines_fx_pico_cents_per_unit_check", (c) => c("fx_pico_cents_per_unit").ge(1000));
  table("invoice_lines", { schema: "zeroship" }).addCheck("invoice_lines_included_units_check", (c) => c("included_units").ge(0));
  table("invoice_lines", { schema: "zeroship" }).addCheck("invoice_lines_segment_no_check", (c) => c("segment_no").ge(0));
  table("invoice_payments", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      invoice_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char(3).notNull().default("usd"),
      kind: t.domain("invoice_payment_kind").notNull(),
      provider_ref: t.text(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  table("invoice_payments", { schema: "zeroship" }).addCheck("invoice_payments_amount_cents_check", (c) => c("amount_cents").ne(0));
  table("invoice_payments", { schema: "zeroship" }).addCheck("invoice_payments_currency_check", (c) => c("currency").matches("^[a-z]{3}$"));
  table("invoices", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      status: t.domain("invoice_status").notNull().default("draft"),
      currency: t.char(3).notNull().default("usd"),
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
  table("invoices", { schema: "zeroship" }).addCheck("invoice_total_balances", (c) => c("total_cents").eq(c("subtotal_cents").sub(c("credit_cents")).add(c("tax_cents"))));
  table("invoices", { schema: "zeroship" }).addCheck("invoices_credit_cents_check", (c) => c("credit_cents").ge(0));
  table("invoices", { schema: "zeroship" }).addCheck("invoices_currency_check", (c) => c("currency").matches("^[a-z]{3}$"));
  table("invoices", { schema: "zeroship" }).addCheck("invoices_subtotal_cents_check", (c) => c("subtotal_cents").ge(0));
  table("invoices", { schema: "zeroship" }).addCheck("invoices_tax_cents_check", (c) => c("tax_cents").ge(0));
  table("invoices", { schema: "zeroship" }).addCheck("invoices_total_cents_check", (c) => c("total_cents").ge(0));
  table("metering_exports", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      exported_units: t.bigInt().notNull().default(0),
      consecutive_failures: t.integer().notNull().default(0),
      last_error: t.text(),
      last_attempt_at: t.timestamp(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["creator_id", "period"],
  });
  table("metering_exports", { schema: "zeroship" }).addCheck("metering_exports_consecutive_failures_check", (c) => c("consecutive_failures").ge(0));
  table("metering_exports", { schema: "zeroship" }).addCheck("metering_exports_exported_units_check", (c) => c("exported_units").ge(0));
  table("metric_weights", { schema: "zeroship" }).create({
    columns: {
      metric: t.text().notNull(),
      units_per_op: t.bigInt().notNull(),
      per_units: t.bigInt().notNull(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["metric"],
  });
  table("metric_weights", { schema: "zeroship" }).addCheck("metric_weights_per_units_check", (c) => c("per_units").gt(0));
  table("metric_weights", { schema: "zeroship" }).addCheck("metric_weights_units_per_op_check", (c) => c("units_per_op").ge(0));
  table("payout_failures", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      provider_payout_id: t.text().notNull(),
      stripe_account_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char(3).notNull().default("usd"),
      failure_code: t.text(),
      failure_message: t.text(),
      occurred_at: t.timestamp().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  table("payout_failures", { schema: "zeroship" }).addCheck("payout_failures_amount_cents_check", (c) => c("amount_cents").ge(0));
  table("payout_failures", { schema: "zeroship" }).addCheck("payout_failures_currency_check", (c) => c("currency").matches("^[a-z]{3}$"));
  table("pending_disputes", { schema: "zeroship" }).create({
    columns: {
      provider_dispute_id: t.text().notNull(),
      payment_intent: t.text(),
      charge: t.text(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char(3).notNull().default("usd"),
      reason: t.text(),
      evidence_due_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["provider_dispute_id"],
  });
  table("pending_disputes", { schema: "zeroship" }).addCheck("pending_disputes_amount_cents_check", (c) => c("amount_cents").gt(0));
  table("pending_disputes", { schema: "zeroship" }).addCheck("pending_disputes_check", (c) => or(c("payment_intent").isNotNull(), c("charge").isNotNull()));
  table("pending_disputes", { schema: "zeroship" }).addCheck("pending_disputes_currency_check", (c) => c("currency").matches("^[a-z]{3}$"));
  table("plan_change_events", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      from_plan_id: t.text(),
      to_plan_id: t.text().notNull(),
      effective_at: t.timestamp().notNull().default({ fn: "now" }),
      usage_at_change: t.json().notNull().default({}),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
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
      net_policy_limits_json: t.json().notNull().default({ max_sockets: 4, egress_ceiling_bytes: 10485760 }),
    },
    primaryKey: ["id"],
  });
  table("plans", { schema: "zeroship" }).addCheck("plans_base_fee_cents_check", (c) => c("base_fee_cents").ge(0));
  table("plans", { schema: "zeroship" }).addCheck("plans_fx_pico_cents_per_unit_check", (c) => or(c("fx_pico_cents_per_unit").isNull(), c("fx_pico_cents_per_unit").ge(1000)));
  table("plans", { schema: "zeroship" }).addCheck("plans_included_units_check", (c) => c("included_units").ge(0));
  table("plans", { schema: "zeroship" }).addCheck("plans_spend_limit_default_cents_check", (c) => c("spend_limit_default_cents").ge(0));
  table("pricing_config", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull().default("global"),
      fx_pico_cents_per_unit: t.bigInt().notNull(),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  table("pricing_config", { schema: "zeroship" }).addCheck("pricing_config_fx_pico_cents_per_unit_check", (c) => c("fx_pico_cents_per_unit").ge(1000));
  table("pricing_config", { schema: "zeroship" }).addCheck("pricing_config_id_check", (c) => c("id").eq("global"));
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
      currency: t.char(3).notNull().default("usd"),
      destination: t.domain("refund_destination").notNull(),
      reason: t.text(),
      idempotency_key: t.text().notNull(),
      request_fingerprint: t.text().notNull(),
      status: t.domain("refund_status").notNull().default("pending"),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      issued_at: t.timestamp(),
      failed_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("refunds", { schema: "zeroship" }).addCheck("refund_amount_split", (c) => c("amount_cents").eq(c("subtotal_cents").add(c("tax_cents"))));
  table("refunds", { schema: "zeroship" }).addCheck("refunds_amount_cents_check", (c) => c("amount_cents").gt(0));
  table("refunds", { schema: "zeroship" }).addCheck("refunds_currency_check", (c) => c("currency").matches("^[a-z]{3}$"));
  table("refunds", { schema: "zeroship" }).addCheck("refunds_subtotal_cents_check", (c) => c("subtotal_cents").ge(0));
  table("refunds", { schema: "zeroship" }).addCheck("refunds_tax_cents_check", (c) => c("tax_cents").ge(0));
  table("spend_state_history", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      from_state: t.domain("spend_state").notNull(),
      to_state: t.domain("spend_state").notNull(),
      spend_cents: t.bigInt().notNull(),
      limit_cents: t.bigInt(),
      at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  table("spend_state_history", { schema: "zeroship" }).addCheck("spend_state_history_limit_cents_check", (c) => or(c("limit_cents").isNull(), c("limit_cents").ge(0)));
  table("spend_state_history", { schema: "zeroship" }).addCheck("spend_state_history_spend_cents_check", (c) => c("spend_cents").ge(0));
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
      period: t.domain("billing_period").notNull(),
      metric: t.text().notNull(),
      total: t.bigInt().notNull().default(0),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["app_id", "period", "metric"],
  });
  table("usage_aggregates", { schema: "zeroship" }).addCheck("usage_aggregates_total_check", (c) => c("total").ge(0));
  table("usage_reports_seen", { schema: "zeroship" }).create({
    columns: {
      worker_id: t.text().notNull(),
      sequence: t.bigInt().notNull(),
      period: t.domain("billing_period"),
      seen_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["worker_id", "sequence"],
  });
}

export function down() {

}
