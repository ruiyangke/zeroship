// The MEASURED exceptions to `dialect_conformance_live.rs`'s layer-1 rule.
//
// `include!`d, not a module, so it stays a plain data file: two tables, each
// entry naming the other value rather than merely silencing a row. The judge in
// the including file fails on a stale entry in BOTH directions - an allowance
// whose row now agrees, and a pin whose row now executes - so neither table can
// rot into a permanent exemption list. `words` is checked with `contains`, so a
// changed diagnosis fails here rather than being absorbed.
//
// Every entry below was produced by running this suite against PostgreSQL 18.4 on
// 2026-08-18 and against in-process SQLite. Nothing here is a guess, and the
// `words` field is the verbatim text that run produced.
//
// The entries fall into three families, and the distinction is the point:
//
//   (A) DEGENERATE REPRESENTATIVE. The corpus op cannot be authored on ANY
//       dialect, because it was built to select a support branch rather than to
//       be applied. `insert` carries `rows: []`; `addConstraint/exclusion`
//       carries `elements: []`; `dropIndex` omits its owning table. These say
//       nothing about the declaration and everything about the corpus, and they
//       are the reason a live layer needs its own fixture review.
//   (B) DECLARATION ERROR. The sidecar says supported and the engine itself
//       refuses, cleanly, every time. These are real defects in
//       `dialect-support.toml`. They are RECORDED rather than fixed because the
//       fix is NOT the one-line sidecar change it looks like: `Op::support()`
//       reads the table, so flipping a cell to `unsupported` routes the operator
//       message through `op_support.rs::unsupported_reason`, which has no arm for
//       any of these (kind, variant) pairs and answers the literal string
//       "internal: supported cell has no refusal reason". Each fix is a sidecar
//       line PLUS a reason arm PLUS a regenerate of two artifacts.
//   (C) ENGINE DEFECT. The declaration is defensible and the engine still gets it
//       wrong at or after render. There are two, and one of them is a
//       `ServerError`.
//
// The full accounting is in `docs/review-log.md`.

