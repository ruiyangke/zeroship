//! Smoke test for `#[v8_class]`. Defines a toy `Counter` class with the
//! macro, drives it through a real V8 isolate, and asserts the
//! constructor + method + getter wiring works end-to-end.
//!
//! This is the first user of the macro; if anything is structurally
//! wrong with the codegen it surfaces here before we touch fetch.

use zeroship_runtime::init_v8;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method};

// ---------------------------------------------------------------------------
// Toy class under test
// ---------------------------------------------------------------------------

struct Counter {
    value: u32,
}

#[v8_class]
impl Counter {
    #[v8_constructor]
    fn new(start: Option<u32>) -> Counter {
        Counter {
            value: start.unwrap_or(0),
        }
    }

    #[v8_method]
    fn increment(&mut self) -> u32 {
        self.value += 1;
        self.value
    }

    #[v8_method]
    fn add(&mut self, n: u32) -> u32 {
        self.value += n;
        self.value
    }

    #[v8_getter]
    fn current(&self) -> u32 {
        self.value
    }
}

// ---------------------------------------------------------------------------
// Smoke test
// ---------------------------------------------------------------------------

#[test]
fn counter_via_macro_works_end_to_end() {
    init_v8();

    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);

    // Install Counter on globalThis.
    let tmpl = Counter::install(scope);
    let global = scope.get_current_context().global(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "Counter").unwrap();
    global.set(scope, key.into(), class_fn.into());

    // Drive it from JS.
    let src = v8::String::new(
        scope,
        r#"
        const c = new Counter(10);
        const a = c.increment();   // 11
        const b = c.add(5);        // 16
        const got = c.current;     // 16 (getter)
        JSON.stringify({ a, b, got });
        "#,
    )
    .unwrap();

    let script = v8::Script::compile(scope, src, None).unwrap();
    let result = script.run(scope).unwrap();
    let result_str = result.to_rust_string_lossy(scope);
    assert_eq!(result_str, r#"{"a":11,"b":16,"got":16}"#);
}
