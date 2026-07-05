// op.* migration fixture — PostgreSQL online constraint adoption:
// addForeignKey / addCheck with `notValid: true` (rendered `ADD CONSTRAINT …
// NOT VALID`), then a later `validateConstraint(name)` (rendered `ALTER TABLE …
// VALIDATE CONSTRAINT …`). Gates the new `not_valid` FK/CHECK facet + the new
// `Op::ValidateConstraint` through the REAL recorder → frozen wire ops.
import { table } from "@zeroship/migrate";

export const name = "constraint_not_valid";

export function up() {
  const lineItems = table("line_items");
  // FK added NOT VALID — skip the add-time full-table scan.
  lineItems.addForeignKey("line_items_order_fkey", {
    columns: ["order_id"],
    references: { table: "orders", columns: ["id"] },
    notValid: true,
  });
  // CHECK added NOT VALID via the selector form.
  lineItems.check("line_items_qty_positive").add({
    expr: (c) => c("qty").gt(0),
    notValid: true,
  });
  // …then validate both later under a weaker lock.
  lineItems.validateConstraint("line_items_order_fkey");
  lineItems.validateConstraint("line_items_qty_positive");
}
