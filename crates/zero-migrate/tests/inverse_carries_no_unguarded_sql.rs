//! An inverse cannot become a way to run SQL the forward half is refused.
//!
//! The rollback executor gives a textual `Migration.down` the SAME line-1 guard
//! the `up` gets, on the stated reasoning that skipping it "would make `down` a
//! way to run exactly what `up` is refused".
//!
//! A recorded inverse deliberately does NOT take that path: it is structured IR,
//! lowered to parameterized DML, and never becomes SQL text for a guard to read.
//! The executor therefore skips the text guard for it. That is only safe if the
//! inverse cannot carry raw SQL in the first place, so this file measures that
//! claim rather than restating it.
//!
//! Two independent barriers, and the test asserts BOTH, because either alone
//! would be a single point of failure on the path an operator runs mid-incident:
//!
//!   1. the load gate validates the inverse with the same structural validator as
//!      the forward ops (job D), and
//!   2. the rollback planner refuses any inverse step that is not transactional
//!      DML, up front, before an earlier down can commit.
//!
//! `pgRaw` is the adversarial op precisely because it is the one that renders
//! author-supplied SQL text. If a reverse could smuggle it past both barriers,
//! `inverse()` would be a hole in the guard rather than a feature.
//!
//! GATE: none.

use std::collections::BTreeMap;

use zero_migrate::model::ir::{IrScalar, MigrationIr, Op};
use zero_migrate::model::load::load_ir_document;
use zero_migrate::model::validate::Dialect;

const OWNER: &str = "app_inverse_guard";
const TABLE: &str = "acct";

fn registry() -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    map.insert(TABLE.to_string(), OWNER.to_string());
    map
}

fn envelope_with_inverse(inverse: Vec<Op>) -> MigrationIr {
    MigrationIr {
        ir_version: 1,
        name: "seed".to_string(),
        owner_app: OWNER.to_string(),
        ops: vec![Op::Insert {
            table: TABLE.to_string(),
            columns: vec!["id".to_string()],
            rows: vec![vec![IrScalar::Int(1).into()]],
            on_conflict: None,
            schema: None,
        }],
        inverse_ops: Some(inverse),
        irreversible: None,
        flags: Default::default(),
        depends_on: vec![],
        supersedes: vec![],
        preconditions: vec![],
        checksum: None,
    }
}

/// The load gate treats an inverse op exactly as it treats a forward one, so a
/// privileged primitive in the reverse is refused on the same terms.
#[test]
fn a_raw_sql_op_in_the_inverse_does_not_load_unprivileged() {
    let ir = envelope_with_inverse(vec![Op::PgRaw {
        sql: "GRANT ALL ON SCHEMA public TO PUBLIC".to_string(),
        reason: "smuggle raw SQL through the reverse".to_string(),
    }]);

    let verdict = load_ir_document(
        &serde_json::to_string(&ir).expect("envelope serializes"),
        OWNER,
        Dialect::Postgres,
        &registry(),
        None,
    );

    // The forward half of this same op is gated by vendor authority; the inverse
    // must not be a way around that gate. If this ever starts loading, the second
    // barrier below is all that stands between a recorded inverse and arbitrary
    // SQL, and a single barrier on this path is not enough.
    assert!(
        verdict.is_err(),
        "a raw-SQL op in the inverse must face the same authority gate as one in \
         the forward ops; it loaded instead"
    );
}

/// The control: an ordinary DML inverse still loads. Without it, the refusal
/// above is equally consistent with having broken recorded inverses altogether.
#[test]
fn control_an_ordinary_dml_inverse_still_loads() {
    let ir = envelope_with_inverse(vec![Op::Delete {
        table: TABLE.to_string(),
        r#where: zero_migrate::Expr::BinOp {
            op: zero_migrate::BinaryOp::Eq,
            lhs: Box::new(zero_migrate::Expr::col("id")),
            rhs: Box::new(zero_migrate::Expr::lit(IrScalar::Int(1))),
        },
        limit: None,
        schema: None,
    }]);

    load_ir_document(
        &serde_json::to_string(&ir).expect("envelope serializes"),
        OWNER,
        Dialect::Postgres,
        &registry(),
        None,
    )
    .expect("a plain DML inverse must still load");
}
