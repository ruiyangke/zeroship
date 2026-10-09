//! Compile-fail: one admission copied to queue a second op.
//!
//! An admission is for one op: `spawn_op` consumes it, and it cannot be cloned
//! into a second one.

use zeroship_runtime::state::{admit_op, OpOwner, OpResult, SharedState};

fn callback(scope: &mut v8::PinScope, state: &SharedState) {
    let owner = admit_op(scope).expect("admitted");
    let second = OpOwner::clone(&owner);
    state.borrow_mut().spawn_op(owner, async { OpResult::Cancelled });
    state.borrow_mut().spawn_op(second, async { OpResult::Cancelled });
}

fn main() {}
