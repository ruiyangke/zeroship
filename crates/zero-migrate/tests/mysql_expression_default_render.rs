//! A MySQL expression column default is rendered without the parentheses MySQL needs.
//!
//! The authoring IR carries `IrDefault::Expr`, a closed expression default. The
//! validator admits it for every dialect - its `target_dialect` parameter is threaded
//! through the whole walk and used ONLY to build error messages, never to decide - and
//! the renderer sends it straight to `render_expr_inline` with no wrapping. So a
//! function-call default targeting MySQL is emitted bare.
//!
//! MEASURED separately against the live container (MySQL 8.4.11), by issuing the
//! statements directly rather than through the engine, because this crate has no
//! MySQL client in its dev-dependencies - MySQL live coverage lives in the CLI host
//! suite:
//!
//!     datetime DEFAULT UPPER('x')         ERROR    datetime DEFAULT ('x')       ok
//!     datetime DEFAULT 1+1                ERROR    datetime DEFAULT (1+1)       ok
//!     datetime DEFAULT now()              ok       datetime DEFAULT (now())     ok
//!     datetime DEFAULT CURRENT_TIMESTAMP  ok       datetime DEFAULT (UUID())    ok
//!
//! And the statements this test pins, issued verbatim against that server. They
//! differ in ONE variable - the default - so the rejection is attributable to it:
//!
//!     ADD COLUMN `label` VARCHAR(64) ... DEFAULT lower(_utf8mb4 X'58')
//!       ERROR 1064 (42000): You have an error in your SQL syntax; ... near
//!       'lower(_utf8mb4 X'58')' at line 1
//!     ADD COLUMN `label` VARCHAR(64) ... DEFAULT 'plain'
//!       accepted
//!
//! So bare is accepted only for literals and the datetime special case; every other
//! expression must be parenthesised. The two halves of this defect were established
//! by different means - this test pins what the ENGINE emits, and the table above
//! records what the SERVER does with that shape. Neither half is inferred.
//!
//! What makes this a defect rather than a test artifact: the VALIDATOR ADMITS this
//! shape. MEASURED BY ME by calling `validate_ir` directly on the same op -
//! `Dialect::Mysql` returns `Ok`, as does `Dialect::Postgres`. So nothing upstream
//! refuses it and the renderer's output is what a caller gets.
//!
//! The contrast that proves the validator is not simply absent here: the same probe
//! on a TEXT column carrying a plain literal default returns, for MySQL only,
//! "column \"label\" declares the literal default 'plain' but renders as MySQL TEXT
//! storage; MySQL refuses a literal DEFAULT on TEXT, BLOB, JSON, and GEOMETRY
//! columns". So the engine DOES gate MySQL default legality - it just does not gate
//! this shape.
//!
//! This is a pin on a known defect, not an endorsement. It is written to fail once
//! the renderer learns to wrap, which is the fix it is waiting for.
//!
//! Note the fix is NOT "parenthesise every default": `now()` and `CURRENT_TIMESTAMP`
//! are accepted bare, and a literal should stay bare. It is a question of which
//! expression shapes need wrapping.

mod support;

use zero_migrate::render::lower::IrAuthor;
use zero_migrate::{
    ColType, Expr, IrDefault, IrFlagsOverride, IrScalar, LiveSchema, MigrationIr, Op, ScalarFn,
    SqlDialect, CURRENT_IR_VERSION,
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

fn add_column_ir(default: IrDefault) -> MigrationIr {
    MigrationIr {
        ir_version: CURRENT_IR_VERSION,
        name: "add_label".to_string(),
        owner_app: APP.to_string(),
        ops: vec![Op::AddColumn {
            table: "accounts".to_string(),
            column: "label".to_string(),
            ty: ColType::String { length: 64 },
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

#[test]
fn a_mysql_function_call_default_is_emitted_without_the_parentheses_mysql_requires() {
    let up = rendered_up(&add_column_ir(lower_of_literal()));

    assert!(
        up.contains("DEFAULT"),
        "the expression default reaches a DEFAULT clause: {up}"
    );
    assert!(up.contains("lower("), "and renders the function call: {up}");
    assert!(
        !up.contains("DEFAULT (lower("),
        "today the call is emitted BARE, which MySQL rejects. When this assertion \
         starts failing the renderer has learned to wrap, which is the fix this pin \
         is waiting for: {up}"
    );
}

// The control that keeps the pin from being read too widely. A plain literal default
// is valid bare on MySQL, so it must NOT be wrapped by any future fix - a blanket
// "parenthesise everything" change would be measurable here.
#[test]
fn a_literal_default_stays_bare_because_mysql_accepts_it_that_way() {
    let up = rendered_up(&add_column_ir(IrDefault::Literal {
        value: IrScalar::Str("plain".to_string()),
    }));

    assert!(
        up.contains("DEFAULT '") || up.contains("DEFAULT \""),
        "a literal default is emitted as a bare literal, which MySQL accepts: {up}"
    );
    assert!(
        !up.contains("DEFAULT ('"),
        "a literal must not acquire parentheses from a fix aimed at expressions: {up}"
    );
}
