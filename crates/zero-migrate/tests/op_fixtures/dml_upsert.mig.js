// op.* migration fixture — `insert { onConflict }` (the PG-only upsert facet,
// §PR6a / §9). Authored via the fluent table() surface. This pins the `onConflict`
// wire shape on `Op::Insert` and is the corpus member behind the op-level PG-only
// portability boundary: the IDENTICAL `.ir.json` LOADS on BOTH dialects (the load
// gate / structural `validate_op` does NOT inspect `Op::Insert.onConflict`), then
// renders on target_dialect=Postgres but is a HARD reject at LOWER on
// target_dialect=Sqlite. Exercised end-to-end in `crates/zero-migrate/tests/
// ir_dml_*` (PG render + SQLite lower reject).
import { table } from "zero-migrate";

export const name = "dml_upsert";

export function up() {
  const sc = table("status_codes");

  // INSERT … ON CONFLICT (code) DO UPDATE SET label = 'dup'
  sc.insert({
    rows: [{ code: 200, label: "ok" }],
    onConflict: { columns: ["code"], doUpdate: { label: "dup" } },
  });

  // A second insert with ON CONFLICT … DO NOTHING (absent doUpdate).
  sc.insert({
    rows: [{ code: 404 }],
    onConflict: { columns: ["code"] },
  });
}
