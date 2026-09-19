import { grant, now, raw, t, table, uuidV4 } from "@zeroship/migrate";

// The organization-rooted billing domain. `organization_billing` is the subject
// root -- one row per organization the platform bills -- and every other table
// here either extends it (`organization_billing_status`, its append-only
// history), records a connected payout account (`organization_accounts`, its
// history) or a fee schedule (`organization_fee_policy`), or points at the root
// as a child (`billing_customer_refs`, `billing_notifications`, and the carrier
// tables declared elsewhere: `credit_ledger`, `invoices`, `payouts`,
// `payout_failures`, `connect_checkout_failures`).
//
// THE SUBJECT IS `organizations.id`, WHICH IS TEXT. The `organization_billing`
// children reach the root through `organization_billing.organization_id`; the
// provider-account children (`payouts`, `payout_failures`,
// `connect_checkout_failures`) reach it through
// `organization_accounts.organization_id`; and `organization_fee_policy` points
// straight at `organizations`. Deleting a user
// therefore deletes no billing record: no edge here names `users`.
//
// DELETE SEMANTICS ARE PART OF THE SHAPE. A child that merely annotates the root
// cascades with it; `organization_accounts` carries no referential action, so an
// organization with a connected Stripe account is not deletable out from under
// it; and `payouts` RESTRICTs against `organization_accounts`, because a payout
// is money that moved and the account it moved to does not disappear beneath it.
// `organization_account_history` and `app_audit.organization_id` keep NO foreign
// key on purpose: both record a link that must outlive the row it names.
//
// ---- apps.organization_id is an FK-consumed copy, collated here ----
//
// `apps.organization_id` and its shape check are declared with the `apps` table
// in db/migrations-ts/20260702000200_control_tables.ts. This migration owns its
// bytewise collation, and
// db/migrations-ts/20260906000200_apps_project_ownership_key.ts owns the
// composite foreign key that consumes `(project_id, organization_id)` against
// `projects(id, organization_id)`. The split is engine-forced: the engine lowers
// a foreign key against a catalog snapshot taken before the migration runs, and
// a collation is applied by a `raw` island that no snapshot can see, so the
// collation and the key must live in separate, consecutive migrations. A raw
// `ADD CONSTRAINT` would have fit in one file and hidden the most load-bearing
// constraint in this change from the model.
//
// ---- what the collation block is for ----
//
// Every column here whose domain is a canonical typed id, and every foreign-key
// copy of one, is registered for bytewise ordering in this same migration. A
// copy that misses it cannot serve a join against the collated id from its own
// index, and NOTHING ERRORS -- the join simply degrades. That includes columns
// that are not the subject at all: `organization_billing_status.failed_invoice_id`
// and `organization_billing_status_history.id` are typed ids in their own right.
//
// NOTHING IS BACKFILLED. Pre-launch there is no deployed database holding these
// rows, and a development database with them is recreated rather than migrated.
// The tables are declared with their final subject column, so no row is ever
// read across a change.
export default {
  name: "apps_organization_and_billing_subject",
  schema() {
    // ---- the billing root and its children ---------------------------------
    table("organization_billing", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        organization_id: t.text().notNull(),
        default_pm_set: t.boolean().notNull().default(false),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("organization_billing", { schema: "zeroship" }).unique("organization_billing_natural_key").add({ columns: ["organization_id"] });
    table("organization_billing_status", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        organization_id: t.text().notNull(),
        state: t.domain("account_state").notNull().default("active"),
        past_due_since: t.timestamp(),
        suspended_at: t.timestamp(),
        last_payment_failure_at: t.timestamp(),
        failed_invoice_id: t.text(),
        last_event_at: t.timestamp(),
        last_recovered_at: t.timestamp(),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("organization_billing_status", { schema: "zeroship" }).unique("organization_billing_status_natural_key").add({ columns: ["organization_id"] });
    table("organization_billing_status_history", { schema: "zeroship" }).create({
      columns: {
        id: t.text().notNull(),
        organization_id: t.text().notNull(),
        from_state: t.domain("account_state").notNull(),
        to_state: t.domain("account_state").notNull(),
        reason: t.text(),
        at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("organization_accounts", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        organization_id: t.text().notNull(),
        stripe_account_id: t.text().notNull(),
        onboarded_at: t.timestamp().notNull().default(now()),
        unlinked_at: t.timestamp(),
        charges_enabled: t.boolean().notNull().default(false),
        payouts_enabled: t.boolean().notNull().default(false),
        details_submitted: t.boolean().notNull().default(false),
      },
      primaryKey: ["id"],
    });
    table("organization_accounts", { schema: "zeroship" }).unique("organization_accounts_natural_key").add({ columns: ["organization_id"] });
    table("organization_account_history", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        organization_id: t.text().notNull(),
        stripe_account_id: t.text().notNull(),
        linked_at: t.timestamp().notNull().default(now()),
        unlinked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("organization_fee_policy", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        organization_id: t.text().notNull(),
        kind: t.text().notNull(),
        amount_cents: t.bigInt(),
        percent_bps: t.int(),
        cap_cents: t.bigInt(),
        floor_cents: t.bigInt(),
        updated_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("organization_fee_policy", { schema: "zeroship" }).unique("organization_fee_policy_natural_key").add({ columns: ["organization_id"] });
    table("organization_fee_policy", { schema: "zeroship" })
      .check("organization_fee_policy_cap_nonneg")
      .add({ expr: (col) => col("cap_cents").isNull().or(col("cap_cents").ge(0)) });
    table("organization_fee_policy", { schema: "zeroship" })
      .check("organization_fee_policy_floor_le_cap")
      .add({
        expr: (col) =>
          col("floor_cents")
            .isNull()
            .or(col("cap_cents").isNull(), col("floor_cents").le(col("cap_cents"))),
      });
    table("organization_fee_policy", { schema: "zeroship" })
      .check("organization_fee_policy_floor_nonneg")
      .add({ expr: (col) => col("floor_cents").isNull().or(col("floor_cents").ge(0)) });
    table("organization_fee_policy", { schema: "zeroship" })
      .check("organization_fee_policy_kind_check")
      .add({ expr: (col) => col("kind").in(["fixed", "percent"]) });
    table("organization_fee_policy", { schema: "zeroship" })
      .check("organization_fee_policy_shape")
      .add({
        expr: (col) =>
          col("kind")
            .eq("fixed")
            .and(col("amount_cents").isNotNull(), col("amount_cents").ge(0))
            .or(
              col("kind")
                .eq("percent")
                .and(
                  col("percent_bps").isNotNull(),
                  col("percent_bps").ge(0).and(col("percent_bps").le(10000)),
                ),
            ),
      });
    // The provider-keyed children of the billing root.
    table("billing_customer_refs", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        organization_id: t.text().notNull(),
        provider: t.text().notNull(),
        external_id: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("billing_customer_refs", { schema: "zeroship" }).unique("billing_customer_refs_natural_key").add({ columns: ["organization_id", "provider"] });
    table("billing_customer_refs", { schema: "zeroship" })
      .unique("billing_customer_refs_external_id_key")
      .add({ columns: ["external_id"] });
    table("billing_notifications", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity(),
        organization_id: t.text().notNull(),
        kind: t.domain("billing_notification_kind").notNull(),
        transition_id: t.text().notNull(),
        status: t.domain("notification_status").notNull().default("pending"),
        claimed_at: t.timestamp().notNull().default(now()),
        sent_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("billing_notifications", { schema: "zeroship" }).unique("billing_notifications_natural_key").add({ columns: ["organization_id", "kind", "transition_id"] });

    // ---- bytewise ordering for every typed-id text column ------------------
    // The map is semantic, exactly as
    // db/migrations-ts/20260831000001_sortable_entity_id_collations.ts states:
    // these columns' whole domain is a canonical typed id or a copy of one. It
    // runs before the foreign keys below so no key is validated across a
    // collation that is about to change.
    const typedIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
      organization_billing: ["organization_id"],
      organization_billing_status: ["organization_id", "failed_invoice_id"],
      organization_billing_status_history: ["id", "organization_id"],
      organization_accounts: ["organization_id"],
      organization_account_history: ["organization_id"],
      organization_fee_policy: ["organization_id"],
      invoices: ["organization_id"],
      credit_ledger: ["organization_id"],
      billing_customer_refs: ["organization_id"],
      billing_notifications: ["organization_id", "transition_id"],
      payouts: ["organization_id"],
      payout_failures: ["organization_id"],
      connect_checkout_failures: ["organization_id"],
      app_audit: ["organization_id"],
      apps: ["organization_id"],
    };
    for (const [tableName, columns] of Object.entries(typedIdColumnsByTable)) {
      const alterations = columns
        .map((column) => `ALTER COLUMN "${column}" TYPE text COLLATE "C"`)
        .join(", ");
      raw({
        sql: `ALTER TABLE "zeroship"."${tableName}" ${alterations}`,
        reason:
          "typed-id text domains need bytewise comparison, including matching copies used by "
          + "indexed joins",
      });
    }

    // ---- the roots point at the organization -------------------------------
    table("organization_billing", { schema: "zeroship" })
      .foreignKey("organization_billing_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: { table: "organizations", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
      });
    // No referential action: an organization with a connected Stripe account is
    // not deletable out from under it.
    table("organization_accounts", { schema: "zeroship" })
      .foreignKey("organization_accounts_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: { table: "organizations", columns: ["id"], schema: "zeroship" },
      });
    table("organization_fee_policy", { schema: "zeroship" })
      .foreignKey("organization_fee_policy_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: { table: "organizations", columns: ["id"], schema: "zeroship" },
        onDelete: "cascade",
      });

    // ---- and every child keeps pointing at its root ------------------------
    const billingChildren: readonly (readonly [string, string])[] = [
      ["billing_customer_refs", "billing_customer_refs_organization_id_fkey"],
      ["billing_notifications", "billing_notifications_organization_id_fkey"],
      ["organization_billing_status", "organization_billing_status_organization_id_fkey"],
      [
        "organization_billing_status_history",
        "organization_billing_status_history_organization_id_fkey",
      ],
      ["credit_ledger", "credit_ledger_organization_id_fkey"],
      ["invoices", "invoices_organization_id_fkey"],
    ];
    for (const [child, constraintName] of billingChildren) {
      table(child, { schema: "zeroship" })
        .foreignKey(constraintName)
        .add({
          columns: ["organization_id"],
          references: {
            table: "organization_billing",
            columns: ["organization_id"],
            schema: "zeroship",
          },
          onDelete: "cascade",
        });
    }
    table("connect_checkout_failures", { schema: "zeroship" })
      .foreignKey("connect_checkout_failures_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: {
          table: "organization_accounts",
          columns: ["organization_id"],
          schema: "zeroship",
        },
        onDelete: "cascade",
      });
    table("payout_failures", { schema: "zeroship" })
      .foreignKey("payout_failures_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: {
          table: "organization_accounts",
          columns: ["organization_id"],
          schema: "zeroship",
        },
        onDelete: "cascade",
      });
    // RESTRICT: a payout is money that moved, and the account it moved to does
    // not disappear from under it.
    table("payouts", { schema: "zeroship" })
      .foreignKey("payouts_organization_id_fkey")
      .add({
        columns: ["organization_id"],
        references: {
          table: "organization_accounts",
          columns: ["organization_id"],
          schema: "zeroship",
        },
        onDelete: "restrict",
      });

    // ---- the indexes, under the subject's name -----------------------------
    table("app_audit", { schema: "zeroship" })
      .index("idx_app_audit_organization_at")
      .add({ on: ["organization_id", { column: "occurred_at", order: "desc" }] });
    table("connect_checkout_failures", { schema: "zeroship" })
      .index("connect_checkout_failures_organization_idx")
      .add({ on: ["organization_id", { column: "created_at", order: "desc" }] });
    table("credit_ledger", { schema: "zeroship" })
      .index("credit_ledger_organization_created_idx")
      .add({ on: ["organization_id", "created_at"] });
    // One live invoice per ORGANIZATION per period.
    table("invoices", { schema: "zeroship" })
      .index("invoices_organization_active_period_claim")
      .add({
        on: ["organization_id", "period"],
        unique: true,
        where: (col) => col("status").cast({ to: "text" }).ne("void"),
      });
    table("payout_failures", { schema: "zeroship" })
      .index("payout_failures_organization_idx")
      .add({ on: ["organization_id", { column: "created_at", order: "desc" }] });
    table("payouts", { schema: "zeroship" })
      .index("idx_payouts_organization_time")
      .add({ on: ["organization_id", { column: "occurred_at", order: "desc" }] });
    table("organization_account_history", { schema: "zeroship" })
      .index("idx_organization_account_history_organization")
      .add({ on: ["organization_id", { column: "linked_at", order: "desc" }] });
    table("organization_account_history", { schema: "zeroship" })
      .index("idx_organization_account_history_one_open")
      .add({
        on: ["organization_id"],
        unique: true,
        where: (col) => col("unlinked_at").isNull(),
      });
    table("organization_billing_status", { schema: "zeroship" })
      .index("idx_organization_billing_status_past_due")
      .add({
        on: ["past_due_since"],
        where: (col) => col("state").cast({ to: "text" }).eq("past_due"),
      });
    table("organization_billing_status_history", { schema: "zeroship" })
      .index("idx_organization_billing_status_history_organization_at")
      .add({ on: ["organization_id", { column: "at", order: "desc" }] });
    table("billing_notifications", { schema: "zeroship" })
      .index("idx_billing_notifications_pending")
      .add({
        on: ["claimed_at"],
        where: (col) => col("status").cast({ to: "text" }).eq("pending"),
      });

    // ---- the ACLs the control plane holds ----------------------------------
    // Privilege by privilege, including the asymmetries a uniform grant would
    // widen: only the billing root and the provider refs are deletable, and the
    // status history is append-only to the control plane.
    grant({
      privileges: ["select", "insert", "update", "delete"],
      on: {
        kind: "table",
        schema: "zeroship",
        names: ["organization_billing", "billing_customer_refs"],
      },
      to: ["zeroship_control"],
    });
    grant({
      privileges: ["select", "insert", "update"],
      on: {
        kind: "table",
        schema: "zeroship",
        names: [
          "organization_accounts",
          "organization_account_history",
          "organization_billing_status",
          "organization_fee_policy",
          "billing_notifications",
        ],
      },
      to: ["zeroship_control"],
    });
    grant({
      privileges: ["select", "insert"],
      on: {
        kind: "table",
        schema: "zeroship",
        names: ["organization_billing_status_history"],
      },
      to: ["zeroship_control"],
    });
  },
};
