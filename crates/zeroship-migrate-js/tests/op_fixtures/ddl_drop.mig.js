// op.* migration fixture — dropIndex + dropColumn + dropTable. Authored via the
// fluent table() surface.
//
// PR10 review (LOW, corpus coverage): this fixture is the corpus carrier for BOTH
// new PR10 fields PRESENT — an `existenceGuard` token (via `ifExists`) AND an
// explicit `schema` qualifier. It closes the gap between `full_surface` (proves the
// recorder EMITS schema/existenceGuard) and the JS↔Rust `Checksum::of_ir` corpus
// parity (proves both impls FOLD them identically): a JS impl that mis-folded a
// PRESENT schema/guard into of_ir is caught here, not only by the in-crate fold test.
import { table } from "@zeroship/migrate";

export const name = "ddl_drop";

export function up() {
  table("orders").index("orders_total_idx").drop({ unique: false });
  // `schema` PRESENT alongside the `ifExists` existence-guard token, so the corpus
  // of_ir parity asserts the JS recorder and the Rust loader fold BOTH new fields
  // identically.
  table("orders").column("memo").drop({ ifExists: true, schema: "reporting" });
  table("scratch").drop({ ifExists: true, cascade: true, schema: "reporting" });
}
