// op.* migration fixture — table().rename({ to }). The corpus carrier for the
// `renameTable` Op variant, so the
// JS↔Rust `Checksum::of_ir` parity + the variant-exhaustiveness gate cover it.
//
// A whole-table rename is a FAST `ALTER TABLE … RENAME TO …` (NOT the online column
// expand-contract); `ifExists` guards the SOURCE table and `schema` qualifies it, so
// this fixture also asserts the JS recorder and the Rust loader fold BOTH fields
// on a rename identically (a bare rename and a schema+guard rename are both carried).
import { table } from "zero-migrate";

export const name = "ddl_rename_table";

export function up() {
  // Bare rename (no schema / no guard) — the minimal shape.
  table("accounts").rename({ to: "members" });
  // `schema` PRESENT alongside the `ifExists` existence-guard token.
  table("orders").rename({ to: "purchases", ifExists: true, schema: "reporting" });
}
