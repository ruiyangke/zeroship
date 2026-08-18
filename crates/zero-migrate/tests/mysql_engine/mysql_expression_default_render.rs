//! MySQL column defaults are parenthesised exactly where MySQL's grammar needs it.
//!
//! MySQL accepts a bare `DEFAULT` body only for a literal and for the
//! `CURRENT_TIMESTAMP` family; every other expression is a syntax error unless it
//! is wrapped. MEASURED against the live container (MySQL 8.4.11), by issuing the
//! statements directly rather than through the engine, because this crate has no
//! MySQL client in its dev-dependencies - MySQL live coverage lives in the CLI
//! host suite:
//!
//!     datetime DEFAULT UPPER('x')         ERROR    datetime DEFAULT ('x')       ok
//!     datetime DEFAULT 1+1                ERROR    datetime DEFAULT (1+1)       ok
//!     datetime DEFAULT now()              ok       datetime DEFAULT (now())     ok
//!     datetime DEFAULT CURRENT_TIMESTAMP  ok       datetime DEFAULT (UUID())    ok
//!
//! Wrapping is therefore ACCEPTED for every shape, including the two the engine
//! deliberately leaves bare. Those two are a stability choice rather than a
//! grammar one, and this file pins both halves so the choice cannot be reversed
//! by accident:
//!
//!   - `now()` renders `CURRENT_TIMESTAMP(6)`, which MySQL accepts bare. Wrapping
//!     it would rewrite the DDL every existing MySQL timestamp default emits, and
//!     buy nothing. It is worth being precise about what this is NOT: the stored
//!     catalog text does differ (MEASURED on 8.4.11, both spellings in one table -
//!     `CURRENT_TIMESTAMP(6)` bare, `now(6)` wrapped), but that difference is
//!     invisible to drift, because an ordinary default's raw text is emission
//!     metadata that `ColumnSnapshot`'s equality omits by design.
//!   - `uuidV4` renders its own parentheses at the leaf, so wrapping again would
//!     nest a second redundant pair.
//!
//! The rule is applied to the IR node, not to the rendered string, so it never
//! has to decide whether a rendered literal's spelling looks like a call.
//!
//! The end-to-end proof that the wrapped form actually applies is in the CLI host
//! suite (`mysql-authoring.test.ts`), which authors the same shape through the
//! public DSL and runs it against a real MySQL server. This file pins the bytes;
//! that test proves the server accepts them.

use crate::support;

use zero_migrate::render::lower::IrAuthor;
use zero_migrate::{
    ColType, Expr, IrDefault, IrFlagsOverride, IrScalar, LiveSchema, MigrationIr, Op, ScalarFn,
    SqlDialect, SynthFn, CURRENT_IR_VERSION,
};

const PROJECT: &str = "app";
const APP: &str = "app_test";

/// `lower('X')` - a function call, which per the measurements above MySQL refuses
/// unless it is wrapped.
fn lower_of_literal() -> IrDefault {
    IrDefault::Expr {
        expr: Expr::FnCall {
            r#fn: ScalarFn::Lower,
            args: vec![Expr::Literal {
                value: IrScalar::Str("X".to_string()),
            }],
        },
    }
}

fn add_column_ir(default: IrDefault, ty: ColType) -> MigrationIr {
    MigrationIr {
        inverse_ops: None,
        irreversible: None,
        ir_version: CURRENT_IR_VERSION,
        name: "add_label".to_string(),
        owner_app: APP.to_string(),
        ops: vec![Op::AddColumn {
            table: "accounts".to_string(),
            column: "label".to_string(),
            ty,
            nullable: Some(true),
            default: Some(default),
            value_format: None,
            vector_metric: None,
            case_sensitive: None,
            mask: None,
            generated: None,
            identity: None,
            schema: None,
            existence_guard: None,
        }],
        flags: IrFlagsOverride::default(),
        depends_on: Vec::new(),
        supersedes: Vec::new(),
        preconditions: Vec::new(),
        checksum: None,
    }
}

fn rendered_up(ir: &MigrationIr) -> String {
    let author = IrAuthor::new(
        PROJECT,
        APP,
        SqlDialect::Mysql,
        &support::confined_charter(),
    );
    let steps = author
        .lower_steps(ir, &LiveSchema::default())
        .expect("the engine accepts an expression default for MySQL rather than refusing it");
    steps
        .iter()
        .filter_map(|step| match step {
            zero_migrate::render::step::PlanStep::Ddl(m) => Some(m.up.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(";\n")
}

fn string_column(default: IrDefault) -> String {
    rendered_up(&add_column_ir(default, ColType::String { length: 64 }))
}

#[test]
fn a_mysql_function_call_default_is_wrapped_in_the_parentheses_mysql_requires() {
    let up = string_column(lower_of_literal());

    assert!(
        up.contains("DEFAULT (lower("),
        "a function-call default must reach MySQL parenthesised, because the bare \
         form is ERROR 1064: {up}"
    );
}

// The control that keeps the pin from being read too widely. A plain literal
// default is valid bare on MySQL, so it must NOT be wrapped - a blanket
// "parenthesise everything" change would be measurable here.
#[test]
fn a_literal_default_stays_bare_because_mysql_accepts_it_that_way() {
    let up = string_column(IrDefault::Literal {
        value: IrScalar::Str("plain".to_string()),
    });

    assert!(
        up.contains("DEFAULT '") || up.contains("DEFAULT \""),
        "a literal default is emitted as a bare literal, which MySQL accepts: {up}"
    );
    assert!(
        !up.contains("DEFAULT ('"),
        "a literal must not acquire parentheses from a fix aimed at expressions: {up}"
    );
}

// `now()` is an expression node, so the rule that wraps expressions would catch
// it. It is exempted on purpose, to leave working DDL alone - not because MySQL
// would refuse the wrapped form, and not to protect a drift comparison that does
// not read this field.
#[test]
fn the_current_timestamp_default_stays_bare_so_its_stored_text_does_not_change() {
    let up = rendered_up(&add_column_ir(
        IrDefault::Expr {
            expr: Expr::FnSynth {
                r#fn: SynthFn::Now,
                args: Vec::new(),
            },
        },
        ColType::Timestamp,
    ));

    assert!(
        up.contains("DEFAULT CURRENT_TIMESTAMP"),
        "the timestamp default keeps the spelling MySQL stores verbatim: {up}"
    );
    assert!(
        !up.contains("DEFAULT (CURRENT_TIMESTAMP"),
        "MySQL would accept the wrapped form; leaving it bare keeps every existing \
         timestamp default's DDL byte-identical, which is the only reason to: {up}"
    );
}

// `uuidV4` already emits its own parentheses at the leaf, so the clause-level
// rule must not add a second pair.
#[test]
fn a_uuid_v4_default_is_not_wrapped_twice() {
    let up = rendered_up(&add_column_ir(
        IrDefault::Expr { expr: Expr::UuidV4 },
        ColType::String { length: 64 },
    ));

    assert!(
        up.contains("DEFAULT (lower(concat("),
        "the leaf's own parentheses carry the default: {up}"
    );
    assert!(
        !up.contains("DEFAULT (("),
        "the clause-level rule must not nest a redundant second pair: {up}"
    );
}
