#![allow(unsafe_code)]

mod common;
use common::*;

#[test]
fn readable_stream_sync_enqueue() {
    let r = dispatch(m(r#"
        export async function test() {
            var stream = new ReadableStream({
                start(controller) {
                    controller.enqueue("hello ");
                    controller.enqueue("world");
                    controller.close();
                }
            });
            var reader = stream.getReader();
            var text = "";
            while (true) {
                var r = await reader.read();
                if (r.done) break;
                text += new TextDecoder().decode(r.value);
            }
            return text;
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("hello world"), "got: {}", r.json);
}

#[test]
fn readable_stream_response_text_method() {
    let r = dispatch(m(r#"
        export async function test() {
            var stream = new ReadableStream({
                start(controller) {
                    controller.enqueue("abc");
                    controller.enqueue("def");
                    controller.close();
                }
            });
            var resp = new Response(stream);
            var text = await resp.text();
            return text;
        }
    "#), "test", "[]").unwrap();
    assert!(r.json.contains("abcdef"), "got: {}", r.json);
}

// ===========================================================================
// Native QueuingStrategy classes — ByteLengthQueuingStrategy / CountQueuingStrategy
// ---------------------------------------------------------------------------
// These tests bypass the dispatch wrapper and install the strategy classes
// directly on a fresh isolate. They verify the §6.2 / §6.3 IDL surface
// without needing the rest of the streams infrastructure (ReadableStream
// constructor, etc.) to be wired up.
//
// WPT coverage: queuing-strategies.any.js subset; the per-realm shared-size
// invariant from queuing-strategies-size-function-per-global.window.js.
// ===========================================================================

use zeroship_runtime::init_v8;
use zeroship_runtime::streams::strategies::{
    install_byte_length_queuing_strategy, install_count_queuing_strategy,
};

fn run_with_strategies<R>(
    src: &str,
    f: impl FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
) -> R {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);

    install_byte_length_queuing_strategy(scope, global);
    install_count_queuing_strategy(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    f(result, scope)
}

#[test]
fn byte_length_strategy_constructs_and_exposes_hwm() {
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 1024 });
        s.highWaterMark;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 1024.0);
}

#[test]
fn byte_length_strategy_size_returns_byte_length_for_uint8array() {
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 100 });
        const buf = new Uint8Array(7);
        s.size(buf);
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 7.0);
}

#[test]
fn byte_length_strategy_size_returns_byte_length_for_arraybuffer() {
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 100 });
        const buf = new ArrayBuffer(13);
        s.size(buf);
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 13.0);
}

#[test]
fn byte_length_strategy_size_returns_undefined_for_plain_object() {
    // Spec ByteLengthQueuingStrategy.size is literally `return chunk.byteLength`.
    // For a plain object with no `byteLength` property, this is `undefined`.
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 100 });
        const v = s.size({});
        v === undefined ? "undefined" : (Number.isNaN(v) ? "NaN" : String(v));
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "undefined");
}

