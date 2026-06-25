// op.* migration fixture — the C2 follow-on: `.column(name).add({ type })` where
// the added column carries a `.unique()` and/or `.primaryKey()` modifier. An ADD
// COLUMN has no inline UNIQUE/PK, so each modifier LOWERS to a separate follow-on
// ADD CONSTRAINT op. This is the ONE PR15 behavior that ADDS ops to the IR, so it
// must be byte-pinned by the cross-impl golden (op_round_trip GATE 1 + the value
// checksum), not only the hand-written TS assertions.
//
// Cases:
//   - `.add({ type: t.text().unique() })`   ⇒ addColumn + addConstraint(unique)
//   - `.add({ type: t.uuid().primaryKey() })` ⇒ addColumn + addConstraint(pk)
//   - `.add({ type: t.text().unique().primaryKey() })` ⇒ addColumn + addConstraint(pk)
//     ONLY (the redundant UNIQUE is suppressed: a PK already implies uniqueness).
import { table, t } from "@zeroship/migrate";

export const name = "ddl_addcol_constraints";

export function up() {
  const accounts = table("accounts");

  // `.unique()` on an added column ⇒ a follow-on ADD CONSTRAINT(unique).
  accounts.column("email").add({ type: t.text().notNull().unique() });

  // `.primaryKey()` on an added column ⇒ a follow-on ADD CONSTRAINT(pk).
  accounts.column("id").add({ type: t.uuid().primaryKey() });

  // BOTH set ⇒ the redundant UNIQUE is suppressed; only the pk add is recorded.
  accounts.column("slug").add({ type: t.text().unique().primaryKey() });
}