/// Rows where the declaration and the server disagree.
const ALLOWANCES: &[Allowance] = &[
    // ---------------------------------------------------------------- postgres
    // (A) The representative omits its owning table, and the production validate
    // gate refuses a bare-name index drop fail-closed on EVERY dialect. Note the
    // CODE: an OWNERSHIP refusal is spelled `UNSUPPORTED`, the dialect table's own
    // code, so a bare-name dropIndex reads as "this dialect cannot drop an index".
    Allowance {
        kind: "dropIndex",
        variant: "base",
        dialect: "postgres",
        observed: Outcome::RefusedByCapability,
        words: "omits its owning table",
        why: "(A) representative carries table: None; refused identically on sqlite. \
               The refusal is correct, its CODE_UNSUPPORTED spelling is not.",
    },
    // (C) ENGINE DEFECT. `alterSequence` with no options renders `ALTER SEQUENCE
    // "s"` - a statement with no action - and the only thing that catches it is
    // the SQL guard's PARSER. Nothing in validate or lower objects. With the guard
    // absent this reaches the server.
    Allowance {
        kind: "alterSequence",
        variant: "base",
        dialect: "postgres",
        observed: Outcome::RefusedByPolicy,
        words: "syntax error at end of input",
        why: "(C) an option-less alterSequence renders unparseable SQL; the guard's \
               parser is the only gate that notices.",
    },
    // (A) A collapse-affirmed partitioned parent needs a default child in the SAME
    // recording. The representative carries no child, so it cannot apply anywhere.
    Allowance {
        kind: "createTable",
        variant: "partitionedCollapse",
        dialect: "postgres",
        observed: Outcome::EngineError,
        words: "has no default child",
        why: "(A) collapse affirmation is a whole-recording property the single-op \
               representative cannot satisfy.",
    },
    Allowance {
        kind: "addConstraint",
        variant: "exclusion",
        dialect: "postgres",
        observed: Outcome::RefusedByCapability,
        words: "exclusion constraint needs at least one element",
        why: "(A) the representative carries elements: [].",
    },
    Allowance {
        kind: "insert",
        variant: "base",
        dialect: "postgres",
        observed: Outcome::EngineError,
        words: "malformed insert into \"t\": no rows",
        why: "(A) the representative carries rows: [].",
    },
    Allowance {
        kind: "insert",
        variant: "onConflictDoUpdate",
        dialect: "postgres",
        observed: Outcome::EngineError,
        words: "malformed insert into \"t\": no rows",
        why: "(A) the representative carries rows: [].",
    },
    Allowance {
        kind: "insert",
        variant: "onConflictDoNothing",
        dialect: "postgres",
        observed: Outcome::EngineError,
        words: "malformed insert into \"t\": no rows",
        why: "(A) the representative carries rows: [].",
    },
    // (A) The representative is internally inconsistent: a TEXT domain carrying a
    // nextval default. The engine is right to refuse it.
    Allowance {
        kind: "createDomain",
        variant: "nextvalDefault",
        dialect: "postgres",
        observed: Outcome::EngineError,
        words: "nextval defaults require an integer column",
        why: "(A) the representative declares a nextval default on `as: text`.",
    },

    // ------------------------------------------------------------------ sqlite
    Allowance {
        kind: "dropIndex",
        variant: "base",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "omits its owning table",
        why: "(A) same representative defect as the postgres row above.",
    },
    // (B) DECLARATION ERRORS. Five rows declare `portable` on SQLite while the
    // lowerer refuses them with `IrLowerError::SqliteRebuildOnly`: the imperative
    // op lane has no SQLite render for these, only the declarative differ's
    // 12-step rebuild does. `render/lower.rs`'s own comment on
    // `require_alter_column_rendering` records the PRECEDENT - F674 corrected
    // exactly this class for `setColumnNotNull`, `dropColumnNotNull` and
    // `dropColumnDefault`, and stopped there. These five are the rest of it.
    Allowance {
        kind: "dropConstraint",
        variant: "base",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "needs the 12-step table rebuild",
        why: "(B) sidecar says portable; lower refuses SqliteRebuildOnly. F674 class.",
    },
    Allowance {
        kind: "setColumnType",
        variant: "base",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "needs the 12-step table rebuild",
        why: "(B) sidecar says portable; lower refuses SqliteRebuildOnly. F674 class.",
    },
    Allowance {
        kind: "setColumnDefault",
        variant: "base",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "needs the 12-step table rebuild",
        why: "(B) sidecar says portable; lower refuses SqliteRebuildOnly. F674 class.",
    },
    Allowance {
        kind: "setColumnDefault",
        variant: "containerOrJson",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "needs the 12-step table rebuild",
        why: "(B) sidecar says portable; lower refuses SqliteRebuildOnly. F674 class.",
    },
    Allowance {
        kind: "addConstraint",
        variant: "unique",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "needs the 12-step table rebuild",
        why: "(B) sidecar says portable; lower refuses SqliteRebuildOnly. The FK \
               variants of this same op DO apply, so this is a per-variant gap.",
    },
    // (B) DECLARATION ERROR, and this one has its correction already written down
    // in the adjacent table. `support.rs`'s `Feature::TriggerMultipleEvents` cell
    // declares SQLite UNSUPPORTED, with a comment recording that SQLite "used to be
    // declared supported beside a message that already spelled out why it could
    // not be". The op-level sidecar still says `portable`. Both SQLite's and
    // MySQL's grammars take exactly one trigger event; only PostgreSQL accepts
    // `INSERT OR UPDATE`.
    Allowance {
        kind: "createTrigger",
        variant: "bodyMultipleEvents",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "SQLite CREATE TRIGGER accepts exactly one trigger event",
        why: "(B) the FEATURE table already says unsupported; the OP sidecar still \
               says portable. The two disagree and the feature gate wins.",
    },
    // (C) ENGINE DEFECT, and the sharpest one here: a `portable` declaration whose
    // op clears validate, clears lower, and DIES AGAINST THE DATABASE. SQLite
    // accepts INSTEAD OF triggers on VIEWS only, and nothing in the engine checks
    // the target's kind. The declaration is defensible - the op IS supported, on a
    // view - so the missing gate is the defect, not the cell.
    Allowance {
        kind: "createTrigger",
        variant: "bodyInsteadOf",
        dialect: "sqlite",
        observed: Outcome::ServerError,
        words: "cannot create INSTEAD OF trigger on table",
        why: "(C) no gate checks that an INSTEAD OF trigger's target is a view; the \
               refusal arrives from SQLite, mid-apply.",
    },
    // (A) Partition collapse is a whole-recording property, so a single-op
    // representative can never reach the degraded leg. Three rows, one cause.
    Allowance {
        kind: "createPartition",
        variant: "base",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "collapse-affirmed partitioned parent",
        why: "(A) the affirmation must be in the SAME recording as the op; a prelude \
               in a prior recording cannot supply it.",
    },
    Allowance {
        kind: "dropPartition",
        variant: "base",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "dropPartition needs a collapse-affirmed parent",
        why: "(A) same whole-recording property. Note this row declares `portable`, \
               not `transparentDegradable`, unlike its two siblings.",
    },
    Allowance {
        kind: "createTable",
        variant: "partitionedCollapse",
        dialect: "sqlite",
        observed: Outcome::EngineError,
        words: "has no default child",
        why: "(A) same as the postgres row.",
    },
    Allowance {
        kind: "insert",
        variant: "base",
        dialect: "sqlite",
        observed: Outcome::EngineError,
        words: "malformed insert into \"t\": no rows",
        why: "(A) the representative carries rows: [].",
    },
    Allowance {
        kind: "insert",
        variant: "onConflictDoUpdate",
        dialect: "sqlite",
        observed: Outcome::EngineError,
        words: "malformed insert into \"t\": no rows",
        why: "(A) the representative carries rows: [].",
    },
    Allowance {
        kind: "insert",
        variant: "onConflictDoNothing",
        dialect: "sqlite",
        observed: Outcome::EngineError,
        words: "malformed insert into \"t\": no rows",
        why: "(A) the representative carries rows: [].",
    },
];

