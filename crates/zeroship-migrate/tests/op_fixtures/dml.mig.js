// op.* migration fixture — insert + update + delete + backfill, carrying the
// closed expression-AST via the fluent `(c) => Expr` builder in `set`/`where`. The
// OP SHAPE is frozen in PR1; this fixture pins the DML wire shape + the in-AST
// typed-literal canonicalization for the corpus + the round-trip gate.
import { table } from "@zeroship/migrate";

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
    set: { label: (c) => c.fn.coalesce(c("label"), "unknown") },
    where: (c) => c("code").gt(0),
  });

  // DELETE FROM status_codes WHERE code is null  (mandatory where)
  sc.del({ where: (c) => c("code").isNull(), limit: 100 });

  // A resumable backfill paging over `code`, filtered, with a synth concatWs set.
  sc.backfill({
    set: { label: (c) => c.fn.concatWs(" ", c("code"), c("label")) },
    where: (c) => c("code").gt(0),
    cursorColumn: "code",
    batchSize: 500,
    name: "backfill_labels",
  });
}
