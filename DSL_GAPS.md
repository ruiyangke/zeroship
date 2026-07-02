# DSL gaps

Current census after the Slice 8 platform baseline re-authoring pass.

Every remaining `raw()` marker in `db/migrations-ts` was kept because the current
keystone DSL/renderer still cannot express the fragment byte-faithfully enough
for the platform baseline schema.

## Raw marker count by file

| File | Remaining `TODO(dsl-v2)` |
| --- | ---: |
| `20260702000100_schema_roles_extensions.ts` | 14 |
| `20260702000200_control_tables.ts` | 20 |
| `20260702000300_auth_oauth_tables.ts` | 41 |
| `20260702000400_billing_metering_invoice_tables.ts` | 97 |
| `20260702000500_sandbox_tables.ts` | 72 |
| `20260702000600_constraints_indexes_fks.ts` | 127 |
| `20260702000700_functions_triggers_comments.ts` | 1 |
| `20260702000800_policies_rls.ts` | 0 |
| `20260702000900_grants.ts` | 0 |
| **Total** | **372** |

Previous baseline: 519. Slice 8 converted 147 markers to structural authoring.

## Remaining gaps by category

| Category | Count | Backlog |
| --- | ---: | --- |
| CHECK constraints needing Expr->SQL renderer | 133 | P1 Expr renderer |
| Rich index features | 68 | P3 rich index model/rendering |
| Other exact DDL fragments | 45 | P3 exact platform fragments |
| Defaults outside scalar/synth carrier | 26 | P3 default expression/cast fidelity |
| Multi-column/non-id foreign keys | 22 | P3 richer FK model |
| Domain CHECK predicates needing Expr->SQL renderer | 13 | P1 Expr renderer |
| Column type support: `text[]` | 12 | P3 platform type coverage |
| Column type support: `character(3)` | 8 | P3 platform type coverage |
| Column type support: `public.citext` | 7 | P3 platform type coverage |
| Column type support: `zeroship.billing_period` | 7 | P3 platform type coverage |
| Column type support: `smallint` | 5 | P3 platform type coverage |
| Column type support: `inet` | 4 | P3 platform type coverage |
| Column type support: `zeroship.account_state` | 3 | P3 platform type coverage |
| Column type support: `zeroship.spend_state` | 3 | P3 platform type coverage |
| Partitioned table DDL | 2 | P3 partition model |
| Column type support: `real` | 1 | P3 platform type coverage |
| Column type support: `zeroship.billing_notification_kind` | 1 | P3 platform type coverage |
| Column type support: `zeroship.credit_entry_kind` | 1 | P3 platform type coverage |
| Column type support: `zeroship.dispute_status` | 1 | P3 platform type coverage |
| Column type support: `zeroship.invoice_payment_kind` | 1 | P3 platform type coverage |
| Column type support: `zeroship.invoice_status` | 1 | P3 platform type coverage |
| Column type support: `zeroship.metric_kind` | 1 | P3 platform type coverage |
| Column type support: `zeroship.notification_status` | 1 | P3 platform type coverage |
| Column type support: `zeroship.reconciliation_finding_kind` | 1 | P3 platform type coverage |
| Column type support: `zeroship.reconciliation_finding_severity` | 1 | P3 platform type coverage |
| Column type support: `zeroship.refund_destination` | 1 | P3 platform type coverage |
| Column type support: `zeroship.refund_status` | 1 | P3 platform type coverage |
| Domain base type/date + EXTRACT predicate | 1 | P1 Expr renderer / domain support |
| Trigger UPDATE OF column-list | 1 | P3 trigger model |

## Notes

- Core exact platform tables that use supported physical types are now structural
  `table(...).create(...)` operations. Remaining table fragments are caused by
  missing type coverage, platform-only exact fragments such as partitioning or
  sequence ownership, or defaults that must retain exact PostgreSQL casts.
- Basic structural attachments are now structural where supported:
  `foreignKey`, `index`, `unique`, `createTrigger`, RLS, policies, and comments.
- Remaining CHECK/domain fragments are intentionally raw until the Expr->SQL
  renderer can faithfully emit constructs such as `ANY(ARRAY[...])`, `EXTRACT`,
  range predicates, regex matches, and richer boolean expressions.
- Remaining rich indexes are intentionally raw until the index model can express
  features such as BRIN/GIN, INCLUDE, opclasses, partial predicates,
  expression indexes, `ONLY`, ordering, and null-order modifiers.
