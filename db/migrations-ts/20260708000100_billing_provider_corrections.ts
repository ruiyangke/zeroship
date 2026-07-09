import { raw } from "@zeroship/migrate";

export const name = "billing_provider_corrections";

export function up() {
  raw({
    reason: "S7 billing-provider correction findings add provider drift and late adjustment kinds.",
    sql: `
ALTER DOMAIN zeroship.reconciliation_finding_kind
  DROP CONSTRAINT reconciliation_finding_kind_check;
ALTER DOMAIN zeroship.reconciliation_finding_kind
  ADD CONSTRAINT reconciliation_finding_kind_check
  CHECK (VALUE IN (
    'missed_invoice_payment',
    'invoice_status_drift',
    'refund_status_drift',
    'dispute_status_drift',
    'missing_dispute',
    'provider_reject',
    'provider_meter_drift',
    'late_period_adjustment',
    'correction_unpriceable',
    'forwarder_down_exceeds_retention',
    'subject_attribution_mismatch',
    'terminal_period_trueup'
  ));
`,
  });

  raw({
    reason: "S7 signed correction invoice lines need a line kind and stable correction dedup key.",
    sql: `
ALTER TABLE zeroship.invoice_lines
  ADD COLUMN line_kind text NOT NULL DEFAULT 'usage',
  ADD COLUMN correction_dedup_key text;

ALTER TABLE zeroship.invoice_lines
  DROP CONSTRAINT invoice_lines_amount_cents_check;
ALTER TABLE zeroship.invoice_lines
  ADD CONSTRAINT invoice_lines_amount_cents_check
  CHECK (
    (line_kind = 'usage' AND amount_cents >= 0 AND correction_dedup_key IS NULL)
    OR (line_kind = 'debit_note' AND amount_cents > 0 AND correction_dedup_key IS NOT NULL)
    OR (line_kind = 'credit_note' AND amount_cents < 0 AND correction_dedup_key IS NOT NULL)
  );
ALTER TABLE zeroship.invoice_lines
  ADD CONSTRAINT invoice_lines_line_kind_check
  CHECK (line_kind IN ('usage', 'debit_note', 'credit_note'));

CREATE UNIQUE INDEX invoice_lines_correction_dedup_key_key
  ON zeroship.invoice_lines (correction_dedup_key)
  WHERE correction_dedup_key IS NOT NULL;
`,
  });
}

export function down() {

}
