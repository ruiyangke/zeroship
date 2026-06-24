// op.* migration fixture — insert + update + delete + backfill, carrying the
// closed expression-AST (`e.*`) in `set`/`where`/`filter`. The OP SHAPE is frozen
// in PR1 (the executors land in PR6a/PR6b); this fixture pins the DML wire shape
// + the in-AST typed-literal canonicalization for the corpus + the round-trip gate.
import { insert, update, del, backfill, e } from "@zeroship/migrate";

export const name = "dml";

export function up() {
  insert("status_codes", ["code", "label"], [
    [200, "ok"],
    [404, "not found"],
  ]);

  // UPDATE status_codes SET label = coalesce(label, 'unknown') WHERE code > 0
  update("status_codes", { label: e.fnCall("coalesce", [e.col("label"), e.lit("unknown")]) }, {
    where: e.binOp("gt", e.col("code"), e.lit(0)),
  });

  // DELETE FROM status_codes WHERE code is null  (mandatory where)
  del("status_codes", e.unaryOp("isNull", e.col("code")), { limit: 100 });

  // A resumable backfill paging over `code`, filtered, with a synth concatWs set.
  backfill(
    "status_codes",
    "code",
    500,
    { label: e.fnSynth("concatWs", [e.lit(" "), e.col("code"), e.col("label")]) },
    "backfill_labels",
    { filter: e.binOp("gt", e.col("code"), e.lit(0)) },
  );
}
