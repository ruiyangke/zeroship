//! Tests for the native streams module's internal primitives:
//! microtask scheduling, promise reaction helpers, slot helpers.
//!
//! These tests use a raw V8 isolate (via the same harness as v8_class_smoke).
//! Higher-level tests that exercise full ReadableStream / WritableStream /
//! TransformStream classes through user JS live in `streams.rs`.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use zeroship_runtime::init_v8;
use zeroship_runtime::streams::promise_resolve::{
    enqueue_microtask, set_promise_is_handled_to_true, upon_promise,
};
use zeroship_runtime::streams::slots::{delete_slot, read_slot, slot_is_empty, write_slot};

fn run_in_v8<F, R>(f: F) -> R
where
    F: FnOnce(&mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    f(scope)
}

// ---------------------------------------------------------------------------
// slots.rs — V8 private symbol read/write/delete
// ---------------------------------------------------------------------------

#[test]
fn slot_read_returns_undefined_when_unset() {
    run_in_v8(|scope| {
        let obj = v8::Object::new(scope);
        let v = read_slot(scope, obj, "[[reader]]");
        assert!(v.is_undefined(), "unset slot should read as undefined");
    });
}

#[test]
fn slot_write_then_read_returns_value() {
    run_in_v8(|scope| {
        let obj = v8::Object::new(scope);
        let val = v8::Number::new(scope, 42.0);
        write_slot(scope, obj, "[[reader]]", val.into());
        let read = read_slot(scope, obj, "[[reader]]");
        assert_eq!(read.number_value(scope).unwrap(), 42.0);
    });
}

#[test]
fn slot_delete_clears_to_undefined() {
    run_in_v8(|scope| {
        let obj = v8::Object::new(scope);
        let val = v8::Number::new(scope, 7.0);
        write_slot(scope, obj, "[[reader]]", val.into());
        assert!(!slot_is_empty(scope, obj, "[[reader]]"));
        delete_slot(scope, obj, "[[reader]]");
        assert!(slot_is_empty(scope, obj, "[[reader]]"));
    });
}

#[test]
fn slot_different_names_are_independent() {
    // Critical invariant: the slot name acts as the unique key. Writing
    // [[reader]] must not be observable via [[controller]].
    run_in_v8(|scope| {
        let obj = v8::Object::new(scope);
        let val_a = v8::Number::new(scope, 1.0);
        let val_b = v8::Number::new(scope, 2.0);
        write_slot(scope, obj, "[[reader]]", val_a.into());
        write_slot(scope, obj, "[[controller]]", val_b.into());
        assert_eq!(
            read_slot(scope, obj, "[[reader]]").number_value(scope).unwrap(),
            1.0
        );
        assert_eq!(
            read_slot(scope, obj, "[[controller]]").number_value(scope).unwrap(),
            2.0
        );
    });
}

#[test]
fn slot_separate_objects_have_separate_slots() {
    run_in_v8(|scope| {
        let a = v8::Object::new(scope);
        let b = v8::Object::new(scope);
        let v_a = v8::Number::new(scope, 100.0);
        let v_b = v8::Number::new(scope, 200.0);
        write_slot(scope, a, "[[reader]]", v_a.into());
        write_slot(scope, b, "[[reader]]", v_b.into());
        assert_eq!(read_slot(scope, a, "[[reader]]").number_value(scope).unwrap(), 100.0);
        assert_eq!(read_slot(scope, b, "[[reader]]").number_value(scope).unwrap(), 200.0);
    });
}

// ---------------------------------------------------------------------------
// promise_resolve.rs — enqueue_microtask
// ---------------------------------------------------------------------------

#[test]
fn enqueue_microtask_runs_closure_on_checkpoint() {
    run_in_v8(|scope| {
        let counter: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
        let counter_cb = counter.clone();
        enqueue_microtask(scope, move |_scope| {
            *counter_cb.borrow_mut() += 1;
        });
        // Microtask hasn't run yet — V8 only runs them on checkpoints
        // (or after callback return).
        assert_eq!(*counter.borrow(), 0);
        scope.perform_microtask_checkpoint();
        assert_eq!(*counter.borrow(), 1);
    });
}

