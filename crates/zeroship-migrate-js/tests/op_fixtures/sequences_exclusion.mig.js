// op.* migration fixture — standalone sequences and exclusion constraints.
// Covers the resettable sequence fields where explicit null carries SQL meaning
// (`OWNED BY NONE`) and the closed exclusion-operator set.
import { sequence, table } from "@zeroship/migrate";

export const name = "sequences_exclusion";

export function up() {
  sequence("invoice_seq").create({
    increment: 5,
    start: 100,
    cache: 10,
    cycle: true,
    ownedBy: { table: "invoices", column: "id" },
  });
  sequence("invoice_seq").alter({
    increment: 7,
    restart: 200,
    minValue: 1,
    maxValue: 999,
    cache: 20,
    cycle: false,
    ownedBy: null,
  });
  sequence("old_invoice_seq").drop({ ifExists: true });

  table("bookings").exclusion("bookings_no_overlap").add({
    using: "gist",
    elements: [
      { target: "room", operator: "=" },
      { target: "during", operator: "&&" },
    ],
    where: (c) => c("cancelled").eq(false),
    deferrable: true,
  });
}
