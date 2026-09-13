//! Keep ORM dependency capture with the V8 procedure that owns it.

#![allow(unsafe_code)]

use zeroship_data_orm::cdc::read_set::Capture;
use zeroship_data_orm::error::DbError;
use zeroship_runtime::rpc::{current_kind, ProcedureKind};

struct CaptureKey(v8::Global<v8::Private>);

fn key<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Private> {
    if let Some(key) = scope.get_slot::<CaptureKey>() {
        return v8::Local::new(scope, key.0.clone());
    }
    let name = v8::String::new(scope, "db:ReadCapture").unwrap();
    let key = v8::Private::new(scope, Some(name));
    let saved = v8::Global::new(scope, key);
    scope.set_slot(CaptureKey(saved));
    key
}

fn failed() -> DbError {
    DbError::Coded {
        code: "INTERNAL".into(),
        message: "could not bind the procedure's read capture".into(),
        hint: None,
    }
}

/// Capture lookup is performed before yielding. The private wrapper owns a
/// clone; queued operations keep another clone until completion or cancellation.
pub(super) fn current(scope: &mut v8::PinScope) -> Result<Capture, DbError> {
    let Some(frame) = zeroship_runtime::rpc::capability::current_procedure_frame(scope) else {
        return Ok(Capture::new(false));
    };
    let key = key(scope);
    let existing = frame.get_private(scope, key).ok_or_else(failed)?;
    if !existing.is_undefined() {
        let object = v8::Local::<v8::Object>::try_from(existing).map_err(|_| failed())?;
        if object.internal_field_count() != 1 {
            return Err(failed());
        }
        let field = object.get_internal_field(scope, 0).ok_or_else(failed)?;
        let external = v8::Local::<v8::External>::try_from(field).map_err(|_| failed())?;
        // The isolate-private key can only name a wrapper minted below. Its
        // strong V8 reference keeps the native allocation alive during lookup.
        return Ok(unsafe { (&*external.value().cast::<Capture>()).clone() });
    }

    let capture = Capture::new(current_kind(scope) == Some(ProcedureKind::Query));
    let template = v8::ObjectTemplate::new(scope);
    template.set_internal_field_count(1);
    let wrapper = template.new_instance(scope).ok_or_else(failed)?;
    let raw = Box::into_raw(Box::new(capture.clone()));
    let address = raw as usize;
    let external = v8::External::new(scope, raw.cast());
    wrapper.set_internal_field(0, external.into());
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        wrapper,
        Box::new(move || {
            // The wrapper owns this allocation until collection or isolate disposal.
            unsafe {
                drop(Box::from_raw(address as *mut Capture));
            }
        }),
    );
    std::mem::forget(weak);
    if frame.set_private(scope, key, wrapper.into()) != Some(true) {
        return Err(failed());
    }
    Ok(capture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_data_orm::cdc::read_set::{is_active, record_if_active};
    use zeroship_data_orm::{schema::FieldMap, value};
    use zeroship_runtime::rpc::with_kind;

    fn in_context<R>(body: impl FnOnce(&mut v8::PinScope<'_, '_>) -> R) -> R {
        zeroship_runtime::init_v8();
        let mut isolate = v8::Isolate::new(Default::default());
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        let scope = &mut v8::ContextScope::new(scope, context);
        body(scope)
    }

    fn read(capture: &Capture, id: &str) {
        capture.with(|| record_if_active("messages", &value!({"id":id}), &FieldMap::new()));
    }

    #[test]
    fn nested_query_captures_are_distinct_and_restore_the_parent() {
        in_context(|scope| {
            with_kind(scope, ProcedureKind::Query, |scope| {
                let outer = current(scope).unwrap();
                read(&outer, "outer-before");
                with_kind(scope, ProcedureKind::Query, |scope| {
                    let inner = current(scope).unwrap();
                    assert!(inner.snapshot_for("messages").is_empty());
                    read(&inner, "inner");
                    assert_eq!(current(scope).unwrap().snapshot_for("messages").len(), 1);
                });
                read(&current(scope).unwrap(), "outer-after");
                let rows = outer.snapshot_for("messages");
                assert_eq!(rows.len(), 2);
                assert!(!rows
                    .iter()
                    .any(|row| row.matches(&std::collections::HashMap::from([(
                        "id".into(),
                        "inner".into()
                    )]))));
                assert!(!is_active());
            })
        });
    }

    #[test]
    fn non_query_frames_and_unframed_calls_do_not_record() {
        in_context(|scope| {
            let outside = current(scope).unwrap();
            read(&outside, "outside");
            assert!(outside.snapshot_for("messages").is_empty());
            for kind in [
                ProcedureKind::Mutation,
                ProcedureKind::Action,
                ProcedureKind::Subscription,
            ] {
                with_kind(scope, kind, |scope| {
                    let capture = current(scope).unwrap();
                    read(&capture, "not-a-query");
                    assert!(capture.snapshot_for("messages").is_empty());
                });
            }
            assert!(!is_active());
        });
    }

    #[test]
    fn native_work_keeps_its_capture_after_isolate_disposal() {
        let capture = in_context(|scope| {
            with_kind(scope, ProcedureKind::Query, |scope| {
                let capture = current(scope).unwrap();
                read(&capture, "before-disposal");
                capture
            })
        });
        read(&capture, "after-disposal");
        assert_eq!(capture.snapshot_for("messages").len(), 2);
        assert!(!is_active());
    }
}
