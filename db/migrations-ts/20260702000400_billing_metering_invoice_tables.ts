import { table, t, now } from "@zeroship/migrate";

export const name = "billing_metering_invoice_tables";

export function up() {
  table("app_spend_limit", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      spend_limit_cents: t.bigInt(),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["app_id"],
  });
  table("app_spend_limit", { schema: "zeroship" }).check("app_spend_limit_spend_limit_cents_check").add({ expr: (col) => col("spend_limit_cents").isNull().or(col("spend_limit_cents").ge(0)) });
  table("app_spend_state", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      state: t.domain("spend_state").notNull().default("allow"),
      spend_cents: t.bigInt().notNull().default(0),
      eval_limit_cents: t.bigInt().notNull().default(0),
      period: t.domain("billing_period").notNull(),
      evaluated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["app_id"],
  });
  table("app_spend_state", { schema: "zeroship" }).check("app_spend_state_eval_limit_cents_check").add({ expr: (col) => col("eval_limit_cents").ge(0) });
  table("app_spend_state", { schema: "zeroship" }).check("app_spend_state_spend_cents_check").add({ expr: (col) => col("spend_cents").ge(0) });
  table("billing_customer_refs", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      provider: t.text().notNull(),
      external_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["creator_id", "provider"],
  });
  table("billing_disputes", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      invoice_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char({ length: 3 }).notNull().default("usd"),
      status: t.domain("dispute_status").notNull().default("open"),
      reason: t.text(),
      evidence_due_at: t.timestamp(),
      provider_dispute_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
      resolved_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("billing_disputes", { schema: "zeroship" }).check("billing_disputes_amount_cents_check").add({ expr: (col) => col("amount_cents").gt(0) });
  table("billing_disputes", { schema: "zeroship" }).check("billing_disputes_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("billing_line_provider_refs", { schema: "zeroship" }).create({
    columns: {
      invoice_id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      provider: t.text().notNull(),
      ref_kind: t.text().notNull(),
      external_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
      segment_no: t.smallInt().notNull().default(0),
    },
    primaryKey: ["invoice_id", "app_id", "segment_no", "provider", "ref_kind"],
  });
  table("billing_line_provider_refs", { schema: "zeroship" }).check("billing_line_provider_refs_segment_no_check").add({ expr: (col) => col("segment_no").ge(0) });
  table("billing_metrics", { schema: "zeroship" }).create({
    columns: {
      metric: t.text().notNull(),
      kind: t.domain("metric_kind").notNull(),
      unit: t.text().notNull(),
      archived: t.boolean().notNull().default(false),
      last_seen_at: t.timestamp(),
      created_at: t.timestamp().notNull().default(now()),
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
      claimed_at: t.timestamp().notNull().default(now()),
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
      created_at: t.timestamp().notNull().default(now()),
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
      detected_at: t.timestamp().notNull().default(now()),
      resolved_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("provider_dead_letter", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      provider_id: t.text().notNull(),
      event_id: t.text().notNull(),
      source: t.text().notNull(),
      subject: t.json().notNull(),
      meter: t.text().notNull(),
      event_time: t.bigInt().notNull(),
      value: t.bigInt().notNull(),
      dims: t.json().notNull(),
      reason: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("provider_dead_letter", { schema: "zeroship" }).check("provider_dead_letter_value_check").add({ expr: (col) => col("value").ge(0) });
  table("connect_checkout_failures", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      provider_payment_intent_id: t.text().notNull(),
      stripe_account_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char({ length: 3 }).notNull().default("usd"),
      failure_code: t.text(),
      failure_message: t.text(),
      occurred_at: t.timestamp().notNull(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("connect_checkout_failures", { schema: "zeroship" }).check("connect_checkout_failures_amount_cents_check").add({ expr: (col) => col("amount_cents").ge(0) });
  table("connect_checkout_failures", { schema: "zeroship" }).check("connect_checkout_failures_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("creator_billing", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      default_pm_set: t.boolean().notNull().default(false),
      created_at: t.timestamp().notNull().default(now()),
      updated_at: t.timestamp().notNull().default(now()),
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
      updated_at: t.timestamp().notNull().default(now()),
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
      at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("creator_fee_policy", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      kind: t.text().notNull(),
      amount_cents: t.bigInt(),
      percent_bps: t.int(),
      cap_cents: t.bigInt(),
      floor_cents: t.bigInt(),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["creator_id"],
  });
  table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_cap_nonneg").add({ expr: (col) => col("cap_cents").isNull().or(col("cap_cents").ge(0)) });
  table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_floor_le_cap").add({ expr: (col) => col("floor_cents").isNull().or(col("cap_cents").isNull(), col("floor_cents").le(col("cap_cents"))) });
  table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_floor_nonneg").add({ expr: (col) => col("floor_cents").isNull().or(col("floor_cents").ge(0)) });
  table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_kind_check").add({ expr: (col) => col("kind").in(["fixed", "percent"]) });
  table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_shape").add({ expr: (col) => col("kind").eq("fixed").and(col("amount_cents").isNotNull(), col("amount_cents").ge(0)).or(col("kind").eq("percent").and(col("percent_bps").isNotNull(), col("percent_bps").ge(0).and(col("percent_bps").le(10000)))) });
  table("credit_ledger", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      kind: t.domain("credit_entry_kind").notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char({ length: 3 }).notNull().default("usd"),
      applied_invoice_id: t.text(),
      consumed_from_grant_id: t.text(),
      expires_at: t.timestamp(),
      note: t.text(),
      idempotency_key: t.text(),
      request_fingerprint: t.text(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("credit_ledger", { schema: "zeroship" }).check("credit_ledger_amount_cents_check").add({ expr: (col) => col("amount_cents").ne(0) });
  table("credit_ledger", { schema: "zeroship" }).check("credit_ledger_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("credit_ledger", { schema: "zeroship" }).check("credit_ledger_grant_ref").add({ expr: (col) => col("kind").cast({ to: "text" }).in(["consumed", "void_reversal", "refund_clawback"]).and(col("consumed_from_grant_id").isNotNull()).or(col("kind").cast({ to: "text" }).notIn(["consumed", "void_reversal", "refund_clawback"]).and(col("consumed_from_grant_id").isNull())) });
  table("credit_ledger", { schema: "zeroship" }).check("credit_ledger_kind_sign").add({ expr: (col) => col("kind").cast({ to: "text" }).in(["consumed", "refund_clawback"]).and(col("amount_cents").lt(0)).or(col("kind").cast({ to: "text" }).notIn(["consumed", "refund_clawback"]).and(col("amount_cents").gt(0))) });
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
      created_at: t.timestamp().notNull().default(now()),
      segment_no: t.smallInt().notNull().default(0),
      plan_id: t.text().notNull(),
    },
    primaryKey: ["invoice_id", "app_id", "segment_no"],
  });
  table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_amount_cents_check").add({ expr: (col) => col("amount_cents").ge(0) });
  table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_base_fee_cents_check").add({ expr: (col) => col("base_fee_cents").ge(0) });
  table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_fx_pico_cents_per_unit_check").add({ expr: (col) => col("fx_pico_cents_per_unit").ge(1000) });
  table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_included_units_check").add({ expr: (col) => col("included_units").ge(0) });
  table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_segment_no_check").add({ expr: (col) => col("segment_no").ge(0) });
  table("invoice_payments", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      invoice_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char({ length: 3 }).notNull().default("usd"),
      kind: t.domain("invoice_payment_kind").notNull(),
      provider_ref: t.text(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("invoice_payments", { schema: "zeroship" }).check("invoice_payments_amount_cents_check").add({ expr: (col) => col("amount_cents").ne(0) });
  table("invoice_payments", { schema: "zeroship" }).check("invoice_payments_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("invoices", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      status: t.domain("invoice_status").notNull().default("draft"),
      currency: t.char({ length: 3 }).notNull().default("usd"),
      subtotal_cents: t.bigInt().notNull().default(0),
      credit_cents: t.bigInt().notNull().default(0),
      tax_cents: t.bigInt().notNull().default(0),
      total_cents: t.bigInt().notNull().default(0),
      finalized_at: t.timestamp(),
      voided_at: t.timestamp(),
      created_at: t.timestamp().notNull().default(now()),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("invoices", { schema: "zeroship" }).check("invoice_total_balances").add({ expr: (col) => col("total_cents").eq(col("subtotal_cents").sub(col("credit_cents")).add(col("tax_cents"))) });
  table("invoices", { schema: "zeroship" }).check("invoices_credit_cents_check").add({ expr: (col) => col("credit_cents").ge(0) });
  table("invoices", { schema: "zeroship" }).check("invoices_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("invoices", { schema: "zeroship" }).check("invoices_subtotal_cents_check").add({ expr: (col) => col("subtotal_cents").ge(0) });
  table("invoices", { schema: "zeroship" }).check("invoices_tax_cents_check").add({ expr: (col) => col("tax_cents").ge(0) });
  table("invoices", { schema: "zeroship" }).check("invoices_total_cents_check").add({ expr: (col) => col("total_cents").ge(0) });
  table("metering_exports", { schema: "zeroship" }).create({
    columns: {
      creator_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      exported_units: t.bigInt().notNull().default(0),
      consecutive_failures: t.int().notNull().default(0),
      last_error: t.text(),
      last_attempt_at: t.timestamp(),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["creator_id", "period"],
  });
  table("metering_exports", { schema: "zeroship" }).check("metering_exports_consecutive_failures_check").add({ expr: (col) => col("consecutive_failures").ge(0) });
  table("metering_exports", { schema: "zeroship" }).check("metering_exports_exported_units_check").add({ expr: (col) => col("exported_units").ge(0) });
  table("metric_weights", { schema: "zeroship" }).create({
    columns: {
      metric: t.text().notNull(),
      units_per_op: t.bigInt().notNull(),
      per_units: t.bigInt().notNull(),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["metric"],
  });
  table("metric_weights", { schema: "zeroship" }).check("metric_weights_per_units_check").add({ expr: (col) => col("per_units").gt(0) });
  table("metric_weights", { schema: "zeroship" }).check("metric_weights_units_per_op_check").add({ expr: (col) => col("units_per_op").ge(0) });
  table("payout_failures", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      creator_id: t.uuid().notNull(),
      provider_payout_id: t.text().notNull(),
      stripe_account_id: t.text().notNull(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char({ length: 3 }).notNull().default("usd"),
      failure_code: t.text(),
      failure_message: t.text(),
      occurred_at: t.timestamp().notNull(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("payout_failures", { schema: "zeroship" }).check("payout_failures_amount_cents_check").add({ expr: (col) => col("amount_cents").ge(0) });
  table("payout_failures", { schema: "zeroship" }).check("payout_failures_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("pending_disputes", { schema: "zeroship" }).create({
    columns: {
      provider_dispute_id: t.text().notNull(),
      payment_intent: t.text(),
      charge: t.text(),
      amount_cents: t.bigInt().notNull(),
      currency: t.char({ length: 3 }).notNull().default("usd"),
      reason: t.text(),
      evidence_due_at: t.timestamp(),
      created_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["provider_dispute_id"],
  });
  table("pending_disputes", { schema: "zeroship" }).check("pending_disputes_amount_cents_check").add({ expr: (col) => col("amount_cents").gt(0) });
  table("pending_disputes", { schema: "zeroship" }).check("pending_disputes_check").add({ expr: (col) => col("payment_intent").isNotNull().or(col("charge").isNotNull()) });
  table("pending_disputes", { schema: "zeroship" }).check("pending_disputes_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("plan_change_events", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      from_plan_id: t.text(),
      to_plan_id: t.text().notNull(),
      effective_at: t.timestamp().notNull().default(now()),
      usage_at_change: t.json().notNull().default({}),
      created_at: t.timestamp().notNull().default(now()),
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
      created_at: t.timestamp().notNull().default(now()),
      updated_at: t.timestamp().notNull().default(now()),
      net_policy_limits_json: t.json().notNull().default({ max_sockets: 4, egress_ceiling_bytes: 10485760 }),
    },
    primaryKey: ["id"],
  });
  table("plans", { schema: "zeroship" }).check("plans_base_fee_cents_check").add({ expr: (col) => col("base_fee_cents").ge(0) });
  table("plans", { schema: "zeroship" }).check("plans_fx_pico_cents_per_unit_check").add({ expr: (col) => col("fx_pico_cents_per_unit").isNull().or(col("fx_pico_cents_per_unit").ge(1000)) });
  table("plans", { schema: "zeroship" }).check("plans_included_units_check").add({ expr: (col) => col("included_units").ge(0) });
  table("plans", { schema: "zeroship" }).check("plans_spend_limit_default_cents_check").add({ expr: (col) => col("spend_limit_default_cents").ge(0) });
  table("pricing_config", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull().default("global"),
      fx_pico_cents_per_unit: t.bigInt().notNull(),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("pricing_config", { schema: "zeroship" }).check("pricing_config_fx_pico_cents_per_unit_check").add({ expr: (col) => col("fx_pico_cents_per_unit").ge(1000) });
  table("pricing_config", { schema: "zeroship" }).check("pricing_config_id_check").add({ expr: (col) => col("id").eq("global") });
  table("refund_provider_refs", { schema: "zeroship" }).create({
    columns: {
      refund_id: t.text().notNull(),
      provider: t.text().notNull(),
      ref_kind: t.text().notNull(),
      external_id: t.text().notNull(),
      created_at: t.timestamp().notNull().default(now()),
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
      currency: t.char({ length: 3 }).notNull().default("usd"),
      destination: t.domain("refund_destination").notNull(),
      reason: t.text(),
      idempotency_key: t.text().notNull(),
      request_fingerprint: t.text().notNull(),
      status: t.domain("refund_status").notNull().default("pending"),
      created_at: t.timestamp().notNull().default(now()),
      issued_at: t.timestamp(),
      failed_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  table("refunds", { schema: "zeroship" }).check("refund_amount_split").add({ expr: (col) => col("amount_cents").eq(col("subtotal_cents").add(col("tax_cents"))) });
  table("refunds", { schema: "zeroship" }).check("refunds_amount_cents_check").add({ expr: (col) => col("amount_cents").gt(0) });
  table("refunds", { schema: "zeroship" }).check("refunds_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
  table("refunds", { schema: "zeroship" }).check("refunds_subtotal_cents_check").add({ expr: (col) => col("subtotal_cents").ge(0) });
  table("refunds", { schema: "zeroship" }).check("refunds_tax_cents_check").add({ expr: (col) => col("tax_cents").ge(0) });
  table("spend_state_history", { schema: "zeroship" }).create({
    columns: {
      id: t.text().notNull(),
      app_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      from_state: t.domain("spend_state").notNull(),
      to_state: t.domain("spend_state").notNull(),
      spend_cents: t.bigInt().notNull(),
      limit_cents: t.bigInt(),
      at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["id"],
  });
  table("spend_state_history", { schema: "zeroship" }).check("spend_state_history_limit_cents_check").add({ expr: (col) => col("limit_cents").isNull().or(col("limit_cents").ge(0)) });
  table("spend_state_history", { schema: "zeroship" }).check("spend_state_history_spend_cents_check").add({ expr: (col) => col("spend_cents").ge(0) });
  table("stripe_events_seen", { schema: "zeroship" }).create({
    columns: {
      event_id: t.text().notNull(),
      event_type: t.text().notNull(),
      seen_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["event_id"],
  });
  table("usage_aggregates", { schema: "zeroship" }).create({
    columns: {
      app_id: t.uuid().notNull(),
      period: t.domain("billing_period").notNull(),
      metric: t.text().notNull(),
      total: t.bigInt().notNull().default(0),
      updated_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["app_id", "period", "metric"],
  });
  table("usage_aggregates", { schema: "zeroship" }).check("usage_aggregates_total_check").add({ expr: (col) => col("total").ge(0) });
  table("usage_reports_seen", { schema: "zeroship" }).create({
    columns: {
      worker_id: t.text().notNull(),
      sequence: t.bigInt().notNull(),
      period: t.domain("billing_period"),
      seen_at: t.timestamp().notNull().default(now()),
    },
    primaryKey: ["worker_id", "sequence"],
  });
}

export function down() {

}
