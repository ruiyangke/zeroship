// op.* migration fixture — the FULL FLUENT DML + EXPRESSION surface, authored via
// the SOLE public `table()` entry. Exercises the row-object `insert({ rows })`
// form, `update`/`delete`/`backfill` with the one `where` keyword, and the
// single-handle `(c) => Expr` builder (`c("name")` + the chainable operator methods
// + the `c.fn.*` namespace).
//
// Exercises EVERY Expr node + operator:
//   colRef, literal (auto-wrapped bare value), binOp {eq,ne,lt,le,gt,ge,and,or,
//   add,sub,mul,div,concat}, unaryOp {not,isNull,isNotNull,isTrue,isFalse},
//   case, fnCall {coalesce,nullif,lower,upper,trim,length,abs}, fnSynth
//   {concatWs,splitPart,now,genRandomUuid}, cast.
import { table } from "@zeroship/migrate";

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

    // update({ set, where }) — `set` values + `where` are `(c) => Expr`.
    sc.update({
      set: {
        label: (c) => c.fn.coalesce(c("label"), "unknown"),
        norm: (c) => c.fn.lower(c.fn.trim(c("label"))),
        shout: (c) => c.fn.upper(c("label")),
        len: (c) => c.fn.length(c("label")),
        mag: (c) => c.fn.abs(c("code").sub(500)),
        canon: (c) => c.fn.nullif(c("label"), ""),
        score: (c) => c("code").add(1).mul(2).sub(3).div(1),
        joined: (c) => c("label").concat(" ", c("code").cast({ to: "text" })),
        code_txt: (c) => c("code").cast({ to: "text" }),
      },
      where: (c) => c("code").gt(0).and(c("label").isNotNull()),
    });

    // delete({ where, limit }) — mandatory `where`; ne/le/ge/or/not + isNull/isFalse +
    // a searched CASE predicate.
    sc.delete({
      where: (c) =>
        c("code")
          .ne(0)
          .or(c("code").le(0))
          .or(c("code").ge(999))
          .or(c("label").isNull())
          .or(c("active").isFalse())
          .and(
            c
              .case({ branches: [{ when: c("code").lt(100), then: c("code").isNull() }], else: c("label").isNull() })
              .isTrue(),
          ),
      limit: 100,
    });

    // backfill({ set, where }) — `cursorColumn`/`batchSize` overridable; fnSynth
    // concatWs/splitPart/now/genRandomUuid.
    sc.backfill({
      set: {
        full: (c) => c.fn.concatWs(" ", c("label"), c("code").cast({ to: "text" })),
        first: (c) => c.fn.splitPart(c("label"), " ", 1),
        touched: (c) => c.fn.now(),
        token: (c) => c.fn.genRandomUuid(),
      },
      where: (c) => c("code").gt(0),
      cursorColumn: "code",
      batchSize: 500,
      name: "fluent_backfill",
    });
  },
};
