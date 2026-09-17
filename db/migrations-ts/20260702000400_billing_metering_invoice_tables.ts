import { table, t, now } from "@zeroship/migrate";

const userIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  billing_customer_refs: ["creator_id"],
  billing_notifications: ["creator_id"],
  connect_checkout_failures: ["creator_id"],
  creator_billing: ["creator_id"],
  creator_billing_status: ["creator_id"],
  creator_billing_status_history: ["creator_id"],
  creator_fee_policy: ["creator_id"],
  credit_ledger: ["creator_id"],
  invoices: ["creator_id"],
  metering_exports: ["creator_id"],
  payout_failures: ["creator_id"],
};

export default {
  name: "billing_metering_invoice_tables",
  schema() {
    table("app_spend_limit", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        spend_limit_cents: t.bigInt(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_spend_limit", { schema: "zeroship" }).unique("app_spend_limit_natural_key").add({ columns: ["app_id"] });
    table("app_spend_limit", { schema: "zeroship" }).check("app_spend_limit_spend_limit_cents_check").add({ expr: (col) => col("spend_limit_cents").isNull().or(col("spend_limit_cents").ge(0)) });
    table("app_spend_state", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        state: t.domain("spend_state").required().default("allow"),
        spend_cents: t.bigInt().required().default(0),
        eval_limit_cents: t.bigInt().required().default(0),
        period: t.domain("billing_period").required(),
        evaluated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("app_spend_state", { schema: "zeroship" }).unique("app_spend_state_natural_key").add({ columns: ["app_id"] });
    table("app_spend_state", { schema: "zeroship" }).check("app_spend_state_eval_limit_cents_check").add({ expr: (col) => col("eval_limit_cents").ge(0) });
    table("app_spend_state", { schema: "zeroship" }).check("app_spend_state_spend_cents_check").add({ expr: (col) => col("spend_cents").ge(0) });
    table("billing_customer_refs", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        creator_id: t.text().required(),
        provider: t.text().required(),
        external_id: t.text().required(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("billing_customer_refs", { schema: "zeroship" }).unique("billing_customer_refs_natural_key").add({ columns: ["creator_id", "provider"] });
    table("billing_disputes", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        invoice_id: t.text().required(),
        amount_cents: t.bigInt().required(),
        currency: t.char({ length: 3 }).required().default("usd"),
        status: t.domain("dispute_status").required().default("open"),
        reason: t.text(),
        evidence_due_at: t.timestamp(),
        provider_dispute_id: t.text().required(),
        created_at: t.timestamp().required().default(now()),
        resolved_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("billing_disputes", { schema: "zeroship" }).check("billing_disputes_amount_cents_check").add({ expr: (col) => col("amount_cents").gt(0) });
    table("billing_disputes", { schema: "zeroship" }).check("billing_disputes_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
    table("billing_line_provider_refs", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        invoice_id: t.text().required(),
        app_id: t.text().required(),
        provider: t.text().required(),
        ref_kind: t.text().required(),
        external_id: t.text().required(),
        created_at: t.timestamp().required().default(now()),
        segment_no: t.int().required().default(0),
      },
      primaryKey: ["id"],
    });
    table("billing_line_provider_refs", { schema: "zeroship" }).unique("billing_line_provider_refs_natural_key").add({ columns: ["invoice_id", "app_id", "segment_no", "provider", "ref_kind"] });
    table("billing_line_provider_refs", { schema: "zeroship" }).check("billing_line_provider_refs_segment_no_check").add({ expr: (col) => col("segment_no").ge(0) });
    table("billing_metrics", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        metric: t.text().required(),
        kind: t.domain("metric_kind").required(),
        unit: t.text().required(),
        archived: t.boolean().required().default(false),
        last_seen_at: t.timestamp(),
        created_at: t.timestamp().required().default(now()),
        owner_app: t.text(),
      },
      primaryKey: ["id"],
    });
    table("billing_metrics", { schema: "zeroship" }).unique("billing_metrics_natural_key").add({ columns: ["metric"] });
    table("billing_notifications", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        creator_id: t.text().required(),
        kind: t.domain("billing_notification_kind").required(),
        transition_id: t.text().required(),
        status: t.domain("notification_status").required().default("pending"),
        claimed_at: t.timestamp().required().default(now()),
        sent_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("billing_notifications", { schema: "zeroship" }).unique("billing_notifications_natural_key").add({ columns: ["creator_id", "kind", "transition_id"] });
    table("billing_provider_refs", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        invoice_id: t.text().required(),
        provider: t.text().required(),
        ref_kind: t.text().required(),
        external_id: t.text().required(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("billing_provider_refs", { schema: "zeroship" }).unique("billing_provider_refs_natural_key").add({ columns: ["invoice_id", "provider", "ref_kind"] });
    table("billing_reconciliation_findings", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        kind: t.domain("reconciliation_finding_kind").required(),
        severity: t.domain("reconciliation_finding_severity").required().default("medium"),
        entity_id: t.text().required(),
        our_value: t.json(),
        stripe_value: t.json(),
        dedup_key: t.text().required(),
        detected_at: t.timestamp().required().default(now()),
        resolved_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("provider_dead_letter", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        provider_id: t.text().required(),
        event_id: t.text().required(),
        source: t.text().required(),
        subject: t.json().required(),
        meter: t.text().required(),
        event_time: t.bigInt().required(),
        value: t.bigInt().required(),
        dims: t.json().required(),
        reason: t.text().required(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("provider_dead_letter", { schema: "zeroship" }).check("provider_dead_letter_value_check").add({ expr: (col) => col("value").ge(0) });
    table("connect_checkout_failures", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        creator_id: t.text().required(),
        provider_payment_intent_id: t.text().required(),
        stripe_account_id: t.text().required(),
        amount_cents: t.bigInt().required(),
        currency: t.char({ length: 3 }).required().default("usd"),
        failure_code: t.text(),
        failure_message: t.text(),
        occurred_at: t.timestamp().required(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("connect_checkout_failures", { schema: "zeroship" }).check("connect_checkout_failures_amount_cents_check").add({ expr: (col) => col("amount_cents").ge(0) });
    table("connect_checkout_failures", { schema: "zeroship" }).check("connect_checkout_failures_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
    table("creator_billing", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        creator_id: t.text().required(),
        default_pm_set: t.boolean().required().default(false),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("creator_billing", { schema: "zeroship" }).unique("creator_billing_natural_key").add({ columns: ["creator_id"] });
    table("creator_billing_status", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        creator_id: t.text().required(),
        state: t.domain("account_state").required().default("active"),
        past_due_since: t.timestamp(),
        suspended_at: t.timestamp(),
        last_payment_failure_at: t.timestamp(),
        failed_invoice_id: t.text(),
        last_event_at: t.timestamp(),
        last_recovered_at: t.timestamp(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("creator_billing_status", { schema: "zeroship" }).unique("creator_billing_status_natural_key").add({ columns: ["creator_id"] });
    table("creator_billing_status_history", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        creator_id: t.text().required(),
        from_state: t.domain("account_state").required(),
        to_state: t.domain("account_state").required(),
        reason: t.text(),
        at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("creator_fee_policy", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        creator_id: t.text().required(),
        kind: t.text().required(),
        amount_cents: t.bigInt(),
        percent_bps: t.int(),
        cap_cents: t.bigInt(),
        floor_cents: t.bigInt(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("creator_fee_policy", { schema: "zeroship" }).unique("creator_fee_policy_natural_key").add({ columns: ["creator_id"] });
    table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_cap_nonneg").add({ expr: (col) => col("cap_cents").isNull().or(col("cap_cents").ge(0)) });
    table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_floor_le_cap").add({ expr: (col) => col("floor_cents").isNull().or(col("cap_cents").isNull(), col("floor_cents").le(col("cap_cents"))) });
    table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_floor_nonneg").add({ expr: (col) => col("floor_cents").isNull().or(col("floor_cents").ge(0)) });
    table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_kind_check").add({ expr: (col) => col("kind").in(["fixed", "percent"]) });
    table("creator_fee_policy", { schema: "zeroship" }).check("creator_fee_policy_shape").add({ expr: (col) => col("kind").eq("fixed").and(col("amount_cents").isNotNull(), col("amount_cents").ge(0)).or(col("kind").eq("percent").and(col("percent_bps").isNotNull(), col("percent_bps").ge(0).and(col("percent_bps").le(10000)))) });
    table("credit_ledger", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        creator_id: t.text().required(),
        kind: t.domain("credit_entry_kind").required(),
        amount_cents: t.bigInt().required(),
        currency: t.char({ length: 3 }).required().default("usd"),
        applied_invoice_id: t.text(),
        consumed_from_grant_id: t.text(),
        expires_at: t.timestamp(),
        note: t.text(),
        idempotency_key: t.text(),
        request_fingerprint: t.text(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("credit_ledger", { schema: "zeroship" }).check("credit_ledger_amount_cents_check").add({ expr: (col) => col("amount_cents").ne(0) });
    table("credit_ledger", { schema: "zeroship" }).check("credit_ledger_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
    table("credit_ledger", { schema: "zeroship" }).check("credit_ledger_grant_ref").add({ expr: (col) => col("kind").cast({ to: "text" }).in(["consumed", "void_reversal", "refund_clawback"]).and(col("consumed_from_grant_id").isNotNull()).or(col("kind").cast({ to: "text" }).notIn(["consumed", "void_reversal", "refund_clawback"]).and(col("consumed_from_grant_id").isNull())) });
    table("credit_ledger", { schema: "zeroship" }).check("credit_ledger_kind_sign").add({ expr: (col) => col("kind").cast({ to: "text" }).in(["consumed", "refund_clawback"]).and(col("amount_cents").lt(0)).or(col("kind").cast({ to: "text" }).notIn(["consumed", "refund_clawback"]).and(col("amount_cents").gt(0))) });
    table("invoice_lines", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        invoice_id: t.text().required(),
        app_id: t.text().required(),
        included_units: t.bigInt().required(),
        fx_pico_cents_per_unit: t.bigInt().required(),
        base_fee_cents: t.bigInt().required().default(0),
        amount_cents: t.bigInt().required(),
        usage_snapshot: t.json().required(),
        weights_snapshot: t.json().required(),
        created_at: t.timestamp().required().default(now()),
        segment_no: t.int().required().default(0),
        plan_id: t.text().required(),
      },
      primaryKey: ["id"],
    });
    table("invoice_lines", { schema: "zeroship" }).unique("invoice_lines_natural_key").add({ columns: ["invoice_id", "app_id", "segment_no"] });
    table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_amount_cents_check").add({ expr: (col) => col("amount_cents").ge(0) });
    table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_base_fee_cents_check").add({ expr: (col) => col("base_fee_cents").ge(0) });
    table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_fx_pico_cents_per_unit_check").add({ expr: (col) => col("fx_pico_cents_per_unit").ge(1000) });
    table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_included_units_check").add({ expr: (col) => col("included_units").ge(0) });
    table("invoice_lines", { schema: "zeroship" }).check("invoice_lines_segment_no_check").add({ expr: (col) => col("segment_no").ge(0) });
    table("invoice_payments", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        invoice_id: t.text().required(),
        amount_cents: t.bigInt().required(),
        currency: t.char({ length: 3 }).required().default("usd"),
        kind: t.domain("invoice_payment_kind").required(),
        provider_ref: t.text(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("invoice_payments", { schema: "zeroship" }).check("invoice_payments_amount_cents_check").add({ expr: (col) => col("amount_cents").ne(0) });
    table("invoice_payments", { schema: "zeroship" }).check("invoice_payments_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
    table("invoices", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        creator_id: t.text().required(),
        period: t.domain("billing_period").required(),
        status: t.domain("invoice_status").required().default("draft"),
        currency: t.char({ length: 3 }).required().default("usd"),
        subtotal_cents: t.bigInt().required().default(0),
        credit_cents: t.bigInt().required().default(0),
        tax_cents: t.bigInt().required().default(0),
        total_cents: t.bigInt().required().default(0),
        finalized_at: t.timestamp(),
        voided_at: t.timestamp(),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
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
        id: t.bigInt().required().identity(),
        creator_id: t.text().required(),
        period: t.domain("billing_period").required(),
        exported_units: t.bigInt().required().default(0),
        consecutive_failures: t.int().required().default(0),
        last_error: t.text(),
        last_attempt_at: t.timestamp(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("metering_exports", { schema: "zeroship" }).unique("metering_exports_natural_key").add({ columns: ["creator_id", "period"] });
    table("metering_exports", { schema: "zeroship" }).check("metering_exports_consecutive_failures_check").add({ expr: (col) => col("consecutive_failures").ge(0) });
    table("metering_exports", { schema: "zeroship" }).check("metering_exports_exported_units_check").add({ expr: (col) => col("exported_units").ge(0) });
    table("metric_weights", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        metric: t.text().required(),
        units_per_op: t.bigInt().required(),
        per_units: t.bigInt().required(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("metric_weights", { schema: "zeroship" }).unique("metric_weights_natural_key").add({ columns: ["metric"] });
    table("metric_weights", { schema: "zeroship" }).check("metric_weights_per_units_check").add({ expr: (col) => col("per_units").gt(0) });
    table("metric_weights", { schema: "zeroship" }).check("metric_weights_units_per_op_check").add({ expr: (col) => col("units_per_op").ge(0) });
    table("payout_failures", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        creator_id: t.text().required(),
        provider_payout_id: t.text().required(),
        stripe_account_id: t.text().required(),
        amount_cents: t.bigInt().required(),
        currency: t.char({ length: 3 }).required().default("usd"),
        failure_code: t.text(),
        failure_message: t.text(),
        occurred_at: t.timestamp().required(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("payout_failures", { schema: "zeroship" }).check("payout_failures_amount_cents_check").add({ expr: (col) => col("amount_cents").ge(0) });
    table("payout_failures", { schema: "zeroship" }).check("payout_failures_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
    table("pending_disputes", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        provider_dispute_id: t.text().required(),
        payment_intent: t.text(),
        charge: t.text(),
        amount_cents: t.bigInt().required(),
        currency: t.char({ length: 3 }).required().default("usd"),
        reason: t.text(),
        evidence_due_at: t.timestamp(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("pending_disputes", { schema: "zeroship" }).unique("pending_disputes_natural_key").add({ columns: ["provider_dispute_id"] });
    table("pending_disputes", { schema: "zeroship" }).check("pending_disputes_amount_cents_check").add({ expr: (col) => col("amount_cents").gt(0) });
    table("pending_disputes", { schema: "zeroship" }).check("pending_disputes_check").add({ expr: (col) => col("payment_intent").isNotNull().or(col("charge").isNotNull()) });
    table("pending_disputes", { schema: "zeroship" }).check("pending_disputes_currency_check").add({ expr: (col) => col("currency").regex("^[a-z]{3}$") });
    table("plan_change_events", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        app_id: t.text().required(),
        period: t.domain("billing_period").required(),
        from_plan_id: t.text(),
        to_plan_id: t.text().required(),
        effective_at: t.timestamp().required().default(now()),
        usage_at_change: t.json().required().default({}),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("plans", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        name: t.text().required(),
        base_fee_cents: t.bigInt().required().default(0),
        included_units: t.bigInt().required().default(0),
        fx_pico_cents_per_unit: t.bigInt(),
        spend_limit_default_cents: t.bigInt().required().default(0),
        assignable_by_creator: t.boolean().required().default(false),
        workflows_allowed: t.boolean().required().default(false),
        workflow_policy_json: t.json(),
        runtime_limits_json: t.json().required(),
        archived: t.boolean().required().default(false),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
        net_policy_limits_json: t.json().required().default({ max_sockets: 4, egress_ceiling_bytes: 10485760 }),
      },
      primaryKey: ["id"],
    });
    table("plans", { schema: "zeroship" }).check("plans_base_fee_cents_check").add({ expr: (col) => col("base_fee_cents").ge(0) });
    table("plans", { schema: "zeroship" }).check("plans_fx_pico_cents_per_unit_check").add({ expr: (col) => col("fx_pico_cents_per_unit").isNull().or(col("fx_pico_cents_per_unit").ge(1000)) });
    table("plans", { schema: "zeroship" }).check("plans_included_units_check").add({ expr: (col) => col("included_units").ge(0) });
    table("plans", { schema: "zeroship" }).check("plans_spend_limit_default_cents_check").add({ expr: (col) => col("spend_limit_default_cents").ge(0) });
    table("pricing_config", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required().default("global"),
        fx_pico_cents_per_unit: t.bigInt().required(),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("pricing_config", { schema: "zeroship" }).check("pricing_config_fx_pico_cents_per_unit_check").add({ expr: (col) => col("fx_pico_cents_per_unit").ge(1000) });
    table("pricing_config", { schema: "zeroship" }).check("pricing_config_id_check").add({ expr: (col) => col("id").eq("global") });
    table("refund_provider_refs", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        refund_id: t.text().required(),
        provider: t.text().required(),
        ref_kind: t.text().required(),
        external_id: t.text().required(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("refund_provider_refs", { schema: "zeroship" }).unique("refund_provider_refs_natural_key").add({ columns: ["refund_id", "provider", "ref_kind"] });
    table("refunds", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        invoice_id: t.text().required(),
        amount_cents: t.bigInt().required(),
        subtotal_cents: t.bigInt().required(),
        tax_cents: t.bigInt().required().default(0),
        currency: t.char({ length: 3 }).required().default("usd"),
        destination: t.domain("refund_destination").required(),
        reason: t.text(),
        idempotency_key: t.text().required(),
        request_fingerprint: t.text().required(),
        status: t.domain("refund_status").required().default("pending"),
        created_at: t.timestamp().required().default(now()),
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
        id: t.text().required(),
        app_id: t.text().required(),
        period: t.domain("billing_period").required(),
        from_state: t.domain("spend_state").required(),
        to_state: t.domain("spend_state").required(),
        spend_cents: t.bigInt().required(),
        limit_cents: t.bigInt(),
        at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("spend_state_history", { schema: "zeroship" }).check("spend_state_history_limit_cents_check").add({ expr: (col) => col("limit_cents").isNull().or(col("limit_cents").ge(0)) });
    table("spend_state_history", { schema: "zeroship" }).check("spend_state_history_spend_cents_check").add({ expr: (col) => col("spend_cents").ge(0) });
    table("stripe_events_seen", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        event_id: t.text().required(),
        event_type: t.text().required(),
        seen_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("stripe_events_seen", { schema: "zeroship" }).unique("stripe_events_seen_natural_key").add({ columns: ["event_id"] });
    table("usage_aggregates", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_id: t.text().required(),
        period: t.domain("billing_period").required(),
        metric: t.text().required(),
        total: t.bigInt().required().default(0),
        updated_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("usage_aggregates", { schema: "zeroship" }).unique("usage_aggregates_natural_key").add({ columns: ["app_id", "period", "metric"] });
    table("usage_aggregates", { schema: "zeroship" }).check("usage_aggregates_total_check").add({ expr: (col) => col("total").ge(0) });
    for (const [tableName, columns] of Object.entries(userIdColumnsByTable)) {
      for (const column of columns) {
        table(tableName, { schema: "zeroship" })
          .check(`${tableName}_${column}_usr_shape`)
          .add({ expr: (col) => col(column).regex("^usr_[0-9a-z]{25}$") });
      }
    }
  },
};