#[test]
fn enqueue_microtask_multiple_run_in_fifo_order() {
    run_in_v8(|scope| {
        let log: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));
        for i in 1..=5u32 {
            let log_cb = log.clone();
            enqueue_microtask(scope, move |_scope| {
                log_cb.borrow_mut().push(i);
            });
        }
        scope.perform_microtask_checkpoint();
        assert_eq!(*log.borrow(), vec![1, 2, 3, 4, 5]);
    });
}

#[test]
fn enqueue_microtask_can_be_used_to_resolve_promise() {
    run_in_v8(|scope| {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let resolver_g = v8::Global::new(scope, resolver);
        enqueue_microtask(scope, move |scope| {
            let r = v8::Local::new(scope, &resolver_g);
            let v = v8::Number::new(scope, 99.0);
            r.resolve(scope, v.into());
        });
        // Pre-checkpoint: still pending.
        assert_eq!(promise.state(), v8::PromiseState::Pending);
        scope.perform_microtask_checkpoint();
        assert_eq!(promise.state(), v8::PromiseState::Fulfilled);
        let result = promise.result(scope);
        assert_eq!(result.number_value(scope).unwrap(), 99.0);
    });
}

// ---------------------------------------------------------------------------
// promise_resolve.rs — upon_promise
// ---------------------------------------------------------------------------

#[test]
fn upon_promise_fulfilled_runs_handler() {
    run_in_v8(|scope| {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let v = v8::Number::new(scope, 7.0);
        resolver.resolve(scope, v.into());

        let log: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));
        let log_cb = log.clone();
        upon_promise(
            scope,
            promise,
            Some(Box::new(move |scope, val| {
                log_cb.borrow_mut().push(val.number_value(scope).unwrap());
            })),
            None,
        );
        scope.perform_microtask_checkpoint();
        assert_eq!(*log.borrow(), vec![7.0]);
    });
}

#[test]
fn upon_promise_rejected_runs_rejection_handler() {
    run_in_v8(|scope| {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let err_msg = v8::String::new(scope, "boom").unwrap();
        let err = v8::Exception::error(scope, err_msg);
        resolver.reject(scope, err);

        let caught: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let caught_cb = caught.clone();
        upon_promise(
            scope,
            promise,
            None,
            Some(Box::new(move |scope, reason| {
                let s = reason.to_rust_string_lossy(scope);
                *caught_cb.borrow_mut() = Some(s);
            })),
        );
        scope.perform_microtask_checkpoint();
        let got = caught.borrow().clone();
        // Error.toString includes "Error: boom"
        assert!(got.as_deref().unwrap().contains("boom"), "got {got:?}");
    });
}

#[test]
fn set_promise_is_handled_does_not_resurface_rejection() {
    run_in_v8(|scope| {
        let resolver = v8::PromiseResolver::new(scope).unwrap();
        let promise = resolver.get_promise(scope);
        let err_msg = v8::String::new(scope, "ignored").unwrap();
        let err = v8::Exception::error(scope, err_msg);
        resolver.reject(scope, err);

        // set_promise_is_handled_to_true installs a default rejection
        // handler — V8 should not invoke an unhandled-rejection callback.
        // The simplest assertion here is that this call does not panic.
        set_promise_is_handled_to_true(scope, promise);
        scope.perform_microtask_checkpoint();
        // Promise must still be observably rejected.
        assert_eq!(promise.state(), v8::PromiseState::Rejected);
    });
}

// ---------------------------------------------------------------------------
// budget.rs — concurrent stream cap
// ---------------------------------------------------------------------------

#[test]
fn budget_allocates_under_cap() {
    use zeroship_runtime::streams::budget::{live_count, try_alloc_stream};
    let baseline = live_count();
    let _g = try_alloc_stream().unwrap();
    assert_eq!(live_count(), baseline + 1);
    drop(_g);
    assert_eq!(live_count(), baseline);
}
