// `insert { onConflict }` fixture authored through the fluent table() surface.
// It pins the closed `Op::Insert.onConflict` wire shape shared by PostgreSQL,
// SQLite, and MySQL.
import { table } from "zero-migrate";

export const name = "dml_upsert";
export const irreversible =
  "this recorder corpus fixture is never applied to a database and does not define a database rollback";

export function data() {
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
