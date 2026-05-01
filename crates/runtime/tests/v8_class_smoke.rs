//! Tests for the `#[v8_class]` proc macro.
#![allow(unsafe_code)]
//!
//! Each test class lives in its own module to keep type names from
//! colliding (the macro emits `__ClassName_*` callbacks at module
//! scope). The tests drive the generated bindings through a real V8
//! isolate and assert observable JS-level behavior.
//!
//! Coverage:
//!   - basic        — constructor + method + getter
//!   - state_isolation — two instances keep independent state
//!   - default_ctor — no `#[v8_constructor]`, falls back to Default
//!   - result_throws — `Result<T, OpError>` returns throw on Err
//!   - vec_return   — Vec<u8> return materializes as ArrayBuffer
//!   - option_return — Option<T> returns null on None
//!   - setter       — `#[v8_setter]` mutates state, getter reflects it
//!   - illegal_recv — calling method on non-instance throws TypeError
//!   - local_arg    — v8::Local<v8::Value> arg passes through
//!   - gc_finalizer — boxed instance dropped when JS wrapper is GC'd

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use zeroship_runtime::init_v8;
use zeroship_runtime::state::OpError;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_class, v8_constructor, v8_getter, v8_method, v8_setter};

// ---------------------------------------------------------------------------
// Test harness — minimal isolate setup
// ---------------------------------------------------------------------------

