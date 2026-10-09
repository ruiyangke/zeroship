//! Compile-fail: an op's owner kept past the callback the gate admitted it in.
//!
//! The gate's admission is valid for the callback that made the call. Moved
//! into a queued op's future, it would let that future queue a later op for
//! the request without passing the gate again, after the request may have
//! ended.

use zeroship_runtime::state::{admit_op, OpResult, SharedState};

fn callback(scope: &mut v8::PinScope, state: &SharedState) {
    let kept = admit_op(scope).expect("admitted");
    let owner = admit_op(scope).expect("admitted");
    state.borrow_mut().spawn_op(owner, async move {
        let _later = kept;
        OpResult::Cancelled
    });
}

fn main() {}
