// op.* migration fixture — the typed-scalar AUTHORING ergonomics (§3.5): a
// branded `decimal()` value and a `Uint8Array` passed through the FLUENT insert / column default,
// authored via the SOLE public `table()` entry. The builder MUST normalize them
// into the closed `IrScalar` WIRE carriers before recording:
//   - a branded `decimal("...")` value → `{ decimal: "<v>" }`;
//   - a `Uint8Array` → `{ bytes: "<base64>" }` (the default `{"0":…}` array-index
//     spelling is HARD-REJECTED by the Rust `IrScalar` deserializer).
// This is the round-trip proof that spec-blessed decimal/bytes author values emit
// shapes Rust accepts value-equal.
import { table, t, decimal } from "zero-migrate";

export const name = "fluent_scalars";

export function up() {
  table("ledger").create({
    columns: {
      id: t.id(),
      // a large-int column default carried via the decimal() -> {decimal} carrier
      seq: t.numeric({ precision: 38, scale: 0 }).notNull().default(decimal("9007199254740993")),
      // a bytes column default carried via the Uint8Array -> {bytes} carrier
      salt: t.bytes().default(new Uint8Array([1, 2, 3, 255])),
    },
  });
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