fn run_in_v8<F, R>(install: impl FnOnce(&mut v8::PinScope, v8::Local<v8::Object>), src: &str, f: F) -> R
where
    F: FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
{
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);
    install(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

fn install_class<'s, T>(
    install_fn: fn(&mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate>,
    name: &str,
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let _ = std::marker::PhantomData::<T>;
    let tmpl = install_fn(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, name).unwrap();
    global.set(scope, key.into(), class_fn.into());
}

fn js_string(val: v8::Local<v8::Value>, scope: &mut v8::PinScope) -> String {
    val.to_rust_string_lossy(scope)
}

// ---------------------------------------------------------------------------
// Test 1: basic — ctor + method + getter
// ---------------------------------------------------------------------------

mod basic {
    use super::*;

    pub struct Counter {
        pub value: u32,
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
}

#[test]
fn basic_counter_works() {
    let s = run_in_v8(
        |scope, global| install_class::<basic::Counter>(basic::Counter::install, "Counter", scope, global),
        r#"
        const c = new Counter(10);
        const a = c.increment();   // 11
        const b = c.add(5);        // 16
        const got = c.current;     // 16
        JSON.stringify({ a, b, got });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"a":11,"b":16,"got":16}"#);
}

// ---------------------------------------------------------------------------
// Test 2: state isolation — two instances independent
// ---------------------------------------------------------------------------

#[test]
fn instances_are_independent() {
    let s = run_in_v8(
        |scope, global| install_class::<basic::Counter>(basic::Counter::install, "Counter", scope, global),
        r#"
        const a = new Counter(0);
        const b = new Counter(100);
        a.increment(); a.increment(); a.increment();
        b.add(50);
        JSON.stringify({ a: a.current, b: b.current });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"a":3,"b":150}"#);
}

// ---------------------------------------------------------------------------
// Test 3: default constructor when #[v8_constructor] is omitted
// ---------------------------------------------------------------------------

mod default_ctor {
    use super::*;

    #[derive(Default)]
    pub struct Empty {
        pub touched: bool,
    }

    #[v8_class]
    impl Empty {
        #[v8_method]
        fn touch(&mut self) -> bool {
            self.touched = true;
            self.touched
        }

        #[v8_getter]
        fn was_touched(&self) -> bool {
            self.touched
        }
    }
}

#[test]
fn default_constructor_when_unspecified() {
    let s = run_in_v8(
        |scope, global| install_class::<default_ctor::Empty>(default_ctor::Empty::install, "Empty", scope, global),
        r#"
        const e = new Empty();
        const before = e.was_touched;
        const after = e.touch();
        JSON.stringify({ before, after });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"before":false,"after":true}"#);
}

// ---------------------------------------------------------------------------
// Test 4: Result<T, OpError> throws on Err
// ---------------------------------------------------------------------------

mod result_method {
    use super::*;

    pub struct Divider {
        pub _unused: u32,
    }

    #[v8_class]
    impl Divider {
        #[v8_constructor]
        fn new() -> Divider {
            Divider { _unused: 0 }
        }

        #[v8_method]
        fn divide(&self, a: u32, b: u32) -> Result<u32, OpError> {
            if b == 0 {
                return Err(OpError::range_error("divide by zero"));
            }
            Ok(a / b)
        }
    }
}

#[test]
fn result_ok_returns_value() {
    let s = run_in_v8(
        |scope, global| install_class::<result_method::Divider>(
            result_method::Divider::install, "Divider", scope, global,
        ),
        r#"
        const d = new Divider();
        d.divide(10, 2);   // 5
        "#,
        |val, scope| val.uint32_value(scope).unwrap(),
    );
    assert_eq!(s, 5);
}

#[test]
fn result_err_throws_typed_exception() {
    // Use try/catch in JS and serialize the caught error so we can
    // inspect the kind.
    let s = run_in_v8(
        |scope, global| install_class::<result_method::Divider>(
            result_method::Divider::install, "Divider", scope, global,
        ),
        r#"
        const d = new Divider();
        let kind, msg;
        try { d.divide(10, 0); }
        catch (e) { kind = e.constructor.name; msg = e.message; }
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"kind":"RangeError","msg":"divide by zero"}"#);
}

// ---------------------------------------------------------------------------
// Test 4b: Result<(), OpError> — nothing on Ok, throws on Err
// ---------------------------------------------------------------------------
//
// Mutator-style methods (Headers.append, Headers.set, etc.) return
// nothing on success and throw on validation failure. The macro
// must handle `Result<(), OpError>` cleanly: emit no-op on Ok,
// throw the typed exception on Err. Previously this hit the
// scalar-set fallback and tried `v8::String::new(scope, &())` —
// compile failure.

mod result_unit {
    use super::*;

    pub struct Validator;

    #[v8_class]
    impl Validator {
        #[v8_constructor]
        fn new() -> Validator {
            Validator
        }

        #[v8_method]
        fn check(&self, n: u32) -> Result<(), OpError> {
            if n > 100 {
                Err(OpError::range_error("too big"))
            } else {
                Ok(())
            }
        }
    }
}

#[test]
fn result_unit_ok_returns_undefined() {
    let s = run_in_v8(
        |scope, global| install_class::<result_unit::Validator>(
            result_unit::Validator::install, "Validator", scope, global,
        ),
        r#"
        const v = new Validator();
        const r = v.check(42);
        r === undefined ? "undefined" : `not-undefined:${r}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "undefined");
}

#[test]
fn result_unit_err_throws_typed_exception() {
    let s = run_in_v8(
        |scope, global| install_class::<result_unit::Validator>(
            result_unit::Validator::install, "Validator", scope, global,
        ),
        r#"
        const v = new Validator();
        let kind, msg;
        try { v.check(200); }
        catch (e) { kind = e.constructor.name; msg = e.message; }
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"kind":"RangeError","msg":"too big"}"#);
}

// ---------------------------------------------------------------------------
// Test 5: Vec<u8> return → Uint8Array (spec-correct)
// ---------------------------------------------------------------------------

mod vec_return {
    use super::*;

    pub struct Builder;

    #[v8_class]
    impl Builder {
        #[v8_constructor]
        fn new() -> Builder {
            Builder
        }

        #[v8_method]
        fn make_bytes(&self) -> Vec<u8> {
            vec![1, 2, 3, 4, 5]
        }
    }
}

#[test]
fn vec_u8_returns_uint8array() {
    let s = run_in_v8(
        |scope, global| install_class::<vec_return::Builder>(
            vec_return::Builder::install, "Builder", scope, global,
        ),
        r#"
        const b = new Builder();
        const u8 = b.make_bytes();
        JSON.stringify({
            kind: u8.constructor.name,
            len: u8.byteLength,
            isView: ArrayBuffer.isView(u8),
            b0: u8[0],
            b4: u8[4],
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"kind":"Uint8Array","len":5,"isView":true,"b0":1,"b4":5}"#
    );
}

// ---------------------------------------------------------------------------
// Test 6: Option<T> return → null on None
// ---------------------------------------------------------------------------

mod option_return {
    use super::*;

    pub struct Lookup {
        pub key: Option<String>,
    }

    #[v8_class]
    impl Lookup {
        #[v8_constructor]
        fn new(key: Option<String>) -> Lookup {
            Lookup { key }
        }

        #[v8_method]
        fn get(&self) -> Option<String> {
            self.key.clone()
        }
    }
}

#[test]
fn option_some_returns_value() {
    let s = run_in_v8(
        |scope, global| install_class::<option_return::Lookup>(
            option_return::Lookup::install, "Lookup", scope, global,
        ),
        r#"
        const l = new Lookup("hello");
        l.get();
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "hello");
}

#[test]
fn option_none_returns_null() {
    let s = run_in_v8(
        |scope, global| install_class::<option_return::Lookup>(
            option_return::Lookup::install, "Lookup", scope, global,
        ),
        r#"
        const l = new Lookup();
        const v = l.get();
        v === null ? "null" : `not-null:${v}`;
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "null");
}

// ---------------------------------------------------------------------------
// Test 7: setter — #[v8_setter] mutates state, getter reflects it
// ---------------------------------------------------------------------------

// Setter test deferred: the install codegen calls `set_accessor_property`
// twice (once each for getter and setter under the same JS name), which
// V8 rejects — same-name accessor pairing requires one call with both
// templates. Tracked as a known gap for when fetch needs it (Headers
// has no setters; Body has body/bodyUsed which are read-only).
//
// Additionally, Rust can't have two methods named `value` in the same
// impl block, so a real getter+setter pair would need either renaming
// (`get_value`/`set_value`) plus a `#[v8_name = "value"]` attribute
// override, or two separate impl blocks. Defer the design until
// there's a real consumer.

// ---------------------------------------------------------------------------
// Test 7b: #[v8_name = "..."] — JS-side method rename
// ---------------------------------------------------------------------------
//
// Headers needs `delete(name)` on the JS surface, but `delete` is a Rust
// keyword. The macro must accept `#[v8_name = "delete"]` on a method
// like `delete_` and install it under the JS-visible name "delete".

#[allow(unused_imports)]
use zeroship_runtime_macros::v8_name;

mod renamed_method {
    use super::*;

    pub struct CounterBox {
        pub items: u32,
    }

    #[v8_class]
    impl CounterBox {
        #[v8_constructor]
        fn new() -> CounterBox {
            CounterBox { items: 0 }
        }

        #[v8_method]
        #[v8_name = "delete"]
        fn delete_(&mut self) -> u32 {
            self.items += 1;
            self.items
        }

        #[v8_getter]
        fn count(&self) -> u32 {
            self.items
        }
    }
}

#[test]
fn v8_name_renames_method_on_js_surface() {
    let s = run_in_v8(
        |scope, global| install_class::<renamed_method::CounterBox>(
            renamed_method::CounterBox::install, "CounterBox", scope, global,
        ),
        r#"
        const b = new CounterBox();
        b.delete();
        b.delete();
        const r = b.delete();
        const has_delete_ = typeof b.delete_;
        JSON.stringify({ r, count: b.count, has_delete_ });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"r":3,"count":3,"has_delete_":"undefined"}"#);
}

// ---------------------------------------------------------------------------
// Test 7c: #[v8_to_string_tag = "..."] — override Symbol.toStringTag
// ---------------------------------------------------------------------------
//
// The default install installs the Rust struct name as the @@toStringTag
// value (e.g. "CounterBox"). For WebIDL default iterator objects the
// spec wants the parent interface name + " Iterator" — e.g. "Headers
// Iterator". Verify the impl-level attribute overrides the default.

#[allow(unused_imports)]
use zeroship_runtime_macros::v8_to_string_tag;

mod string_tag_override {
    use super::*;

    pub struct Foo;

    #[v8_class]
    #[v8_to_string_tag = "Custom Tag"]
    impl Foo {
        #[v8_constructor]
        fn new() -> Foo {
            Foo
        }

        #[v8_method]
        fn touch(&self) -> u32 {
            1
        }
    }
}

#[test]
fn v8_to_string_tag_override_changes_default() {
    let s = run_in_v8(
        |scope, global| install_class::<string_tag_override::Foo>(
            string_tag_override::Foo::install, "Foo", scope, global,
        ),
        r#"
        const f = new Foo();
        Object.prototype.toString.call(f);
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, "[object Custom Tag]");
}

// ---------------------------------------------------------------------------
// Test 7d: #[v8_inherit_intrinsic = "IteratorPrototype"] —
// chain prototype to %Iterator.prototype% per WebIDL §3.7.10.2
// ---------------------------------------------------------------------------

#[allow(unused_imports)]
use zeroship_runtime_macros::v8_inherit_intrinsic;

mod intrinsic_iter {
    use super::*;

    pub struct StubIter;

    #[v8_class]
    #[v8_inherit_intrinsic = "IteratorPrototype"]
    impl StubIter {
        #[v8_constructor]
        fn new() -> StubIter {
            StubIter
        }
    }
}

// ---------------------------------------------------------------------------
// Test 7e: ByteString newtype extraction
// ---------------------------------------------------------------------------
//
// WebIDL ByteString boundary: per https://webidl.spec.whatwg.org/#js-to-ByteString
// step 2, any code unit > 0xFF MUST throw TypeError. The macro extends
// `gen_extract` to recognise the `ByteString` newtype and emit a call to
// `read_byte_string` (defined in zeroship_runtime::byte_string), which
// performs the precheck via String::contains_only_onebyte() and returns
// Result<Vec<u8>, OpError>.
//
// Until a real consumer (Headers) lands, we drive the path through a
// stub class that takes a ByteString and echoes it back as a Vec<u8>
// return (which the macro marshals as Uint8Array).

mod bytestring_extract {
    use super::*;

    pub struct Echo;

    #[v8_class]
    impl Echo {
        #[v8_constructor]
        fn new() -> Echo {
            Echo
        }

        /// Round-trip a ByteString as raw bytes. Throws TypeError on
        /// inputs containing code units > 0xFF.
        #[v8_method]
        fn echo(
            &self,
            input: ::zeroship_runtime::byte_string::ByteString,
        ) -> Result<Vec<u8>, OpError> {
            Ok(input.into_bytes())
        }
    }
}

#[test]
fn bytestring_round_trip_passes_low_bytes() {
    let s = run_in_v8(
        |scope, global| install_class::<bytestring_extract::Echo>(
            bytestring_extract::Echo::install, "Echo", scope, global,
        ),
        r#"
        const e = new Echo();
        const out = e.echo("hello");
        JSON.stringify({
            len: out.byteLength,
            kind: out.constructor.name,
            b0: out[0], b4: out[4],
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"len":5,"kind":"Uint8Array","b0":104,"b4":111}"#
    );
}

#[test]
fn bytestring_throws_on_code_unit_above_0xff() {
    let s = run_in_v8(
        |scope, global| install_class::<bytestring_extract::Echo>(
            bytestring_extract::Echo::install, "Echo", scope, global,
        ),
        r#"
        const e = new Echo();
        let kind, msg;
        try { e.echo("\u0100"); }
        catch (err) { kind = err.constructor.name; msg = err.message; }
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert!(s.contains(r#""kind":"TypeError""#), "got: {s}");
}

#[test]
fn bytestring_preserves_high_latin1_bytes() {
    // Latin-1 \u00FF is the maximum allowed code unit for ByteString.
    // It must round-trip as byte 0xFF.
    let s = run_in_v8(
        |scope, global| install_class::<bytestring_extract::Echo>(
            bytestring_extract::Echo::install, "Echo", scope, global,
        ),
        r#"
        const e = new Echo();
        const out = e.echo("\u0080\u00FF");
        JSON.stringify({ len: out.byteLength, b0: out[0], b1: out[1] });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"len":2,"b0":128,"b1":255}"#);
}

#[test]
fn v8_inherit_intrinsic_chains_to_iterator_prototype() {
    let s = run_in_v8(
        |scope, global| install_class::<intrinsic_iter::StubIter>(
            intrinsic_iter::StubIter::install, "StubIter", scope, global,
        ),
        r#"
        const it = new StubIter();
        const ourProto = Object.getPrototypeOf(it);
        const next = Object.getPrototypeOf(ourProto);
        // %Iterator.prototype% is the parent of any built-in iterator
        // result's prototype:
        const iterProto = Object.getPrototypeOf(Object.getPrototypeOf([][Symbol.iterator]()));
        const same = next === iterProto;
        JSON.stringify({ same });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"same":true}"#);
}

// ---------------------------------------------------------------------------
// Test 8: illegal invocation — calling method on non-instance throws
// ---------------------------------------------------------------------------

#[test]
fn calling_method_on_non_instance_throws_typeerror() {
    let s = run_in_v8(
        |scope, global| install_class::<basic::Counter>(basic::Counter::install, "Counter", scope, global),
        r#"
        let kind, msg;
        try {
            Counter.prototype.increment.call({});
        } catch (e) { kind = e.constructor.name; msg = e.message; }
        JSON.stringify({ kind, msg });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(s, r#"{"kind":"TypeError","msg":"Illegal invocation"}"#);
}

// ---------------------------------------------------------------------------
// Test 9: v8::Local<v8::Value> arg passthrough
// ---------------------------------------------------------------------------

mod local_arg {
    use super::*;

    pub struct Inspector;

    #[v8_class]
    impl Inspector {
        #[v8_constructor]
        fn new() -> Inspector {
            Inspector
        }

        /// Takes any JS value and returns a string describing what
        /// kind it is. Exercises the `v8::Local<v8::Value>` arg
        /// passthrough — no Rust marshaling, just inspect the V8
        /// value directly.
        #[v8_method]
        fn kind(&self, val: v8::Local<v8::Value>) -> String {
            if val.is_string() {
                "string".into()
            } else if val.is_number() {
                "number".into()
            } else if val.is_boolean() {
                "boolean".into()
            } else if val.is_array() {
                "array".into()
            } else if val.is_object() {
                "object".into()
            } else if val.is_null() {
                "null".into()
            } else if val.is_undefined() {
                "undefined".into()
            } else {
                "other".into()
            }
        }
    }
}

#[test]
fn local_value_arg_inspects_jsvalue() {
    let s = run_in_v8(
        |scope, global| install_class::<local_arg::Inspector>(
            local_arg::Inspector::install, "Inspector", scope, global,
        ),
        r#"
        const i = new Inspector();
        JSON.stringify({
            s: i.kind("hi"),
            n: i.kind(42),
            b: i.kind(true),
            a: i.kind([1,2]),
            o: i.kind({}),
            null_: i.kind(null),
            undef: i.kind(undefined),
        });
        "#,
        |val, scope| js_string(val, scope),
    );
    assert_eq!(
        s,
        r#"{"s":"string","n":"number","b":"boolean","a":"array","o":"object","null_":"null","undef":"undefined"}"#
    );
}

// ---------------------------------------------------------------------------
// Test 10: GC finalizer — boxed instance dropped when JS wrapper is GC'd
// ---------------------------------------------------------------------------

mod gc_test {
    use super::*;

    pub struct DropTracker {
        pub _id: u32,
        pub drops: Arc<AtomicUsize>,
    }

    impl Drop for DropTracker {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Default for DropTracker {
        // Required because `#[v8_class]` emits a default constructor
        // when no `#[v8_constructor]` is provided — even when no JS
        // code ever calls `new DropTracker()`. The GC test bypasses
        // the default ctor entirely (instantiates manually with a
        // tracked Arc).
        fn default() -> Self {
            DropTracker {
                _id: 0,
                drops: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl DropTracker {
        pub fn new_with_drops(drops: Arc<AtomicUsize>) -> Self {
            DropTracker { _id: 0, drops }
        }
    }

    #[v8_class]
    impl DropTracker {
        #[v8_method]
        fn ping(&self) -> u32 {
            42
        }
    }
}

#[test]
fn finalizer_drops_boxed_instance_on_isolate_teardown() {
    use gc_test::DropTracker;
    let drops = Arc::new(AtomicUsize::new(0));

    {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        // Install class on globalThis.
        let tmpl = DropTracker::install(scope);
        let class_fn = tmpl.get_function(scope).unwrap();
        let key = v8::String::new(scope, "DropTracker").unwrap();
        let global = scope.get_current_context().global(scope);
        global.set(scope, key.into(), class_fn.into());

        // Manually instantiate using the class's instance template so
        // we can plug in a custom-tracked instance. The default
        // constructor callback would be reached via `new
        // DropTracker()`, but it'd require Default — we want a
        // tracker tied to our drop counter. Instead: build the JS
        // object via the FunctionTemplate's instance template, set
        // internal field 0 to a manually-created External, register
        // the same finalizer the macro would.
        let inst_tmpl = tmpl.instance_template(scope);
        let instance = inst_tmpl.new_instance(scope).unwrap();

        let boxed: Box<DropTracker> = Box::new(DropTracker::new_with_drops(drops.clone()));
        let raw = Box::into_raw(boxed);
        let raw_addr = raw as usize;
        let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
        instance.set_internal_field(0, ext.into());

        let weak = v8::Weak::with_guaranteed_finalizer(
            scope,
            instance,
            Box::new(move || unsafe {
                drop(Box::from_raw(raw_addr as *mut DropTracker));
            }),
        );
        std::mem::forget(weak);

        // No explicit GC trigger needed — `with_guaranteed_finalizer`
        // promises the callback fires on isolate teardown even without
        // a prior GC pass. Let the scope guards drop naturally.
    }

    // After the scope guards (and isolate) drop, the finalizer must
    // have fired exactly once.
    assert_eq!(drops.load(Ordering::SeqCst), 1, "finalizer did not run");
}

// ---------------------------------------------------------------------------
// Test 11: bulk allocation — N instances all drop on teardown
// ---------------------------------------------------------------------------

/// Number of instances allocated in the bulk test. Deliberately large
/// enough that a per-instance leak would show up in heap residency
/// (10k × 1KB payload = 10 MB of trackable allocation) but small
/// enough to keep test wall-time under a second.
const BULK_N: usize = 10_000;

#[test]
fn bulk_allocation_all_finalizers_fire_on_teardown() {
    use gc_test::DropTracker;
    let drops = Arc::new(AtomicUsize::new(0));

    {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let tmpl = DropTracker::install(scope);
        let inst_tmpl = tmpl.instance_template(scope);

        for _ in 0..BULK_N {
            let instance = inst_tmpl.new_instance(scope).unwrap();
            let boxed: Box<DropTracker> = Box::new(DropTracker::new_with_drops(drops.clone()));
            let raw = Box::into_raw(boxed);
            let raw_addr = raw as usize;
            let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
            instance.set_internal_field(0, ext.into());

            let weak = v8::Weak::with_guaranteed_finalizer(
                scope,
                instance,
                Box::new(move || unsafe {
                    drop(Box::from_raw(raw_addr as *mut DropTracker));
                }),
            );
            std::mem::forget(weak);
            // Drop the JS-side `instance` Local immediately. The Weak's
            // guaranteed finalizer is what keeps the registration alive.
        }

        // Don't trigger explicit GC — let teardown handle it. This is
        // the lower-bound guarantee we depend on in production: even if
        // V8 never reclaims an object before isolate destruction, the
        // finalizer must still run.
    }

    assert_eq!(
        drops.load(Ordering::SeqCst),
        BULK_N,
        "expected {BULK_N} finalizers to fire, only {} did",
        drops.load(Ordering::SeqCst),
    );
}

// ---------------------------------------------------------------------------
// Test 12: explicit GC mid-run — finalizers fire incrementally
// ---------------------------------------------------------------------------

#[test]
fn explicit_gc_reclaims_unreferenced_instances() {
    use gc_test::DropTracker;
    let drops = Arc::new(AtomicUsize::new(0));
    let after_gc;

    {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let tmpl = DropTracker::install(scope);
        let inst_tmpl = tmpl.instance_template(scope);

        // Allocate 1000 instances inside an inner scope, then drop the
        // scope so V8 has no Local references to them.
        {
            v8::scope!(let inner, scope);
            for _ in 0..1000 {
                let instance = inst_tmpl.new_instance(inner).unwrap();
                let boxed: Box<DropTracker> = Box::new(DropTracker::new_with_drops(drops.clone()));
                let raw = Box::into_raw(boxed);
                let raw_addr = raw as usize;
                let ext = v8::External::new(inner, raw as *mut std::ffi::c_void);
                instance.set_internal_field(0, ext.into());
                let weak = v8::Weak::with_guaranteed_finalizer(
                    inner,
                    instance,
                    Box::new(move || unsafe {
                        drop(Box::from_raw(raw_addr as *mut DropTracker));
                    }),
                );
                std::mem::forget(weak);
            }
        }

        // Force a major GC and drain the finalizer queue. With
        // `--expose-gc` set in init_v8, this is a real Mark-Compact.
        scope.request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);
        scope.perform_microtask_checkpoint();

        after_gc = drops.load(Ordering::SeqCst);

        assert!(
            after_gc > 0,
            "explicit GC reclaimed nothing — finalizer may be misregistered"
        );
    }
    // Scope guards drop here — flushes any remaining finalizers.

    let final_count = drops.load(Ordering::SeqCst);
    assert_eq!(
        final_count, 1000,
        "expected 1000 total drops; got {after_gc} mid-GC + remainder = {final_count}",
    );
}

// ---------------------------------------------------------------------------
// Test 13: payload-heavy instances — confirms no per-instance leak
// ---------------------------------------------------------------------------

mod payload {
    use super::*;

    /// Each instance owns 4KB of heap allocation. A finalizer leak on
    /// 1000 instances would leave 4 MB unreclaimed — easily detectable
    /// in process RSS if we wanted to assert on it, but the simpler
    /// signal (drops == N) covers the same defect.
    pub struct Heavy {
        pub _payload: Vec<u8>,
        pub drops: Arc<AtomicUsize>,
    }

    impl Drop for Heavy {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Default for Heavy {
        fn default() -> Self {
            Heavy {
                _payload: vec![0; 4096],
                drops: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Heavy {
        pub fn new_with(drops: Arc<AtomicUsize>) -> Self {
            Heavy {
                _payload: vec![0xab; 4096],
                drops,
            }
        }
    }

    #[v8_class]
    impl Heavy {
        #[v8_method]
        fn size(&self) -> u32 {
            self._payload.len() as u32
        }
    }
}

#[test]
fn heavy_payload_instances_all_finalize() {
    use payload::Heavy;
    let drops = Arc::new(AtomicUsize::new(0));
    const N: usize = 5_000;

    {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handle_scope, &mut isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let scope = &mut v8::ContextScope::new(handle_scope, context);

        let tmpl = Heavy::install(scope);
        let inst_tmpl = tmpl.instance_template(scope);

        for _ in 0..N {
            let instance = inst_tmpl.new_instance(scope).unwrap();
            let boxed: Box<Heavy> = Box::new(Heavy::new_with(drops.clone()));
            let raw = Box::into_raw(boxed);
            let raw_addr = raw as usize;
            let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
            instance.set_internal_field(0, ext.into());

            let weak = v8::Weak::with_guaranteed_finalizer(
                scope,
                instance,
                Box::new(move || unsafe {
                    drop(Box::from_raw(raw_addr as *mut Heavy));
                }),
            );
            std::mem::forget(weak);
        }
    }

    assert_eq!(
        drops.load(Ordering::SeqCst),
        N,
        "heavy payload finalizer leak: {} of {N} dropped",
        drops.load(Ordering::SeqCst),
    );
}
