use super::*;

fn executes(isolate: &mut v8::OwnedIsolate, context: &v8::Global<v8::Context>) -> bool {
    v8::scope!(let scope, isolate);
    let context = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(let scope, scope);
    let source = v8::String::new(scope, "1 + 1").unwrap();
    v8::Script::compile(scope, source, None)
        .and_then(|script| script.run(scope))
        .is_some()
}

#[test]
fn cpu_timer_drop_unregisters_and_late_notifications_cannot_target_replacement() {
    crate::init_v8();
    let mut isolate = v8::Isolate::new(Default::default());
    let context = {
        v8::scope!(let scope, &mut isolate);
        let context = v8::Context::new(scope, Default::default());
        v8::Global::new(scope, context)
    };
    let system = CpuTimerSystem::get_or_init();
    let note = Arc::new(AtomicBool::new(false));
    let timer = CpuTimer::new(isolate.thread_safe_handle(), note.clone()).unwrap();
    let expired_id = timer.registration.as_ref().unwrap().id;
    assert!(system
        .handles
        .lock()
        .unwrap()
        .handles
        .contains_key(&expired_id));
    drop(timer);
    assert!(!system
        .handles
        .lock()
        .unwrap()
        .handles
        .contains_key(&expired_id));

    let replacement = CpuTimer::new(isolate.thread_safe_handle(), note.clone()).unwrap();
    let replacement_id = replacement.registration.as_ref().unwrap().id;
    assert_ne!(expired_id, replacement_id);
    terminate_registered(&system.handles, expired_id);
    assert!(!note.load(Ordering::Relaxed));
    assert!(executes(&mut isolate, &context));

    // Positive control: a notification for the live timer reaches its isolate.
    terminate_registered(&system.handles, replacement_id);
    assert!(note.load(Ordering::Relaxed));
    assert!(!executes(&mut isolate, &context));
    isolate.cancel_terminate_execution();
    drop(replacement);
    assert!(!system
        .handles
        .lock()
        .unwrap()
        .handles
        .contains_key(&replacement_id));
}

#[test]
fn abandoned_cpu_timer_registration_releases_its_handle() {
    crate::init_v8();
    let isolate = v8::Isolate::new(Default::default());
    let system = CpuTimerSystem::get_or_init();
    let note = Arc::new(AtomicBool::new(false));
    let weak = Arc::downgrade(&note);
    let registration = system.register(isolate.thread_safe_handle(), note).unwrap();
    let id = registration.id;
    assert!(weak.upgrade().is_some());
    // Constructor errors drop this same owned registration before returning.
    drop(registration);
    assert!(weak.upgrade().is_none());
    assert!(!system.handles.lock().unwrap().handles.contains_key(&id));
}