/// Rows that hand the operator `op_support.rs`'s internal placeholder instead of a
/// reason. MEASURED, all four on SQLite, all four from the SAME earlier fix.
///
/// `render/lower.rs` records F674 as having moved `setColumnNotNull`,
/// `dropColumnNotNull` and `dropColumnDefault` to `unsupported` on SQLite because
/// the gate refused what the cells declared. That correction was right, and it
/// stopped at the sidecar: `op_support.rs::unsupported_reason` has no arm for any
/// of them, so `Op::support()` returns the sentinel and the operator is told
/// "internal: supported cell has no refusal reason" for an ordinary,
/// well-understood SQLite limitation. `validateConstraint` is the fourth, from the
/// same shape.
///
/// This is the concrete cost of the fix this file DECLINED to make for the six
/// (B)-family rows above: flipping a cell without adding the arm ships an internal
/// string as the user-facing diagnosis. Recorded here rather than repaired because
/// repairing it is engine work, not conformance work.
const PLACEHOLDER_REASONS: &[PlaceholderReason] = &[
    PlaceholderReason {
        kind: "setColumnNotNull",
        variant: "base",
        dialect: "sqlite",
        why: "F674 flipped the cell and left unsupported_reason without an arm",
    },
    PlaceholderReason {
        kind: "dropColumnNotNull",
        variant: "base",
        dialect: "sqlite",
        why: "F674 flipped the cell and left unsupported_reason without an arm",
    },
    PlaceholderReason {
        kind: "dropColumnDefault",
        variant: "base",
        dialect: "sqlite",
        why: "F674 flipped the cell and left unsupported_reason without an arm",
    },
    PlaceholderReason {
        kind: "validateConstraint",
        variant: "base",
        dialect: "sqlite",
        why: "same shape: an unsupported cell with no unsupported_reason arm",
    },
];

/// Rows whose representative could not be made executable, with the reason.
///
/// Empty, and that is a measurement rather than an omission: every one of the 92
/// rows reached the subject op on both dialects. The rows whose representative is
/// degenerate still got an ANSWER; they are in `ALLOWANCES` above, not here.
const NOT_EXECUTABLE: &[NotExecutableRow] = &[];
