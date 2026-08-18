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
//       refuses, cleanly, every time. Six were recorded here; FIVE HAVE SINCE
//       BEEN FIXED and their allowances removed, each as a sidecar line PLUS an
//       `op_support.rs::unsupported_reason` arm PLUS a regenerate of both
//       generated artifacts - the arm is not optional, because `Op::support()`
//       reads the table and a flip without it hands the operator the literal
//       string "internal: supported cell has no refusal reason".
//       The sixth was REFUTED and moved to (A): see `dropConstraint/base` below.
//       Before flipping a cell, check whether the gate that refuses it is
//       unconditional or conditional on the payload. A conditional gate means the
//       representative measured one SHAPE, not the op.
//   (C) ENGINE DEFECT. The declaration is defensible and the engine still gets it
//       wrong at or after render. There were two, `alterSequence/base` on
//       PostgreSQL and `createTrigger/bodyInsteadOf` on SQLite, and BOTH ARE GONE:
//       the engine now refuses an option-less `alterSequence` and an `INSTEAD OF`
//       trigger aimed at a table, and both representatives - which were degenerate
//       in the (A) sense, since neither shape could apply on any dialect - were
//       corrected, so both rows APPLY. There is no (C) entry left in this file and
//       there should never be one for long: a (C) entry is a bug with a note on it.
//       The refusals themselves are pinned by
//       `tests/alter_sequence_needs_an_action.rs` and
//       `tests/instead_of_trigger_needs_a_view.rs`, each with its over-refusal
//       control.
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
    // (B) DECLARATION ERRORS - FIVE OF THE SIX ARE NOW FIXED, so their allowances
    // are GONE rather than re-worded. `setColumnType/base`, `setColumnDefault/base`,
    // `setColumnDefault/containerOrJson`, `addConstraint/unique` and
    // `createTrigger/bodyMultipleEvents` now declare `unsupported` on SQLite, which
    // is what the server said, so they no longer disagree and a lingering allowance
    // here would fail as STALE. Each flip came with the matching
    // `op_support.rs::unsupported_reason` arm, so none of them ships the internal
    // placeholder - see PLACEHOLDER_REASONS below, now empty.
    //
    // (A) THE SIXTH WAS REFUTED, and it is the interesting one. This row was
    // recorded as a (B) declaration error on the same evidence as the other five: it
    // refuses with `SqliteRebuildOnly`. But SQLite HAS a dropConstraint lane. When
    // the live snapshot carries the table and the named constraint is a FOREIGN KEY,
    // `render/lower.rs` lowers the op to a 12-step rebuild and it APPLIES. The
    // refusal is reached only for a missing snapshot or a NON-FK constraint, and the
    // representative below drops `c`, which is not a foreign key.
    //
    // So the observation was right and the classification was not: this is a
    // DEGENERATE REPRESENTATIVE, not a wrong declaration. Flipping the cell was
    // tried and REVERTED - `tests/sqlite_declaration_flip_over_refusal_control.rs`
    // drives a foreign-key drop through validate + lower + apply, and the flip made
    // it fail with `UNSUPPORTED`, refusing a migration that works today. There is
    // one row for all constraint kinds, so `portable` is the only disposition that
    // does not break the working case.
    //
    // The general lesson, which is why this comment is long: a refusal measured from
    // ONE representative bounds what that SHAPE does, not what the OP does. Five of
    // these six generalized safely because their gate is unconditional; this one did
    // not, because its gate is conditional on the constraint kind.
    Allowance {
        kind: "dropConstraint",
        variant: "base",
        dialect: "sqlite",
        observed: Outcome::RefusedByCapability,
        words: "needs the 12-step table rebuild",
        why: "(A) the representative drops a NON-FK constraint, the one shape SQLite's \
               rebuild lane does not cover. FK drops apply end to end; flipping this \
               cell was measured to be an over-refusal and was reverted.",
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
/// reason.
///
/// EMPTY, and that is a repair rather than a relaxation. Four rows were pinned
/// here - `setColumnNotNull/base`, `dropColumnNotNull/base`,
/// `dropColumnDefault/base` and `validateConstraint/base` - because
/// `op_support.rs::unsupported_reason` had no arm for them and so answered
/// "internal: supported cell has no refusal reason". Three of the four were
/// F674's own: it moved the cells to `unsupported` and did not write the arms.
/// The arms are now written and every one of these rows names a real limit.
///
/// The sweep above stays, and it is what keeps this list empty: it fails on any
/// row that starts showing the sentinel, and on any pin left here once its row
/// stops. Do not re-populate this list to make a red run green - a sentinel in
/// the output means a cell was declared `unsupported` without its reason arm.
///
/// Note this list could only ever have held SEVEN of the affected cells' four
/// rows partially: this suite covers PostgreSQL and SQLite, and three of the four
/// rows shipped the placeholder on MYSQL too, which no live suite here reaches.
/// The offline sweep in `tests/unsupported_reason_is_operator_facing.rs` covers
/// all three dialects and is the primary guard for that reason.
const PLACEHOLDER_REASONS: &[PlaceholderReason] = &[];

/// Rows whose representative could not be made executable, with the reason.
///
/// Empty, and that is a measurement rather than an omission: every one of the 92
/// rows reached the subject op on both dialects. The rows whose representative is
/// degenerate still got an ANSWER; they are in `ALLOWANCES` above, not here.
const NOT_EXECUTABLE: &[NotExecutableRow] = &[];
