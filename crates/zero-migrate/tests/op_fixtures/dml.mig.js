// op.* migration fixture — insert + update + delete + backfill, carrying mixed
// scalar and closed expression-AST values in `set`, plus `(col) => Expr` `where`.
// The OP SHAPE is frozen; this fixture pins the DML wire shape + the
// in-AST typed-literal canonicalization for the corpus + the round-trip gate.
import { concatWs, table } from "zero-migrate";

export const name = "dml";

export function up() {
  const sc = table("status_codes");

  sc.insert({
    rows: [
      { code: 200, label: "ok" },
      { code: 404, label: "not found" },
    ],
  });

  // UPDATE status_codes SET label = coalesce(label, 'unknown') WHERE code > 0
  sc.update({
    set: { label: (col) => col("label").coalesce("unknown"), marker: "fixed" },
    where: (col) => col("code").gt(0),
  });

  // DELETE FROM status_codes WHERE code is null  (mandatory where)
  sc.delete({ where: (col) => col("code").isNull(), limit: 100 });

  // A resumable backfill paging over `code`, filtered, with a synth concatWs set.
  sc.backfill({
    set: { label: (col) => concatWs(" ", col("code"), col("label")), marker: "backfilled" },
    where: (col) => col("code").gt(0),
    cursorColumn: "code",
    batchSize: 500,
    name: "backfill_labels",
  });
}
