// Companion to `fluent_scalars`: typed scalar authoring through a fluent insert,
// kept in a data migration so the corpus never mixes DDL and DML in one envelope.
// The recorder normalizes decimal() and Uint8Array author values into the closed
// IrScalar wire carriers that Rust accepts value-equal.
import { table, decimal } from "zero-migrate";

export const name = "fluent_scalars_dml";
export const irreversible =
  "this recorder corpus fixture is never applied to a database and does not define a database rollback";

export function data() {
  table("ledger").insert({
    rows: [
      {
        // 2^53 + 1 - beyond the JS safe-integer range; decimal() keeps it exact
        seq: decimal("9007199254740993"),
        // raw bytes through the {bytes:base64} carrier
        salt: new Uint8Array([0, 16, 32, 64, 128, 255]),
      },
    ],
  });
}
