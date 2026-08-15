// op.* migration fixture — the typed-scalar AUTHORING ergonomics: a
// branded `decimal()` value and a `Uint8Array` passed through a column default,
// authored via the SOLE public `table()` entry. The builder MUST normalize them
// into the closed `IrScalar` WIRE carriers before recording:
//   - a branded `decimal("...")` value → `{ decimal: "<v>" }`;
//   - a `Uint8Array` → `{ bytes: "<base64>" }` (the default `{"0":…}` array-index
//     spelling is HARD-REJECTED by the Rust `IrScalar` deserializer).
// The companion `fluent_scalars_dml` fixture covers the same carriers through a
// fluent insert without mixing schema and data operations in one migration.
import { table, t, decimal } from "zero-migrate";

export const name = "fluent_scalars";

export function schema() {
  table("ledger").create({
    columns: {
      // a large-int column default carried via the decimal() -> {decimal} carrier
      seq: t.numeric({ precision: 38, scale: 0 }).notNull().default(decimal("9007199254740993")),
      // a bytes column default carried via the Uint8Array -> {bytes} carrier
      salt: t.bytes().default(new Uint8Array([1, 2, 3, 255])),
    },
  });
}
