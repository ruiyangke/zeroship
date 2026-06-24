// op.* migration fixture — `insert { onConflict }` (the PG-only upsert facet,
// §PR6a / §9). This pins the `onConflict` wire shape on `Op::Insert` and is the
// corpus member behind the op-level `dialect_scope = PgOnly` fixture: the IDENTICAL
// `.ir.json` loads with target_dialect=Postgres and is REJECTED at load with
// target_dialect=Sqlite (the op-level peer of PR1's expression-level out-of-envelope
// splitPart PgOnly fixture). The executor + the load-gate behaviour are exercised in
// `crates/zeroship-migrate/tests/ir_dml_*` (PG render + SQLite reject).
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
