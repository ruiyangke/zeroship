// op.* migration fixture — `insert { onConflict }` (the PG-only upsert facet,
// §PR6a / §9). This pins the `onConflict` wire shape on `Op::Insert` and is the
// corpus member behind the op-level PG-only portability boundary: the IDENTICAL
// `.ir.json` LOADS on BOTH dialects (the load gate / structural `validate_op` does
// NOT inspect `Op::Insert.onConflict` — Insert walks to Ok), then renders on
// target_dialect=Postgres but is a HARD reject at LOWER on target_dialect=Sqlite —
// `assemble_insert` returns `DmlError::OnConflictNotPortable`, surfaced as
// `IrLowerError::DmlAssemble`. It is NOT a load-gate reject. The render + the
// lower-time reject are exercised end-to-end in
// `crates/zeroship-migrate/tests/ir_dml_*` (PG render + SQLite lower reject; see
// `on_conflict_rejected_on_sqlite`, which `.expect()`s the load gate then
// `.expect_err()`s at lower).
import { insert } from "@zeroship/migrate";

export const name = "dml_upsert";

export function up() {
  // INSERT … ON CONFLICT (code) DO UPDATE SET label = 'dup'
  insert("status_codes", ["code", "label"], [[200, "ok"]], {
    onConflict: { columns: ["code"], doUpdate: { label: "dup" } },
  });

  // A second insert with ON CONFLICT … DO NOTHING (absent doUpdate).
  insert("status_codes", ["code"], [[404]], {
    onConflict: { columns: ["code"] },
  });
}
