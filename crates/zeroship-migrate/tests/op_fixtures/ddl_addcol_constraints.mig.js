// op.* migration fixture — the C2 follow-on: `.column(name).add({ type })` where
// the added column carries a `.unique()` modifier. An ADD COLUMN has no inline
// UNIQUE, so the modifier LOWERS to a separate follow-on ADD CONSTRAINT op. This
// is the ONE PR15 behavior that ADDS ops to the IR, so it must be byte-pinned by
// the cross-impl golden (op_round_trip GATE 1 + the value checksum), not only the
// hand-written TS assertions.
//
// (Primary key is CREATE-TIME ONLY — `create({ primaryKey })` / the `.primaryKey()`
// facet on a create() column — so there is no add-column PK follow-on: the
// always-refused user PRIMARY KEY constraint shape has been deleted from the IR.)
//
// Cases:
//   - `.add({ type: t.text().unique() })`   ⇒ addColumn + addConstraint(unique)
import { table, t } from "@zeroship/migrate";

export const name = "ddl_addcol_constraints";

export function up() {
  const accounts = table("accounts");

  // `.unique()` on an added column ⇒ a follow-on ADD CONSTRAINT(unique).
  accounts.column("email").add({ type: t.text().notNull().unique() });
}
