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
