// op.* migration fixture — the FULL §3.2/§3.3.1 FLUENT DML + EXPRESSION surface
// (PR3). Authored in the headline form: `verb(table, { … })` DML with the one
// `where` keyword across update/del/backfill, the row-object `insert({ rows })`
// form, and the single-handle `(c) => Expr` builder (`c("name")` + the chainable
// operator methods + the `c.fn.*` namespace). Proves the fluent builder records
// the IDENTICAL frozen closed-AST wire nodes as the legacy `e.*` style.
//
// Exercises EVERY Expr node + operator:
//   colRef, literal (auto-wrapped bare value), binOp {eq,ne,lt,le,gt,ge,and,or,
//   add,sub,mul,div,concat}, unaryOp {not,isNull,isNotNull,isTrue,isFalse},
//   case, fnCall {coalesce,nullif,lower,upper,trim,length,abs}, fnSynth
//   {concatWs,splitPart,now,genRandomUuid}, cast.
import { insert, update, del, backfill } from "@zeroship/migrate";

export default {
  name: "fluent_dml",

  up() {
    // insert({ rows }) — the row-OBJECT form (normalized to columns + positional
    // rows, column order from the first row's keys).
    insert("status_codes", {
      rows: [
        { code: 200, label: "ok" },
        { code: 404, label: "not found" },
      ],
    });

    // update({ set, where }) — `set` values + `where` are `(c) => Expr`.
    // Exercises: fnCall(coalesce/lower/upper/trim/length/abs/nullif), binOp
    // arithmetic + comparison + boolean, unaryOp, cast, concat.
    update("status_codes", {
      set: {
        label: (c) => c.fn.coalesce(c("label"), "unknown"),
        norm: (c) => c.fn.lower(c.fn.trim(c("label"))),
        shout: (c) => c.fn.upper(c("label")),
        len: (c) => c.fn.length(c("label")),
        mag: (c) => c.fn.abs(c("code").sub(500)),
        canon: (c) => c.fn.nullif(c("label"), ""),
        score: (c) => c("code").add(1).mul(2).sub(3).div(1),
        joined: (c) => c("label").concat(" ", c("code").cast("text")),
        code_txt: (c) => c("code").cast("text"),
      },
      where: (c) => c("code").gt(0).and(c("label").isNotNull()),
    });

    // del({ where, limit }) — mandatory `where`; exercises ne/le/ge/or/not + the
    // isNull/isFalse unary tests + a searched CASE predicate.
    del("status_codes", {
      where: (c) =>
        c("code")
          .ne(0)
          .or(c("code").le(0))
          .or(c("code").ge(999))
          .or(c("label").isNull())
          .or(c("active").isFalse())
          .and(
            c.fn
              .case([[c("code").lt(100), c("code").isNull()]], c("label").isNull())
              .isTrue(),
          ),
      limit: 100,
    });

    // backfill({ set, where }) — `cursorColumn`/`batchSize` overridable; the
    // predicate keyword is `where`. Exercises fnSynth concatWs/splitPart/now/
    // genRandomUuid (the DB-evaluated apply-time scalars, §4.3).
    backfill("status_codes", {
      set: {
        full: (c) => c.fn.concatWs(" ", c("label"), c("code").cast("text")),
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