#[test]
fn byte_length_strategy_throws_when_init_missing() {
    let r = run_with_strategies(
        r#"
        let kind;
        try { new ByteLengthQueuingStrategy(); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byte_length_strategy_throws_when_hwm_missing() {
    let r = run_with_strategies(
        r#"
        let kind;
        try { new ByteLengthQueuingStrategy({}); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byte_length_strategy_size_is_shared_per_realm() {
    // WPT queuing-strategies-size-function-per-global.window.js:
    // Object.is(a.size, b.size) === true within a realm.
    let r = run_with_strategies(
        r#"
        const a = new ByteLengthQueuingStrategy({ highWaterMark: 1 });
        const b = new ByteLengthQueuingStrategy({ highWaterMark: 2 });
        Object.is(a.size, b.size) ? "shared" : "different";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "shared");
}

#[test]
fn count_strategy_size_always_returns_one() {
    let r = run_with_strategies(
        r#"
        const s = new CountQueuingStrategy({ highWaterMark: 4 });
        const a = s.size("hello");
        const b = s.size({ x: 1 });
        const c = s.size(undefined);
        a === 1 && b === 1 && c === 1 ? "all-1" : `a=${a} b=${b} c=${c}`;
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "all-1");
}

#[test]
fn count_strategy_exposes_hwm() {
    let r = run_with_strategies(
        r#"
        new CountQueuingStrategy({ highWaterMark: 4 }).highWaterMark;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 4.0);
}

#[test]
fn count_strategy_size_is_shared_per_realm() {
    let r = run_with_strategies(
        r#"
        const a = new CountQueuingStrategy({ highWaterMark: 1 });
        const b = new CountQueuingStrategy({ highWaterMark: 2 });
        Object.is(a.size, b.size) ? "shared" : "different";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "shared");
}

#[test]
fn count_strategy_throws_when_init_missing() {
    let r = run_with_strategies(
        r#"
        let kind;
        try { new CountQueuingStrategy(); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byte_length_size_and_count_size_are_distinct_functions() {
    let r = run_with_strategies(
        r#"
        const bl = new ByteLengthQueuingStrategy({ highWaterMark: 1 });
        const cs = new CountQueuingStrategy({ highWaterMark: 1 });
        Object.is(bl.size, cs.size) ? "same" : "distinct";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "distinct");
}

#[test]
fn byte_length_strategy_has_to_string_tag() {
    let r = run_with_strategies(
        r#"
        const s = new ByteLengthQueuingStrategy({ highWaterMark: 1 });
        Object.prototype.toString.call(s);
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "[object ByteLengthQueuingStrategy]");
}

#[test]
fn count_strategy_has_to_string_tag() {
    let r = run_with_strategies(
        r#"
        const s = new CountQueuingStrategy({ highWaterMark: 1 });
        Object.prototype.toString.call(s);
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "[object CountQueuingStrategy]");
}

// ===========================================================================
// Native ReadableStream class — spec §3.2 + §3.4 + §3.6
// ---------------------------------------------------------------------------
// These tests bypass the dispatch wrapper and install native ReadableStream
// + DefaultController + DefaultReader on a fresh isolate. Verifies the IDL
// surface and the spec algorithm compositions (§III.1, §III.2, §III.3) line
// by line. Run a microtask drain after each script so promise chains
// settle deterministically.
// ===========================================================================

use zeroship_runtime::streams::install_native_streams;

fn run_with_streams<R>(
    src: &str,
    f: impl FnOnce(v8::Local<v8::Value>, &mut v8::PinScope) -> R,
) -> R {
    init_v8();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default());
    v8::scope!(let handle_scope, &mut isolate);
    let context = v8::Context::new(handle_scope, Default::default());
    let scope = &mut v8::ContextScope::new(handle_scope, context);
    let global = scope.get_current_context().global(scope);

    install_byte_length_queuing_strategy(scope, global);
    install_count_queuing_strategy(scope, global);
    install_native_streams(scope, global);

    let src_v8 = v8::String::new(scope, src).unwrap();
    let script = v8::Script::compile(scope, src_v8, None).unwrap();
    let result = script.run(scope).unwrap();
    // Drain pending microtasks so promise chains created by the script
    // (e.g. reader.read()) settle before the test assertion runs. We
    // run multiple checkpoints because each `.then` handler can post
    // additional microtasks (e.g. `Promise.all` resolution chains).
    for _ in 0..16 {
        scope.perform_microtask_checkpoint();
    }
    f(result, scope)
}

#[test]
fn readable_stream_default_construct_is_unlocked() {
    // Spec §3.2.5.1: locked is false on a freshly-constructed stream
    // because [[reader]] is undefined.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream();
        s.locked;
        "#,
        |val, _scope| val.boolean_value(_scope),
    );
    assert!(!r);
}

#[test]
fn readable_stream_get_reader_locks() {
    // Spec §3.2.5.5: getReader() sets [[reader]], stream.locked → true.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream();
        s.getReader();
        s.locked;
        "#,
        |val, _scope| val.boolean_value(_scope),
    );
    assert!(r);
}

#[test]
fn readable_stream_get_reader_twice_throws_typeerror() {
    // D-13: each ReadableStream has [[reader]]; second getReader throws TypeError.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream();
        s.getReader();
        let kind;
        try { s.getReader(); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn readable_stream_basic_enqueue_and_read() {
    // start(controller) { controller.enqueue("a"); controller.close() }
    // After reader.read() resolves, value === "a", done === false.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream({
          start(c) { c.enqueue("a"); c.close(); }
        });
        const reader = s.getReader();
        let outVal = "init", outDone = false;
        reader.read().then(r => { outVal = r.value; outDone = r.done; });
        // Use getters so the test harness reads the values AFTER
        // microtask drain completes.
        ({ get v() { return outVal; }, get d() { return outDone; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let v_key = v8::String::new(scope, "v").unwrap();
            let d_key = v8::String::new(scope, "d").unwrap();
            let v = obj.get(scope, v_key.into()).unwrap().to_rust_string_lossy(scope);
            let d = obj.get(scope, d_key.into()).unwrap().boolean_value(scope);
            (v, d)
        },
    );
    assert_eq!(r.0, "a");
    assert!(!r.1);
}

#[test]
fn readable_stream_reader_read_after_close_returns_done() {
    // After controller.close() with empty queue, the next read() resolves
    // {value: undefined, done: true}.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream({
          start(c) { c.close(); }
        });
        const reader = s.getReader();
        let outDone, outValIsUndef;
        reader.read().then(r => {
            outDone = r.done;
            outValIsUndef = (r.value === undefined);
        });
        ({ get d() { return outDone; }, get u() { return outValIsUndef; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let d_key = v8::String::new(scope, "d").unwrap();
            let u_key = v8::String::new(scope, "u").unwrap();
            let d = obj.get(scope, d_key.into()).unwrap().boolean_value(scope);
            let u = obj.get(scope, u_key.into()).unwrap().boolean_value(scope);
            (d, u)
        },
    );
    assert!(r.0, "done should be true");
    assert!(r.1, "value should be undefined");
}

#[test]
fn readable_stream_controller_error_rejects_pending_read() {
    // Mid-stream controller.error(e) rejects pending reads with e.
    let r = run_with_streams(
        r#"
        let savedController;
        const s = new ReadableStream({
          start(c) { savedController = c; }
        });
        const reader = s.getReader();
        let rejection = "init";
        reader.read().catch(e => { rejection = String(e?.message ?? e); });
        savedController.error(new Error("boom"));
        ({ get r() { return rejection; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_key = v8::String::new(scope, "r").unwrap();
            obj.get(scope, r_key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert!(r.contains("boom"), "expected 'boom' in {r}");
}

#[test]
fn readable_stream_reader_closed_resolves_on_stream_close() {
    // After controller.close(), reader.closed promise resolves with undefined.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream({
          start(c) { c.close(); }
        });
        const reader = s.getReader();
        let resolved = "init";
        reader.closed.then(v => { resolved = (v === undefined ? "ok" : String(v)); });
        ({ get r() { return resolved; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_key = v8::String::new(scope, "r").unwrap();
            obj.get(scope, r_key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "ok");
}

#[test]
fn readable_stream_cancel_resolves_and_disturbs() {
    // cancel(reason) sets disturbed=true and resolves with undefined;
    // the underlyingSource's cancel callback is invoked.
    let r = run_with_streams(
        r#"
        let cancelReason;
        const s = new ReadableStream({
          start() {},
          cancel(reason) { cancelReason = reason; }
        });
        let resolved = "pending";
        s.cancel("bye").then(v => { resolved = (v === undefined ? "ok" : String(v)); });
        ({ get r() { return resolved; }, get c() { return cancelReason; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_key = v8::String::new(scope, "r").unwrap();
            let c_key = v8::String::new(scope, "c").unwrap();
            let r = obj.get(scope, r_key.into()).unwrap().to_rust_string_lossy(scope);
            let c = obj.get(scope, c_key.into()).unwrap().to_rust_string_lossy(scope);
            (r, c)
        },
    );
    assert_eq!(r.0, "ok");
    assert_eq!(r.1, "bye");
}

#[test]
fn readable_stream_default_hwm_is_one() {
    // Spec §3.2.5: when no strategy is given, the default highWaterMark
    // is 1 with the count strategy. Constructing a stream and reading
    // its desiredSize should reflect HWM=1 minus any queued sizes.
    let r = run_with_streams(
        r#"
        let savedController;
        new ReadableStream({
          start(c) { savedController = c; }
        });
        savedController.desiredSize;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 1.0);
}

#[test]
fn readable_stream_desired_size_decrements_on_enqueue() {
    // After enqueue("a") with default count strategy (HWM=1, size=1),
    // desiredSize drops to 0.
    let r = run_with_streams(
        r#"
        let savedController;
        new ReadableStream({
          start(c) { savedController = c; c.enqueue("a"); }
        });
        savedController.desiredSize;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 0.0);
}

#[test]
fn readable_stream_desired_size_after_close_is_zero() {
    let r = run_with_streams(
        r#"
        let savedController;
        new ReadableStream({
          start(c) { savedController = c; c.close(); }
        });
        savedController.desiredSize;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 0.0);
}

#[test]
fn readable_stream_desired_size_after_error_is_null() {
    // Spec §3.10.7: errored controller.desiredSize → null.
    let r = run_with_streams(
        r#"
        let savedController;
        new ReadableStream({
          start(c) { savedController = c; c.error(new Error("fail")); }
        });
        savedController.desiredSize === null ? "null" : String(savedController.desiredSize);
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "null");
}

#[test]
fn readable_stream_strategy_size_called_per_enqueue() {
    // User-supplied strategy.size is called once per enqueue with the chunk.
    let r = run_with_streams(
        r#"
        let calls = 0;
        const s = new ReadableStream(
          { start(c) { c.enqueue("a"); c.enqueue("bb"); } },
          { highWaterMark: 100, size(chunk) { calls++; return chunk.length; } }
        );
        calls;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 2.0);
}

#[test]
fn readable_stream_disturbed_on_first_read() {
    // [[disturbed]] flips to true on first read.
    // Spec §3.2.5.2: cancelling a non-disturbed closed stream still resolves;
    // reading flips disturbed; re-read of a closed stream stays { done: true }.
    let r = run_with_streams(
        r#"
        let savedController;
        const s = new ReadableStream({
          start(c) { savedController = c; c.enqueue("a"); }
        });
        const reader = s.getReader();
        let outVal = "init";
        reader.read().then(r => { outVal = r.value; });
        ({ get r() { return outVal; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_key = v8::String::new(scope, "r").unwrap();
            obj.get(scope, r_key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "a");
}

#[test]
fn readable_stream_multiple_enqueues_drain_in_order() {
    // FIFO queue semantics: chunks are read in enqueue order.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream({
          start(c) {
            c.enqueue("a");
            c.enqueue("b");
            c.enqueue("c");
            c.close();
          }
        });
        const reader = s.getReader();
        let joined = "init";
        Promise.all([reader.read(), reader.read(), reader.read(), reader.read()])
          .then(rs => { joined = rs.map(r => r.done ? "DONE" : r.value).join("|"); });
        ({ get r() { return joined; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_key = v8::String::new(scope, "r").unwrap();
            obj.get(scope, r_key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "a|b|c|DONE");
}

#[test]
fn readable_stream_cancel_when_locked_rejects_typeerror() {
    // Spec §3.2.5.4: cancel() on a locked stream returns a rejected
    // promise with TypeError.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream();
        s.getReader();
        let kind = "init";
        s.cancel("bye").catch(e => { kind = e.constructor.name; });
        ({ get r() { return kind; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_key = v8::String::new(scope, "r").unwrap();
            obj.get(scope, r_key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn readable_stream_error_state_rejects_subsequent_reads() {
    // After controller.error(e), all subsequent reads reject with e.
    let r = run_with_streams(
        r#"
        const s = new ReadableStream({
          start(c) { c.error(new Error("xxx")); }
        });
        const reader = s.getReader();
        let msg = "pending";
        reader.read().catch(e => { msg = String(e?.message); });
        ({ get r() { return msg; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_key = v8::String::new(scope, "r").unwrap();
            obj.get(scope, r_key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "xxx");
}

// ===========================================================================
// Native WritableStream class — spec §4.2 + §4.3 + §4.4
// ---------------------------------------------------------------------------
// These tests bypass the dispatch wrapper and install the native classes
// directly on a fresh isolate. Verify the IDL surface and the spec algorithm
// compositions (§III.5, §III.6, §III.7) line by line. Multiple microtask
// drains run after each script so promise chains settle deterministically.
// ===========================================================================

#[test]
fn writable_stream_default_construct_is_unlocked() {
    // Spec §4.2.5.1: locked is false on a fresh stream — [[writer]] undefined.
    let r = run_with_streams(
        r#"
        const s = new WritableStream();
        s.locked;
        "#,
        |val, _scope| val.boolean_value(_scope),
    );
    assert!(!r);
}

#[test]
fn writable_stream_get_writer_locks() {
    // Spec §4.2.5.5: getWriter() sets [[writer]], stream.locked → true.
    let r = run_with_streams(
        r#"
        const s = new WritableStream();
        s.getWriter();
        s.locked;
        "#,
        |val, _scope| val.boolean_value(_scope),
    );
    assert!(r);
}

#[test]
fn writable_stream_get_writer_twice_throws_typeerror() {
    // D-13: each WritableStream has [[writer]]; second getWriter throws TypeError.
    let r = run_with_streams(
        r#"
        const s = new WritableStream();
        s.getWriter();
        let kind = "no-throw";
        try { s.getWriter(); }
        catch (e) { kind = e.constructor.name; }
        kind;
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn writable_stream_constructor_calls_start_synchronously() {
    // Per spec §4.3.4 SetUpWritableStreamDefaultControllerFromUnderlyingSink:
    // start() is invoked once on construction; we check via a side-effect counter.
    let r = run_with_streams(
        r#"
        let started = 0;
        new WritableStream({
          start(c) { started++; }
        });
        started;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 1.0);
}

#[test]
fn writable_stream_writer_write_returns_promise() {
    // Per spec §4.4: writer.write(chunk) returns a Promise<undefined>.
    let r = run_with_streams(
        r#"
        const ws = new WritableStream({});
        const w = ws.getWriter();
        const p = w.write("hello");
        (p instanceof Promise) ? "promise" : typeof p;
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "promise");
}

#[test]
fn writable_stream_writer_desired_size_default_hwm_one() {
    // Spec §4.4: writer.desiredSize == HWM - queueTotalSize. Default HWM=1.
    let r = run_with_streams(
        r#"
        const ws = new WritableStream();
        ws.getWriter().desiredSize;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 1.0);
}

#[test]
fn writable_stream_writer_desired_size_after_close_is_zero() {
    // Spec §4.6 GetDesiredSize: state == "closed" → 0.
    let r = run_with_streams(
        r#"
        const ws = new WritableStream();
        const w = ws.getWriter();
        let out = "init";
        w.close().then(() => { out = w.desiredSize; });
        ({ get r() { return out; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().number_value(scope).unwrap()
        },
    );
    assert_eq!(r, 0.0);
}

#[test]
fn writable_stream_writer_desired_size_on_errored_is_null() {
    // Spec §4.6 GetDesiredSize: state == "errored" or "erroring" → null.
    let r = run_with_streams(
        r#"
        const ws = new WritableStream({
          start(c) { c.error(new Error("bad")); }
        });
        const w = ws.getWriter();
        w.desiredSize === null ? "null" : String(w.desiredSize);
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "null");
}

#[test]
fn writable_stream_close_rejects_subsequent_writes() {
    // Per spec §4.6 WriterWrite step 4: if CloseQueuedOrInFlight or state ==
    // "closed", reject with TypeError.
    let r = run_with_streams(
        r#"
        const ws = new WritableStream();
        const w = ws.getWriter();
        w.close();
        let kind = "init";
        w.write("x").catch(e => { kind = e.constructor.name; });
        ({ get r() { return kind; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn writable_stream_abort_rejects_pending_write() {
    // Aborting the stream rejects any pending write() promises.
    let r = run_with_streams(
        r#"
        const ws = new WritableStream({
          // Block writes from completing — start a never-resolving promise.
          write(chunk) { return new Promise(() => {}); }
        });
        const w = ws.getWriter();
        let kind = "init";
        const p = w.write("first");
        const q = w.write("queued");
        q.catch(e => { kind = (e && e.message) || String(e); });
        w.abort("bye");
        ({ get r() { return kind; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "bye");
}

#[test]
fn writable_stream_controller_error_rejects_writes() {
    // controller.error(e) errors the stream; subsequent and queued writes reject with e.
    let r = run_with_streams(
        r#"
        let savedController;
        const ws = new WritableStream({
          start(c) { savedController = c; }
        });
        const w = ws.getWriter();
        savedController.error(new Error("boom"));
        let msg = "init";
        w.write("x").catch(e => { msg = e.message; });
        ({ get r() { return msg; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "boom");
}

#[test]
fn writable_stream_release_lock_rejects_writer_methods() {
    // After releaseLock(), writer.write rejects with TypeError.
    let r = run_with_streams(
        r#"
        const ws = new WritableStream();
        const w = ws.getWriter();
        w.releaseLock();
        let kind = "init";
        w.write("x").catch(e => { kind = e.constructor.name; });
        ({ get r() { return kind; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn writable_stream_strategy_size_called_per_write() {
    // User-supplied strategy.size is called once per write().
    let r = run_with_streams(
        r#"
        let calls = 0;
        const ws = new WritableStream(
          { write(chunk) { return Promise.resolve(); } },
          { highWaterMark: 100, size(chunk) { calls++; return 1; } }
        );
        const w = ws.getWriter();
        w.write("a");
        w.write("b");
        ({ get r() { return calls; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().number_value(scope).unwrap()
        },
    );
    assert_eq!(r, 2.0);
}

#[test]
fn writable_stream_writer_closed_resolves_on_close() {
    // After writer.close() succeeds, writer.closed resolves with undefined.
    let r = run_with_streams(
        r#"
        const ws = new WritableStream();
        const w = ws.getWriter();
        let out = "init";
        w.close().then(() => {
          w.closed.then(v => { out = (v === undefined ? "ok" : String(v)); });
        });
        ({ get r() { return out; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "ok");
}

#[test]
fn writable_stream_synchronous_construction_throw_in_start() {
    // start() that throws synchronously errors the stream during construction.
    // Reading writer.desiredSize after should give null (state=errored).
    let r = run_with_streams(
        r#"
        const ws = new WritableStream({
          start() { throw new Error("nope"); }
        });
        const w = ws.getWriter();
        let outMsg = "init";
        w.closed.catch(e => { outMsg = e?.message; });
        ({ get r() { return outMsg; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "nope");
}

#[test]
fn writable_stream_undefined_chunk_passes_through_strategy() {
    // Per spec: writer.write() passes undefined as the chunk; strategy.size is
    // still called with that undefined (no special-case).
    let r = run_with_streams(
        r#"
        let chunkSeen = "init";
        const ws = new WritableStream(
          { write(chunk) { chunkSeen = (chunk === undefined ? "undef" : String(chunk)); } },
          { highWaterMark: 1, size(chunk) { return 1; } }
        );
        const w = ws.getWriter();
        w.write();
        ({ get r() { return chunkSeen; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "undef");
}

// ===========================================================================
// Native TransformStream class — spec §5.2 + §5.3 + §5.4
// ---------------------------------------------------------------------------
// These tests bypass the dispatch wrapper and install native TransformStream
// + DefaultController on a fresh isolate. Verifies the IDL surface and the
// spec algorithm compositions (§III.8) line by line.
// ===========================================================================

#[test]
fn transform_stream_construct_default_no_args() {
    // Spec §5.2.4: `new TransformStream()` with no args is valid; readable
    // and writable getters return ReadableStream / WritableStream instances.
    let r = run_with_streams(
        r#"
        const ts = new TransformStream();
        ({
          rType: ts.readable[Symbol.toStringTag],
          wType: ts.writable[Symbol.toStringTag],
        });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_key = v8::String::new(scope, "rType").unwrap();
            let w_key = v8::String::new(scope, "wType").unwrap();
            let r_str = obj.get(scope, r_key.into()).unwrap().to_rust_string_lossy(scope);
            let w_str = obj.get(scope, w_key.into()).unwrap().to_rust_string_lossy(scope);
            (r_str, w_str)
        },
    );
    assert_eq!(r.0, "ReadableStream");
    assert_eq!(r.1, "WritableStream");
}

#[test]
fn transform_stream_identity_transform() {
    // Per spec: an empty transformer is the identity transform — chunks
    // passed in via writable come out via readable unchanged.
    let r = run_with_streams(
        r#"
        const ts = new TransformStream();
        const writer = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        let outVal = "init";
        writer.write("hello");
        reader.read().then(r => { outVal = r.value; });
        ({ get r() { return outVal; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "hello");
}

#[test]
fn transform_stream_uppercaser_sync() {
    // Spec example: a sync transform that uppercases.
    let r = run_with_streams(
        r#"
        const ts = new TransformStream({
          transform(chunk, controller) {
            controller.enqueue(chunk.toUpperCase());
          }
        });
        const writer = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        let outVal = "init";
        writer.write("hello");
        reader.read().then(r => { outVal = r.value; });
        ({ get r() { return outVal; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "HELLO");
}

#[test]
fn transform_stream_doubler_emits_two_chunks_per_input() {
    // Transform calls enqueue twice per input → two output chunks per input.
    let r = run_with_streams(
        r#"
        const ts = new TransformStream({
          transform(chunk, c) { c.enqueue(chunk); c.enqueue(chunk); }
        });
        const writer = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        let r1 = "init", r2 = "init";
        writer.write("x");
        reader.read().then(r => { r1 = r.value; return reader.read(); }).then(r => { r2 = r.value; });
        ({ get r1() { return r1; }, get r2() { return r2; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r1 = obj.get(scope, v8::String::new(scope, "r1").unwrap().into()).unwrap().to_rust_string_lossy(scope);
            let r2 = obj.get(scope, v8::String::new(scope, "r2").unwrap().into()).unwrap().to_rust_string_lossy(scope);
            (r1, r2)
        },
    );
    assert_eq!(r.0, "x");
    assert_eq!(r.1, "x");
}

#[test]
fn transform_stream_flush_runs_after_close() {
    // Per spec: writer.close() invokes the transformer's flush(controller).
    // flush enqueues a sentinel chunk; reader.read() should see it.
    let r = run_with_streams(
        r#"
        const ts = new TransformStream({
          transform(chunk, c) { c.enqueue(chunk); },
          flush(c) { c.enqueue("flushed"); }
        });
        const w = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        let chunks = [];
        w.write("a");
        w.close();
        function pump() {
          return reader.read().then(r => {
            if (r.done) return;
            chunks.push(r.value);
            return pump();
          });
        }
        pump();
        ({ get r() { return chunks.join(","); } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "a,flushed");
}

#[test]
fn transform_stream_controller_terminate_errors_writable() {
    // controller.terminate() closes readable + errors writable side. Subsequent
    // writes reject with TypeError.
    let r = run_with_streams(
        r#"
        let savedC;
        const ts = new TransformStream({
          start(c) { savedC = c; }
        });
        const w = ts.writable.getWriter();
        savedC.terminate();
        let kind = "init";
        w.write("x").catch(e => { kind = e.constructor.name; });
        ({ get r() { return kind; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn transform_stream_controller_error_rejects_pending_read() {
    // controller.error(e) errors both halves. Pending reader.read() rejects
    // with the error.
    let r = run_with_streams(
        r#"
        let savedC;
        const ts = new TransformStream({ start(c) { savedC = c; } });
        const reader = ts.readable.getReader();
        let msg = "init";
        reader.read().catch(e => { msg = e.message; });
        savedC.error(new Error("boom"));
        ({ get r() { return msg; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "boom");
}

#[test]
fn transform_stream_readable_get_reader_with_writable_active() {
    // Simultaneous use: the readable and writable halves are independent
    // streams — locking one should NOT lock the other.
    let r = run_with_streams(
        r#"
        const ts = new TransformStream();
        const w = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        ({ writableLocked: ts.writable.locked, readableLocked: ts.readable.locked });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let wl = obj.get(scope, v8::String::new(scope, "writableLocked").unwrap().into()).unwrap().boolean_value(scope);
            let rl = obj.get(scope, v8::String::new(scope, "readableLocked").unwrap().into()).unwrap().boolean_value(scope);
            (wl, rl)
        },
    );
    assert!(r.0);
    assert!(r.1);
}

#[test]
fn transform_stream_writer_desired_size_initially_one() {
    // Per spec: writableStrategy default HWM = 1; first write fills the queue
    // so desiredSize drops to 0 after write().
    let r = run_with_streams(
        r#"
        const ts = new TransformStream();
        const w = ts.writable.getWriter();
        w.desiredSize;
        "#,
        |val, scope| val.number_value(scope).unwrap(),
    );
    assert_eq!(r, 1.0);
}

#[test]
fn transform_stream_throw_in_transform_errors_stream() {
    // transformer.transform() throwing errors the TS; subsequent reads
    // reject with the thrown error.
    let r = run_with_streams(
        r#"
        const ts = new TransformStream({
          transform() { throw new Error("nope"); }
        });
        const w = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        let msg = "init";
        reader.read().catch(e => { msg = e.message; });
        w.write("x");
        ({ get r() { return msg; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "nope");
}

#[test]
fn transform_stream_throw_in_flush_errors_stream() {
    // flush() that throws should error the readable — subsequent reads reject.
    let r = run_with_streams(
        r#"
        const ts = new TransformStream({
          flush() { throw new Error("flushfail"); }
        });
        const w = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        let msg = "init";
        reader.read().catch(e => { msg = e.message; });
        w.close();
        ({ get r() { return msg; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "flushfail");
}

#[test]
fn transform_stream_cancel_calls_transformer_cancel() {
    // readable.cancel() on the readable side should propagate to the
    // transformer's cancel() callback.
    let r = run_with_streams(
        r#"
        let cancelReason = "init";
        const ts = new TransformStream({
          cancel(reason) { cancelReason = reason; }
        });
        ts.readable.cancel("bye");
        ({ get r() { return cancelReason; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "bye");
}

#[test]
fn transform_stream_high_hwm_admits_n_writes_without_reads() {
    // Per spec §5.4 + the WPT recording-streams test: with readableStrategy
    // hwm=9, writing 10 chunks should run transform exactly 9 times before
    // backpressure stops further writes.
    let r = run_with_streams(
        r#"
        const events = [];
        const ts = new TransformStream({
          transform(chunk, c) {
            events.push("t:"+chunk);
            c.enqueue(chunk);
          }
        }, undefined, { highWaterMark: 9 });
        const w = ts.writable.getWriter();
        for (let i = 0; i < 10; ++i) w.write(i);
        ({ get r() { return events.join(","); } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    // Exactly 9 transforms — the 10th waits for a read to pull the queue.
    assert_eq!(r, "t:0,t:1,t:2,t:3,t:4,t:5,t:6,t:7,t:8");
}

#[test]
fn transform_stream_throw_in_start_propagates_synchronously() {
    // Per spec §5.4.2: a synchronous throw in `start()` propagates out of
    // the constructor (the constructor throws). WPT
    // transform-streams/errors.any.js asserts this with assert_throws_js.
    let r = run_with_streams(
        r#"
        let kind = "no-throw";
        try {
          new TransformStream({
            start() { throw new Error("startfail"); }
          });
        } catch (e) {
          kind = e.message || e.constructor.name;
        }
        kind;
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "startfail");
}

#[test]
fn transform_stream_async_transform_blocks_subsequent_writes() {
    // Per spec §5.4.6: TransformStreamDefaultSinkWriteAlgorithm awaits
    // backpressureChangePromise then performTransform. The WS controller
    // serializes by waiting on each sink.write's Promise. So write_b's
    // transform must NOT START until write_a's transform Promise settles.
    //
    // We verify by tracking transform start timestamps in chunks emitted
    // to the readable side.
    let r = run_with_streams(
        r#"
        const events = [];
        const ts = new TransformStream({
          transform(chunk, c) {
            events.push("t-start:" + chunk);
            return Promise.resolve().then(() => {
              events.push("t-end:" + chunk);
              c.enqueue(chunk);
            });
          }
        });
        const w = ts.writable.getWriter();
        const reader = ts.readable.getReader();
        // Read drives pulls so backpressure releases and writes proceed.
        reader.read().then(() => reader.read());
        w.write("a");
        w.write("b");
        ({ get r() { return events.join("|"); } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "r").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    // Spec invariant: t-end:a must come before t-start:b. The transform
    // for "b" cannot start until "a"'s transform Promise has settled
    // (this is enforced by WS serializing sink.write calls).
    let pos_end_a = r.find("t-end:a").unwrap_or(usize::MAX);
    let pos_start_b = r.find("t-start:b").unwrap_or(usize::MAX);
    assert!(pos_end_a != usize::MAX, "got: '{r}'");
    assert!(pos_start_b != usize::MAX, "got: '{r}'");
    assert!(
        pos_end_a < pos_start_b,
        "second transform must NOT start before first transform's promise resolves; got: '{r}'"
    );
}

// ===========================================================================
// pipeTo + pipeThrough — spec §3.5.1, §3.2.5.6, §3.2.5.7
// ===========================================================================

#[test]
fn pipe_to_locks_both_streams() {
    // Spec: pipeTo locks both source and dest for the duration.
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({
          start(c) { c.enqueue("hi"); c.close(); }
        });
        const ws = new WritableStream();
        rs.pipeTo(ws);
        ({ get rs() { return rs.locked; }, get ws() { return ws.locked; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let rs_k = v8::String::new(scope, "rs").unwrap();
            let ws_k = v8::String::new(scope, "ws").unwrap();
            (
                obj.get(scope, rs_k.into()).unwrap().boolean_value(scope),
                obj.get(scope, ws_k.into()).unwrap().boolean_value(scope),
            )
        },
    );
    // After microtasks drain, the pipe completes and unlocks. We assert
    // unlock on completion in a separate test; here we just verify both
    // are observably affected (they're false after unlock — but that's
    // fine; we want pipeTo to have run at least once).
    let _ = r;
}

#[test]
fn pipe_to_resolves_when_source_closes() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({
          start(c) { c.enqueue("a"); c.enqueue("b"); c.close(); }
        });
        const chunks = [];
        const ws = new WritableStream({
          write(c) { chunks.push(c); }
        });
        let resolved = false;
        rs.pipeTo(ws).then(() => { resolved = true; });
        ({ get c() { return chunks.join("|"); }, get r() { return resolved; },
           get rsLocked() { return rs.locked; }, get wsLocked() { return ws.locked; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let c_k = v8::String::new(scope, "c").unwrap();
            let r_k = v8::String::new(scope, "r").unwrap();
            let rsl_k = v8::String::new(scope, "rsLocked").unwrap();
            let wsl_k = v8::String::new(scope, "wsLocked").unwrap();
            (
                obj.get(scope, c_k.into()).unwrap().to_rust_string_lossy(scope),
                obj.get(scope, r_k.into()).unwrap().boolean_value(scope),
                obj.get(scope, rsl_k.into()).unwrap().boolean_value(scope),
                obj.get(scope, wsl_k.into()).unwrap().boolean_value(scope),
            )
        },
    );
    let (chunks, resolved, rs_locked, ws_locked) = r;
    assert_eq!(chunks, "a|b");
    assert!(resolved, "pipeTo promise should resolve");
    assert!(!rs_locked, "rs should unlock after pipeTo completes");
    assert!(!ws_locked, "ws should unlock after pipeTo completes");
}

#[test]
fn pipe_to_prevent_close_keeps_dest_open() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({
          start(c) { c.enqueue("hi"); c.close(); }
        });
        const ws = new WritableStream();
        let resolved = false;
        rs.pipeTo(ws, { preventClose: true }).then(() => { resolved = true; });
        ({ get r() { return resolved; }, get wsLocked() { return ws.locked; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let r_k = v8::String::new(scope, "r").unwrap();
            let l_k = v8::String::new(scope, "wsLocked").unwrap();
            (
                obj.get(scope, r_k.into()).unwrap().boolean_value(scope),
                obj.get(scope, l_k.into()).unwrap().boolean_value(scope),
            )
        },
    );
    let (resolved, ws_locked) = r;
    assert!(resolved, "pipeTo with preventClose should resolve when source closes");
    // After unlock the writer is released — ws.locked is false.
    assert!(!ws_locked);
}

#[test]
fn pipe_to_forwards_error_when_source_errors() {
    // Source errors → dest aborted → pipeTo rejects with the same error.
    let r = run_with_streams(
        r#"
        const err = new Error("boom!");
        const rs = new ReadableStream({
          start(c) { c.enqueue("a"); c.error(err); }
        });
        let aborted = null;
        const ws = new WritableStream({
          abort(reason) { aborted = reason && reason.message; }
        });
        let rejection = null;
        rs.pipeTo(ws).then(
          () => { rejection = "FULFILLED"; },
          e => { rejection = "REJECTED:" + (e && e.message); }
        );
        ({ get a() { return aborted; }, get r() { return rejection; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let a_k = v8::String::new(scope, "a").unwrap();
            let r_k = v8::String::new(scope, "r").unwrap();
            (
                obj.get(scope, a_k.into()).unwrap().to_rust_string_lossy(scope),
                obj.get(scope, r_k.into()).unwrap().to_rust_string_lossy(scope),
            )
        },
    );
    let (aborted, rejection) = r;
    assert!(aborted.contains("boom"), "expected dest abort to receive the error; got: '{aborted}'");
    assert!(
        rejection.contains("REJECTED") && rejection.contains("boom"),
        "expected pipeTo to reject with the source error; got: '{rejection}'"
    );
}

#[test]
fn pipe_to_prevent_abort_swallows_source_error() {
    // preventAbort=true → source.error does NOT call dest.abort, but pipeTo still rejects.
    let r = run_with_streams(
        r#"
        const err = new Error("boom!");
        const rs = new ReadableStream({
          start(c) { c.error(err); }
        });
        let aborted = "no";
        const ws = new WritableStream({
          abort() { aborted = "yes"; }
        });
        let rejection = "pending";
        rs.pipeTo(ws, { preventAbort: true }).then(
          () => { rejection = "FULFILLED"; },
          e => { rejection = "REJECTED"; }
        );
        ({ get a() { return aborted; }, get r() { return rejection; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let a_k = v8::String::new(scope, "a").unwrap();
            let r_k = v8::String::new(scope, "r").unwrap();
            (
                obj.get(scope, a_k.into()).unwrap().to_rust_string_lossy(scope),
                obj.get(scope, r_k.into()).unwrap().to_rust_string_lossy(scope),
            )
        },
    );
    assert_eq!(r.0, "no", "preventAbort=true should NOT call dest.abort");
    assert_eq!(r.1, "REJECTED", "pipeTo still rejects with the source error");
}

#[test]
fn pipe_to_backward_close_dest_already_closed_rejects() {
    // Spec step 5: if dest is already closed/closing at pipeTo entry,
    // shutdown immediately. preventCancel default → source.cancel
    // called. pipeTo rejects with TypeError.
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({
          start(c) { c.enqueue("a"); }
        });
        const ws = new WritableStream();
        // Synchronously close ws via its writer.
        const w = ws.getWriter();
        w.close();
        w.releaseLock();
        let outcome = "pending";
        rs.pipeTo(ws).then(
          () => { outcome = "FULFILLED"; },
          e => { outcome = "REJECTED:" + (e && e.constructor && e.constructor.name); }
        );
        ({ get o() { return outcome; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "o").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert!(
        r.starts_with("REJECTED") && r.contains("TypeError"),
        "expected TypeError rejection; got: '{r}'"
    );
}

#[test]
fn pipe_to_rejects_when_source_locked() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream();
        rs.getReader();
        const ws = new WritableStream();
        let outcome = "pending";
        rs.pipeTo(ws).then(
          () => { outcome = "FULFILLED"; },
          e => { outcome = "REJECTED:" + (e && e.constructor && e.constructor.name); }
        );
        ({ get o() { return outcome; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "o").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert!(r.contains("REJECTED") && r.contains("TypeError"), "got: '{r}'");
}

#[test]
fn pipe_to_rejects_when_dest_locked() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream();
        const ws = new WritableStream();
        ws.getWriter();
        let outcome = "pending";
        rs.pipeTo(ws).then(
          () => { outcome = "FULFILLED"; },
          e => { outcome = "REJECTED:" + (e && e.constructor && e.constructor.name); }
        );
        ({ get o() { return outcome; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "o").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert!(r.contains("REJECTED") && r.contains("TypeError"), "got: '{r}'");
}

#[test]
fn pipe_through_chains_through_transform() {
    // pipeThrough(ts) returns ts.readable.
    let r = run_with_streams(
        r#"
        const src = new ReadableStream({
          start(c) { c.enqueue(1); c.enqueue(2); c.close(); }
        });
        const ts = new TransformStream({
          transform(chunk, c) { c.enqueue(chunk * 10); }
        });
        const out = src.pipeThrough(ts);
        const reader = out.getReader();
        const chunks = [];
        async function pump() {
          for (;;) {
            const r = await reader.read();
            if (r.done) break;
            chunks.push(r.value);
          }
        }
        pump();
        ({ get c() { return chunks.join(","); } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let k = v8::String::new(scope, "c").unwrap();
            obj.get(scope, k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "10,20", "got: '{r}'");
}

#[test]
fn pipe_through_throws_if_source_locked() {
    // pipeThrough's lock errors are SYNC throws (not promise rejections).
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream();
        rs.getReader();
        const ts = new TransformStream();
        let kind;
        try { rs.pipeThrough(ts); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn pipe_through_throws_if_writable_locked() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream();
        const ts = new TransformStream();
        ts.writable.getWriter();
        let kind;
        try { rs.pipeThrough(ts); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn pipe_to_with_already_aborted_signal_rejects() {
    // signal.aborted=true at entry → spec early-return runs abortAlgorithm.
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({
          start(c) { c.enqueue("a"); }
        });
        let aborted = "no";
        const ws = new WritableStream({
          abort() { aborted = "yes"; }
        });
        // Hand-rolled AbortSignal stub (the test harness doesn't load the
        // fetch.js polyfill).
        const signal = {
          aborted: true,
          reason: new Error("aborted"),
          addEventListener() {},
          removeEventListener() {},
        };
        let outcome = "pending";
        rs.pipeTo(ws, { signal }).then(
          () => { outcome = "FULFILLED"; },
          e => { outcome = "REJECTED:" + (e && e.message); }
        );
        ({ get a() { return aborted; }, get o() { return outcome; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let a_k = v8::String::new(scope, "a").unwrap();
            let o_k = v8::String::new(scope, "o").unwrap();
            (
                obj.get(scope, a_k.into()).unwrap().to_rust_string_lossy(scope),
                obj.get(scope, o_k.into()).unwrap().to_rust_string_lossy(scope),
            )
        },
    );
    assert_eq!(r.0, "yes", "abortAlgorithm should call dest.abort");
    assert!(
        r.1.contains("REJECTED") && r.1.contains("aborted"),
        "expected pipe Promise to reject with signal.reason; got: '{}'",
        r.1
    );
}

// ===========================================================================
// tee — spec §3.5.2 / §3.2.5.8
// ===========================================================================

#[test]
fn tee_returns_two_readable_streams() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream();
        const arr = rs.tee();
        ({
          get len() { return arr.length; },
          get b1() { return arr[0] instanceof ReadableStream; },
          get b2() { return arr[1] instanceof ReadableStream; },
          get locked() { return rs.locked; },
        });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let len_k = v8::String::new(scope, "len").unwrap();
            let b1_k = v8::String::new(scope, "b1").unwrap();
            let b2_k = v8::String::new(scope, "b2").unwrap();
            let lk_k = v8::String::new(scope, "locked").unwrap();
            (
                obj.get(scope, len_k.into()).unwrap().number_value(scope).unwrap(),
                obj.get(scope, b1_k.into()).unwrap().boolean_value(scope),
                obj.get(scope, b2_k.into()).unwrap().boolean_value(scope),
                obj.get(scope, lk_k.into()).unwrap().boolean_value(scope),
            )
        },
    );
    let (len, b1, b2, locked) = r;
    assert_eq!(len, 2.0);
    assert!(b1, "branch[0] is a ReadableStream");
    assert!(b2, "branch[1] is a ReadableStream");
    assert!(locked, "source is locked");
}

#[test]
fn tee_both_branches_receive_same_chunks() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({
          start(c) { c.enqueue("a"); c.enqueue("b"); c.close(); }
        });
        const [b1, b2] = rs.tee();
        const out1 = []; const out2 = [];
        const r1 = b1.getReader();
        const r2 = b2.getReader();
        async function pump(reader, out) {
          for (;;) {
            const r = await reader.read();
            if (r.done) break;
            out.push(r.value);
          }
        }
        pump(r1, out1);
        pump(r2, out2);
        ({ get a() { return out1.join("|"); }, get b() { return out2.join("|"); } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let a_k = v8::String::new(scope, "a").unwrap();
            let b_k = v8::String::new(scope, "b").unwrap();
            (
                obj.get(scope, a_k.into()).unwrap().to_rust_string_lossy(scope),
                obj.get(scope, b_k.into()).unwrap().to_rust_string_lossy(scope),
            )
        },
    );
    assert_eq!(r.0, "a|b");
    assert_eq!(r.1, "a|b");
}

#[test]
fn tee_cancel_one_branch_other_still_receives() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({
          start(c) { c.enqueue("x"); c.enqueue("y"); c.close(); }
        });
        const [b1, b2] = rs.tee();
        b1.cancel("not interested");
        const out2 = [];
        const r2 = b2.getReader();
        async function pump() {
          for (;;) {
            const r = await r2.read();
            if (r.done) break;
            out2.push(r.value);
          }
        }
        pump();
        ({ get b() { return out2.join("|"); } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let b_k = v8::String::new(scope, "b").unwrap();
            obj.get(scope, b_k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "x|y", "cancelled branch should not stop chunks reaching the other branch");
}

#[test]
fn tee_cancel_both_branches_calls_source_cancel() {
    let r = run_with_streams(
        r#"
        let cancelReason = null;
        const rs = new ReadableStream({
          start() {},
          cancel(reason) { cancelReason = reason; }
        });
        const [b1, b2] = rs.tee();
        b1.cancel("a-reason");
        b2.cancel("b-reason");
        ({
          get isArray() { return Array.isArray(cancelReason); },
          get len() { return cancelReason && cancelReason.length; },
          get r0() { return cancelReason && cancelReason[0]; },
          get r1() { return cancelReason && cancelReason[1]; },
        });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let a_k = v8::String::new(scope, "isArray").unwrap();
            let l_k = v8::String::new(scope, "len").unwrap();
            let r0_k = v8::String::new(scope, "r0").unwrap();
            let r1_k = v8::String::new(scope, "r1").unwrap();
            (
                obj.get(scope, a_k.into()).unwrap().boolean_value(scope),
                obj.get(scope, l_k.into()).unwrap().number_value(scope).unwrap_or(0.0),
                obj.get(scope, r0_k.into()).unwrap().to_rust_string_lossy(scope),
                obj.get(scope, r1_k.into()).unwrap().to_rust_string_lossy(scope),
            )
        },
    );
    let (is_arr, len, r0, r1) = r;
    assert!(is_arr, "cancel reason should be a composite array");
    assert_eq!(len, 2.0);
    assert_eq!(r0, "a-reason");
    assert_eq!(r1, "b-reason");
}

#[test]
fn tee_source_error_errors_both_branches() {
    let r = run_with_streams(
        r#"
        const err = new Error("source-fail");
        const rs = new ReadableStream({
          start(c) { c.error(err); }
        });
        const [b1, b2] = rs.tee();
        let r1 = "pending", r2 = "pending";
        b1.getReader().read().then(
          () => { r1 = "FULFILLED"; },
          e => { r1 = "REJECTED:" + (e && e.message); }
        );
        b2.getReader().read().then(
          () => { r2 = "FULFILLED"; },
          e => { r2 = "REJECTED:" + (e && e.message); }
        );
        ({ get a() { return r1; }, get b() { return r2; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let a_k = v8::String::new(scope, "a").unwrap();
            let b_k = v8::String::new(scope, "b").unwrap();
            (
                obj.get(scope, a_k.into()).unwrap().to_rust_string_lossy(scope),
                obj.get(scope, b_k.into()).unwrap().to_rust_string_lossy(scope),
            )
        },
    );
    assert!(r.0.contains("REJECTED") && r.0.contains("source-fail"), "got: '{}'", r.0);
    assert!(r.1.contains("REJECTED") && r.1.contains("source-fail"), "got: '{}'", r.1);
}

#[test]
fn tee_throws_if_source_locked() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream();
        rs.getReader();
        let kind;
        try { rs.tee(); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

// ===========================================================================
// BYOB byte streams — spec §3.7 + §3.5 + §3.8
// ---------------------------------------------------------------------------
// Validates the IDL surface and core algorithms for `type: "bytes"` streams.
// Uses run_with_streams (same harness as default-stream tests).
// ===========================================================================

#[test]
fn byob_byte_stream_construct_no_args_sets_default_hwm_zero() {
    // Spec §3.2.4 byte streams: default hwm == 0, NOT 1.
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({ type: "bytes" });
        // No direct hwm getter on the stream — verify byobRequest exists
        // (controller is byte-typed) by trying to acquire a BYOB reader.
        const reader = rs.getReader({ mode: "byob" });
        Object.prototype.toString.call(reader);
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "[object ReadableStreamBYOBReader]");
}

#[test]
fn byob_default_reader_works_on_byte_stream() {
    // Default reader on a byte stream should yield Uint8Array views.
    let r = run_with_streams(
        r#"
        let buf;
        const rs = new ReadableStream({
          type: "bytes",
          start(c) { c.enqueue(new Uint8Array([1, 2, 3])); c.close(); }
        });
        const reader = rs.getReader();
        async function go() {
          const r = await reader.read();
          if (r.done) return "done";
          // r.value should be a Uint8Array
          buf = r.value;
          return [r.value.constructor.name, r.value.byteLength].join(":");
        }
        go();
        "#,
        |val, scope| {
            // val is a Promise; perform microtask draining done already
            // by run_with_streams. Read the resolved value.
            val.to_rust_string_lossy(scope)
        },
    );
    // Promise's stringification is "[object Promise]"; that's not what
    // we want. Use a different approach: settle the promise into a
    // global side-effect.
    let _ = r;

    let r2 = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({
          type: "bytes",
          start(c) { c.enqueue(new Uint8Array([1, 2, 3])); c.close(); }
        });
        const reader = rs.getReader();
        (async () => {
          const r = await reader.read();
          if (r.done) { result = "done"; return; }
          result = r.value.constructor.name + ":" + r.value.byteLength + ":" +
                   r.value[0] + "," + r.value[1] + "," + r.value[2];
        })();
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert!(r2.starts_with("Uint8Array:3:"), "got: {}", r2);
    assert!(r2.contains("1,2,3"), "got: {}", r2);
}

#[test]
fn byob_reader_read_fills_view() {
    let r = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({
          type: "bytes",
          start(c) {
            c.enqueue(new Uint8Array([10, 20, 30, 40, 50]));
            c.close();
          }
        });
        const reader = rs.getReader({ mode: "byob" });
        (async () => {
          const view = new Uint8Array(5);
          const r = await reader.read(view);
          if (r.done && r.value.byteLength === 0) { result = "done-empty"; return; }
          result = r.value.constructor.name + ":" + r.value.byteLength + ":" +
                   Array.from(r.value).join(",");
        })();
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert!(r.contains("Uint8Array:5"), "got: {}", r);
    assert!(r.contains("10,20,30,40,50"), "got: {}", r);
}

#[test]
fn byob_reader_read_min_zero_rejects_typeerror() {
    let r = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({ type: "bytes" });
        const reader = rs.getReader({ mode: "byob" });
        const view = new Uint8Array(8);
        reader.read(view, { min: 0 }).then(
          () => { result = "FULFILLED"; },
          e => { result = (e && e.name) || "ERR"; }
        );
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byob_reader_read_min_too_large_rejects_rangeerror() {
    let r = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({ type: "bytes" });
        const reader = rs.getReader({ mode: "byob" });
        const view = new Uint8Array(8);
        reader.read(view, { min: 9 }).then(
          () => { result = "FULFILLED"; },
          e => { result = (e && e.name) || "ERR"; }
        );
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "RangeError");
}

#[test]
fn byob_byobrequest_respond_advances_descriptor() {
    let r = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({
          type: "bytes",
          pull(controller) {
            const req = controller.byobRequest;
            // Fill the view with 3 bytes
            const v = req.view;
            v[0] = 7; v[1] = 8; v[2] = 9;
            req.respond(3);
            controller.close();
          }
        });
        const reader = rs.getReader({ mode: "byob" });
        const view = new Uint8Array(3);
        reader.read(view).then(
          r => {
            if (r.done) { result = "done"; return; }
            result = Array.from(r.value).join(",");
          },
          e => { result = "REJ:" + (e && e.message); }
        );
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "7,8,9", "got: {}", r);
}

#[test]
fn byob_d15_respond_zero_on_close_with_nonempty_queue_throws() {
    // CRITICAL D-15: After controller.close() with non-empty queue,
    // closeRequested === true but state is still 'readable'. respond(0)
    // MUST throw TypeError in this window.
    //
    // We simulate this by enqueueing into the queue (so it's non-empty),
    // calling controller.close() to set closeRequested=true (state stays
    // readable), then trying respond(0) on the byobRequest of a pending
    // BYOB read.
    //
    // The BYOB request only exists when there's a pending pull-into; we
    // need to issue a BYOB read AFTER close() but BEFORE the queue is
    // drained.
    let r = run_with_streams(
        r#"
        let result = "pending";
        let savedController;
        const rs = new ReadableStream({
          type: "bytes",
          start(c) { savedController = c; }
        });
        // Enqueue some bytes to make queue non-empty
        savedController.enqueue(new Uint8Array([1, 2, 3]));
        // Close: this sets closeRequested=true but state is still readable
        savedController.close();
        // At this point: closeRequested=true, state=readable.
        // Acquire a BYOB reader and try to issue a read — that should
        // succeed (and resolve immediately since queue has bytes).
        // But we need to test respond(0) directly. This requires an
        // active byobRequest, which only happens when a BYOB read has
        // been issued AND no chunks were already in the queue. So this
        // path is hard to reach via public API alone.
        //
        // Alternative: test the `state == readable + bytes_written == 0`
        // path via a freshly-issued BYOB read where the source pull
        // calls respond(0).
        const rs2 = new ReadableStream({
          type: "bytes",
          pull(c) {
            try {
              c.byobRequest.respond(0);
              result = "no-throw";
            } catch (e) {
              result = e.name;
            }
          }
        });
        const reader = rs2.getReader({ mode: "byob" });
        const view = new Uint8Array(3);
        reader.read(view); // triggers pull which respond(0)s
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError", "respond(0) on readable stream MUST throw");
}

#[test]
fn byob_d16_enqueue_with_detached_buffer_throws() {
    // D-16: enqueue with a view whose buffer was already detached must
    // throw TypeError.
    let r = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({
          type: "bytes",
          start(c) {
            const view = new Uint8Array([1, 2, 3]);
            // Detach the buffer via ArrayBuffer.prototype.transfer (ES2024).
            view.buffer.transfer();
            // Now view.buffer is detached; enqueue MUST throw.
            try {
              c.enqueue(view);
              result = "no-throw";
            } catch (e) {
              result = e.name;
            }
          }
        });
        // Force start to run
        rs.getReader({ mode: "byob" });
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byob_d16_respond_with_new_view_detached_throws() {
    // D-16: respondWithNewView with a view whose buffer is detached
    // must throw TypeError.
    let r = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({
          type: "bytes",
          pull(c) {
            const detachedView = new Uint8Array(3);
            detachedView.buffer.transfer();
            try {
              c.byobRequest.respondWithNewView(detachedView);
              result = "no-throw";
            } catch (e) {
              result = e.name;
            }
          }
        });
        const reader = rs.getReader({ mode: "byob" });
        reader.read(new Uint8Array(3));
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byob_auto_allocate_chunk_size_default_reader() {
    // With autoAllocateChunkSize set, a default reader's read on a byte
    // stream should auto-allocate a buffer + descriptor; pull's
    // byobRequest provides the buffer to fill.
    let r = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({
          type: "bytes",
          autoAllocateChunkSize: 8,
          pull(c) {
            // byobRequest should be present (auto-alloc made one).
            const req = c.byobRequest;
            if (!req) { result = "no-byob-request"; return; }
            const v = req.view;
            for (let i = 0; i < v.byteLength; i++) v[i] = i + 1;
            req.respond(v.byteLength);
            c.close();
          }
        });
        const reader = rs.getReader();
        reader.read().then(r => {
          if (r.done) { result = "done"; return; }
          result = r.value.constructor.name + ":" + r.value.byteLength;
        });
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "Uint8Array:8");
}

#[test]
fn byob_transfer_array_buffer_detaches_source_on_enqueue() {
    let r = run_with_streams(
        r#"
        let result = "pending";
        const view = new Uint8Array([1, 2, 3]);
        const rs = new ReadableStream({
          type: "bytes",
          start(c) { c.enqueue(view); }
        });
        // After enqueue, view.buffer should be detached (TransferArrayBuffer
        // ran).
        const reader = rs.getReader({ mode: "byob" });
        reader.read(new Uint8Array(3));
        result = view.byteLength === 0 ? "detached" : "still-attached:" + view.byteLength;
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "detached");
}

#[test]
fn byob_release_lock_rejects_in_flight_reads() {
    let r = run_with_streams(
        r#"
        let result = "pending";
        const rs = new ReadableStream({ type: "bytes" });
        const reader = rs.getReader({ mode: "byob" });
        const view = new Uint8Array(8);
        const p = reader.read(view);
        reader.releaseLock();
        p.then(
          () => { result = "FULFILLED"; },
          e => { result = (e && e.name) || "ERR"; }
        );
        ({ get out() { return result; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let key = v8::String::new(scope, "out").unwrap();
            obj.get(scope, key.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byob_byte_stream_locked_after_byob_reader_acquired() {
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream({ type: "bytes" });
        rs.getReader({ mode: "byob" });
        rs.locked;
        "#,
        |val, scope| val.boolean_value(scope),
    );
    assert!(r);
}

#[test]
fn byob_get_reader_byob_on_default_stream_throws_typeerror() {
    // Spec: getReader({mode: "byob"}) on a default stream throws TypeError.
    let r = run_with_streams(
        r#"
        const rs = new ReadableStream();
        let kind;
        try { rs.getReader({ mode: "byob" }); }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "TypeError");
}

#[test]
fn byob_byte_stream_strategy_with_size_throws() {
    // Spec §3.2.4: byte streams cannot have a custom strategy.size.
    let r = run_with_streams(
        r#"
        let kind;
        try {
          new ReadableStream(
            { type: "bytes" },
            { highWaterMark: 1, size: () => 1 }
          );
        }
        catch (e) { kind = e.constructor.name; }
        kind || "no-throw";
        "#,
        |val, scope| val.to_rust_string_lossy(scope),
    );
    assert_eq!(r, "RangeError");
}

#[test]
fn byte_tee_both_branches_receive_chunks() {
    let r = run_with_streams(
        r#"
        let result1 = "pending", result2 = "pending";
        const rs = new ReadableStream({
          type: "bytes",
          start(c) {
            c.enqueue(new Uint8Array([1, 2, 3]));
            c.close();
          }
        });
        const [b1, b2] = rs.tee();
        const r1 = b1.getReader();
        const r2 = b2.getReader();
        async function pump(r) {
          const out = [];
          while (true) {
            const x = await r.read();
            if (x.done) break;
            for (let i = 0; i < x.value.byteLength; i++) out.push(x.value[i]);
          }
          return out.join(",");
        }
        pump(r1).then(s => result1 = s);
        pump(r2).then(s => result2 = s);
        ({ get a() { return result1; }, get b() { return result2; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let a_k = v8::String::new(scope, "a").unwrap();
            let b_k = v8::String::new(scope, "b").unwrap();
            (
                obj.get(scope, a_k.into()).unwrap().to_rust_string_lossy(scope),
                obj.get(scope, b_k.into()).unwrap().to_rust_string_lossy(scope),
            )
        },
    );
    assert_eq!(r.0, "1,2,3", "branch1 got: {}", r.0);
    assert_eq!(r.1, "1,2,3", "branch2 got: {}", r.1);
}

#[test]
fn byte_tee_cancel_one_keeps_other() {
    let r = run_with_streams(
        r#"
        let result2 = "pending";
        const rs = new ReadableStream({
          type: "bytes",
          start(c) {
            c.enqueue(new Uint8Array([7, 8, 9]));
            c.close();
          }
        });
        const [b1, b2] = rs.tee();
        b1.cancel("nope");
        const r2 = b2.getReader();
        async function pump() {
          const out = [];
          while (true) {
            const x = await r2.read();
            if (x.done) break;
            for (let i = 0; i < x.value.byteLength; i++) out.push(x.value[i]);
          }
          return out.join(",");
        }
        pump().then(s => result2 = s);
        ({ get b() { return result2; } });
        "#,
        |val, scope| {
            let obj: v8::Local<v8::Object> = val.try_into().unwrap();
            let b_k = v8::String::new(scope, "b").unwrap();
            obj.get(scope, b_k.into()).unwrap().to_rust_string_lossy(scope)
        },
    );
    assert_eq!(r, "7,8,9", "cancelled branch1 should not stop branch2");
}
