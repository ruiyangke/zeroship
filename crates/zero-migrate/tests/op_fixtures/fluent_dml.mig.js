// op.* migration fixture — the FULL FLUENT DML + EXPRESSION surface, authored via
// the SOLE public `table()` entry. Exercises the row-object `insert({ rows })`
// form, `update`/`delete`/`backfill` with the one `where` keyword, and the
// single-handle `(col) => Expr` builder (`col("name")` + the chainable operator methods
// + the scalar chain methods).
//
// Exercises EVERY Expr node + operator:
//   colRef, literal (auto-wrapped bare value), binOp {eq,ne,lt,le,gt,ge,and,or,
//   add,sub,mul,div,concat}, unaryOp {not,isNull,isNotNull,isTrue,isFalse},
//   case, fnCall {coalesce,nullif,lower,upper,trim,length,abs}, fnSynth
//   {concatWs,splitPart,now,genRandomUuid}, cast.
import { table, now, genRandomUuid, concatWs } from "zero-migrate";

export default {
  name: "fluent_dml",

  up() {
    const sc = table("status_codes");

    // insert({ rows }) — the row-OBJECT form (normalized to columns + positional
    // rows, column order from the first row's keys).
    sc.insert({
      rows: [
        { code: 200, label: "ok" },
        { code: 404, label: "not found" },
      ],
    });

    // update({ set, where }) — `set` values + `where` are `(col) => Expr`.
    sc.update({
      set: {
        label: (col) => col("label").coalesce("unknown"),
        norm: (col) => col("label").trim().lower(),
        shout: (col) => col("label").upper(),
        len: (col) => col("label").length(),
        mag: (col) => col("code").sub(500).abs(),
        canon: (col) => col("label").nullif(""),
        score: (col) => col("code").add(1).mul(2).sub(3).div(1),
        joined: (col) => col("label").concat(" ", col("code").cast({ to: "text" })),
        code_txt: (col) => col("code").cast({ to: "text" }),
      },
      where: (col) => col("code").gt(0).and(col("label").isNotNull()),
    });

    // delete({ where, limit }) — mandatory `where`; ne/le/ge/or/not + isNull/isFalse +
    // a searched CASE predicate.
    sc.delete({
      where: (col) =>
        col("code")
          .ne(0)
          .or(col("code").le(0))
          .or(col("code").ge(999))
          .or(col("label").isNull())
          .or(col("active").isFalse())
          .and(
            col
              .case({ branches: [{ when: col("code").lt(100), then: col("code").isNull() }], else: col("label").isNull() })
              .isTrue(),
          ),
      limit: 100,
    });

    // backfill({ set, where }) — `cursorColumn`/`batchSize` overridable; fnSynth
    // concatWs/splitPart/now/genRandomUuid.
    sc.backfill({
      set: {
        full: (col) => concatWs(" ", col("label"), col("code").cast({ to: "text" })),
        first: (col) => col("label").splitPart(" ", 1),
        touched: now(),
        token: genRandomUuid(),
      },
      where: (col) => col("code").gt(0),
      cursorColumn: "code",
      batchSize: 500,
      name: "fluent_backfill",
    });
  },
};
