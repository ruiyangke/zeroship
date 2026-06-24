// op.* migration fixture — the typed-scalar AUTHORING ergonomics (§2.5/§3.2): a JS
// `bigint` and a `Uint8Array` passed through the FLUENT insert / column default.
// The builder MUST normalize them into the closed `IrScalar` WIRE carriers before
// recording:
//   - a `bigint` → `{ decimal: "<v>" }` (a bare bigint THROWS at JSON.stringify; the
//     `{decimal}` carrier is the integers-beyond-2^53 representation);
//   - a `Uint8Array` → `{ bytes: "<base64>" }` (the default `{"0":…}` array-index
//     spelling is HARD-REJECTED by the Rust `IrScalar` deserializer).
// This is the round-trip proof that a spec-blessed bigint/bytes author value emits a
// shape Rust accepts value-equal — the previously promised-but-broken path.
import { createTable, insert, t } from "@zeroship/migrate";

export const name = "fluent_scalars";

export function up() {
  createTable("ledger", {
    id: t.id(),
    // a large-int column default carried via the bigint -> {decimal} carrier
    seq: t.numeric(38, 0).notNull().default(9007199254740993n),
    // a bytes column default carried via the Uint8Array -> {bytes} carrier
    salt: t.bytes().default(new Uint8Array([1, 2, 3, 255])),
  });
  insert("ledger", {
    rows: [
      {
        // 2^53 + 1 — beyond the JS safe-integer range; the bigint carrier keeps it exact
        seq: 9007199254740993n,
        // raw bytes through the {bytes:base64} carrier
        salt: new Uint8Array([0, 16, 32, 64, 128, 255]),
      },
    ],
  });
}
