//! Creator JavaScript can neither abort the worker nor reach native memory
//! through a web class.
//!
//! One worker thread and one process host many tenants' isolates. Every V8
//! callback is an `extern "C"` function, so a Rust panic inside one cannot
//! unwind: the process aborts and takes every tenant with it. Every native
//! class keeps its state behind a raw pointer in an internal field, so a
//! receiver or argument of the wrong class that reaches the cast reads one
//! class's memory as another's.
//!
//! The cases below drive each of those paths from creator code through the
//! public dispatch surface:
//!
//! - a method `.call()`ed on a wrapper of another class throws a `TypeError`
//!   instead of casting that wrapper's state;
//! - deleting a controller method or the `Response` global, or swapping
//!   `globalThis.ReadableStream` / `globalThis.Response`, changes nothing the
//!   runtime builds internally;
//! - an isolate terminated mid-callback, by its heap limit or its CPU limit,
//!   answers with a non-2xx instead of aborting the process;
//! - text longer than V8 accepts as a string (a result, a body read, a
//!   message quoting an argument) is refused with the error JavaScript itself
//!   raises instead of being unwrapped.
//!
//! A case that aborts kills its process, so every abort-shaped case runs in a
//! process of its own: under nextest each test already has one, and under
//! `cargo test` the case re-executes this binary for just itself.

use crate::support;
use support::*;
use crate::in_own_process;

use std::time::Duration;

use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::{init_v8, EnvSnapshot, FetchOutcome, RequestCtx, Runtime, SettledFetch};

/// Parse a procedure's JSON result into its `{ label: outcome }` pairs, sorted
/// by label and checked to be nonempty so a script that probed nothing cannot
/// pass.
fn outcomes(json: &str) -> Vec<(String, String)> {
    let value: serde_json::Value =
        serde_json::from_str(json).unwrap_or_else(|e| panic!("not JSON: {e}: {json}"));
    let mut pairs: Vec<(String, String)> = value
        .as_object()
        .unwrap_or_else(|| panic!("not an outcome map: {json}"))
        .iter()
        .map(|(label, outcome)| (label.clone(), outcome.as_str().unwrap_or_default().to_owned()))
        .collect();
    pairs.sort();
    assert!(!pairs.is_empty(), "no cases were probed: {json}");
    pairs
}

/// Helpers every cross-receiver script shares. `probe` records what a call
/// did: `TypeError` when it threw one or returned a promise rejected with one,
/// otherwise what happened instead.
const PROBE: &str = r#"
const out = {};
async function probe(label, call) {
    try {
        await call();
        out[label] = "completed";
    } catch (e) {
        out[label] = e instanceof TypeError ? "TypeError" : `${e && e.name}: ${e && e.message}`;
    }
}
// A body or blob part that is not a genuine stream or Blob is converted to a
// string, as WebIDL's union conversion does for any other object. Throw a
// TypeError when that conversion is what happened, so a foreign wrapper read
// as a stream or a Blob reports as not refused.
async function asString(label, value, read) {
    await probe(label, async () => {
        if ((await read(value)) === String(value)) throw new TypeError("converted to a string");
    });
}
function as(proto, wrapper) {
    return Object.setPrototypeOf(wrapper, proto);
}
function getter(proto, name) {
    return Object.getOwnPropertyDescriptor(proto, name).get;
}
"#;

/// Run one cross-receiver script and require every probe to have thrown a
/// `TypeError`. Each probe hands a genuine wrapper of one class to a method of
/// another, so the receiver carries native state of the wrong type.
fn assert_every_probe_is_a_type_error(cases: &str) {
    assert_every_probe_is_a_type_error_after("", cases);
}

/// [`assert_every_probe_is_a_type_error`] with module-level `imports` ahead
/// of the probes.
fn assert_every_probe_is_a_type_error_after(imports: &str, cases: &str) {
    let source =
        format!("{imports}\n{PROBE}\nexport async function test() {{\n{cases}\nreturn out;\n}}");
    let result = dispatch(m(&source), "test", "[]").expect("the probe script settles");
    let pairs = outcomes(&result.json);
    let wrong: Vec<_> = pairs.iter().filter(|(_, outcome)| outcome != "TypeError").collect();
    assert!(wrong.is_empty(), "these foreign receivers were not refused: {wrong:?}");
}

// ---------------------------------------------------------------------------
// A. A wrapper of one class is never read as another
// ---------------------------------------------------------------------------

#[test]
fn readable_stream_methods_refuse_a_foreign_wrapper() {
    in_own_process!(readable_stream_methods_refuse_a_foreign_wrapper, {
        assert_every_probe_is_a_type_error(
            r#"
            await probe("cancel", () => ReadableStream.prototype.cancel.call(new WritableStream()));
            await probe("tee", () => ReadableStream.prototype.tee.call(new WritableStream()));
            await probe("getReader", () => ReadableStream.prototype.getReader.call(new TransformStream()));
            await probe("locked", () => getter(ReadableStream.prototype, "locked").call(new WritableStream()));
            await probe("new reader", () => new ReadableStreamDefaultReader(new WritableStream()));
            await asString("response body", as(ReadableStream.prototype, new WritableStream()), (v) => new Response(v).text());
            "#,
        );
    });
}

#[test]
fn stream_controller_methods_refuse_a_foreign_wrapper() {
    in_own_process!(stream_controller_methods_refuse_a_foreign_wrapper, {
        assert_every_probe_is_a_type_error(
            r#"
            let rc, bc, wc, tc;
            new ReadableStream({ start(c) { rc = c; } });
            new ReadableStream({ type: "bytes", start(c) { bc = c; } });
            new WritableStream({ start(c) { wc = c; } });
            new TransformStream({ start(c) { tc = c; } });
            const rcp = Object.getPrototypeOf(rc), bcp = Object.getPrototypeOf(bc);
            const wcp = Object.getPrototypeOf(wc), tcp = Object.getPrototypeOf(tc);
            await probe("default enqueue", () => rcp.enqueue.call(new WritableStream(), 1));
            await probe("default close", () => rcp.close.call(bc));
            await probe("default desiredSize", () => getter(rcp, "desiredSize").call(new TransformStream()));
            await probe("byte enqueue", () => bcp.enqueue.call(rc, new Uint8Array(1)));
            await probe("writable error", () => wcp.error.call(rc, 1));
            await probe("transform enqueue", () => tcp.enqueue.call(wc, 1));
            "#,
        );
    });
}

#[test]
fn reader_and_writer_methods_refuse_a_foreign_wrapper() {
    in_own_process!(reader_and_writer_methods_refuse_a_foreign_wrapper, {
        assert_every_probe_is_a_type_error(
            r#"
            const R = ReadableStreamDefaultReader.prototype;
            const W = WritableStreamDefaultWriter.prototype;
            const B = ReadableStreamBYOBReader.prototype;
            await probe("reader read", () => R.read.call(as(R, new WritableStream())));
            await probe("reader closed", () => getter(R, "closed").call(as(R, new Headers())));
            await probe("byob read", () => B.read.call(as(B, new ReadableStream().getReader()), new Uint8Array(1)));
            await probe("writer write", () => W.write.call(as(W, new ReadableStream()), 1));
            await probe("writer desiredSize", () => getter(W, "desiredSize").call(as(W, new Blob([]))));
            const iterator = new ReadableStream().values();
            const I = Object.getPrototypeOf(iterator);
            await probe("iterator next", () => I.next.call(as(I, new ReadableStream().getReader())));
            "#,
        );
    });
}

#[test]
fn writable_and_transform_methods_refuse_a_foreign_wrapper() {
    in_own_process!(writable_and_transform_methods_refuse_a_foreign_wrapper, {
        assert_every_probe_is_a_type_error(
            r#"
            const T = TransformStream.prototype;
            await probe("abort", () => WritableStream.prototype.abort.call(new ReadableStream()));
            await probe("getWriter", () => WritableStream.prototype.getWriter.call(as(WritableStream.prototype, new ReadableStream())));
            await probe("readable", () => getter(T, "readable").call(as(T, new WritableStream())));
            await probe("writable", () => getter(T, "writable").call(as(T, new Headers())));
            await probe("pipeTo dest", () => new ReadableStream().pipeTo(as(WritableStream.prototype, new ReadableStream())));
            "#,
        );
    });
}

#[test]
fn fetch_class_methods_refuse_a_foreign_wrapper() {
    in_own_process!(fetch_class_methods_refuse_a_foreign_wrapper, {
        assert_every_probe_is_a_type_error(
            r#"
            const H = Headers.prototype, Q = Request.prototype, S = Response.prototype;
            await probe("headers get", () => H.get.call(as(H, new URLSearchParams("a=1")), "a"));
            await probe("headers has", () => H.has.call(as(H, new Blob(["x"])), "a"));
            await probe("request clone", () => Q.clone.call(as(Q, new Headers())));
            await probe("request url", () => getter(Q, "url").call(as(Q, new Response("x"))));
            await probe("response clone", () => S.clone.call(as(S, new Request("http://localhost/"))));
            await probe("response text", () => S.text.call(as(S, new Headers())));
            await probe("response status", () => getter(S, "status").call(as(S, new Blob(["x"]))));
            await probe("dispatchEvent", () => new EventTarget().dispatchEvent(as(Event.prototype, new Headers())));
            // A constructor called without `new` on another class's wrapper
            // must refuse, not store its own state over the wrapper's.
            await probe("constructor without new", () => {
                const headers = new Headers([["a", "1"]]);
                let refused = false;
                try { EventTarget.call(headers); } catch (e) { refused = e instanceof TypeError; }
                let kept;
                try { kept = headers.get("a"); } catch (e) { kept = `get threw ${e.name}`; }
                if (refused && kept === "1") throw new TypeError("refused, state kept");
            });
            "#,
        );
    });
}

#[test]
fn blob_and_file_methods_refuse_a_foreign_wrapper() {
    in_own_process!(blob_and_file_methods_refuse_a_foreign_wrapper, {
        assert_every_probe_is_a_type_error(
            r#"
            const B = Blob.prototype, F = File.prototype;
            await probe("blob text", () => B.text.call(as(B, new Headers())));
            await probe("blob size", () => getter(B, "size").call(as(B, new URLSearchParams("a=1"))));
            await probe("blob slice", () => B.slice.call(as(B, new Response("x"))));
            await probe("file name", () => getter(F, "name").call(as(F, new Blob(["x"]))));
            await asString("blob part", as(B, new Headers()), (v) => new Blob([v]).text());
            "#,
        );
    });
}

/// The key classes that read native key material from an argument tell a
/// key wrapper apart by its brand, not by a tag byte at the start of
/// whatever the argument's internal field points at: an `EventTarget`'s
/// state is zero-sized, so its pointer is dangling and reading even one byte
/// through it faults.
#[test]
fn crypto_keys_refuse_a_foreign_wrapper() {
    in_own_process!(crypto_keys_refuse_a_foreign_wrapper, {
        assert_every_probe_is_a_type_error_after(
            r#"import { createPublicKey, KeyObject } from "node:crypto";"#,
            r#"
            await probe("exportKey", () => crypto.subtle.exportKey("raw", new EventTarget()));
            // Not a KeyObject, so it is parsed as key input instead, and refused
            // as that (a DataError for a JWK without `kty`).
            await probe("createPublicKey", () => {
                try { createPublicKey(new EventTarget()); }
                catch (e) { throw new TypeError(`refused: ${e.name}`); }
            });
            await probe("KeyObject.from", () => KeyObject.from(new EventTarget()));
            "#,
        );
    });
}

/// `Response.prototype.clone` builds its result with the realm's own
/// `Response`, so a creator constructor that returns another class's wrapper
/// is never handed the clone's state.
#[test]
fn response_clone_ignores_a_swapped_global_response() {
    in_own_process!(response_clone_ignores_a_swapped_global_response, {
        let result = dispatch(
            m(r#"
            export async function test() {
                const R = Response;
                const decoy = new URLSearchParams("a=1");
                globalThis.Response = function () { return decoy; };
                const original = new R("body", { status: 201, headers: { "x-kept": "1" } });
                const copy = original.clone();
                return {
                    brand: String(copy instanceof R && copy !== decoy),
                    status: String(copy.status),
                    header: String(copy.headers.get("x-kept")),
                    text: await copy.text(),
                    original: await original.text(),
                    decoy: decoy.toString(),
                };
            }
            "#),
            "test",
            "[]",
        )
        .expect("the clone script settles");
        let pairs = outcomes(&result.json);
        let expected = [
            ("brand", "true"),
            ("decoy", "a=1"),
            ("header", "1"),
            ("original", "body"),
            ("status", "201"),
            ("text", "body"),
        ];
        let expected: Vec<(String, String)> =
            expected.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        assert_eq!(pairs, expected, "{}", result.json);
    });
}

// ---------------------------------------------------------------------------
// B. Creator-mutable lookups on internal paths cannot abort the process
// ---------------------------------------------------------------------------

/// The pull source behind a buffered body calls the controller natively, so
/// deleting `enqueue` from the controller prototype changes nothing it reads.
#[test]
fn a_deleted_controller_method_does_not_abort_a_body_read() {
    in_own_process!(a_deleted_controller_method_does_not_abort_a_body_read, {
        let result = dispatch(
            m(r#"
            export async function test() {
                let proto;
                new ReadableStream({ start(c) { proto = Object.getPrototypeOf(c); } });
                delete proto.enqueue;
                delete proto.close;
                const reader = new Response("x").body.getReader();
                const first = await reader.read();
                const second = await reader.read();
                return {
                    chunk: new TextDecoder().decode(first.value),
                    done: String(second.done),
                };
            }
            "#),
            "test",
            "[]",
        )
        .expect("the body read settles");
        let pairs = outcomes(&result.json);
        assert_eq!(
            pairs,
            vec![("chunk".to_owned(), "x".to_owned()), ("done".to_owned(), "true".to_owned())],
            "{}",
            result.json
        );
    });
}

/// `clone()` builds with the realm's own `Response`, so deleting the global
/// leaves it working.
#[test]
fn a_deleted_global_response_does_not_abort_clone() {
    in_own_process!(a_deleted_global_response_does_not_abort_clone, {
        let result = dispatch(
            m(r#"
            export async function test() {
                const R = Response;
                const Q = Request;
                delete globalThis.Response;
                delete globalThis.Request;
                const response = new R("x").clone();
                const request = new Q("http://localhost/", { method: "POST", body: "y" }).clone();
                return { response: await response.text(), request: await request.text() };
            }
            "#),
            "test",
            "[]",
        )
        .expect("the clone script settles");
        let pairs = outcomes(&result.json);
        assert_eq!(
            pairs,
            vec![("request".to_owned(), "y".to_owned()), ("response".to_owned(), "x".to_owned())],
            "{}",
            result.json
        );
    });
}

// ---------------------------------------------------------------------------
// C. Internal construction uses the realm's original constructors
// ---------------------------------------------------------------------------

#[test]
fn swapped_global_constructors_do_not_change_internal_streams() {
    in_own_process!(swapped_global_constructors_do_not_change_internal_streams, {
        let result = dispatch(
            m(r#"
            export async function test() {
                const RS = ReadableStream, WS = WritableStream, R = Response;
                function Fake() { return { forged: true }; }
                globalThis.ReadableStream = Fake;
                globalThis.WritableStream = Fake;
                globalThis.TransformStream = Fake;
                globalThis.Response = Fake;
                const body = new R("x").body;
                const [left, right] = new RS().tee();
                const pair = new CompressionStream("gzip");
                const copy = new R("y").clone();
                return {
                    body: String(body instanceof RS),
                    text: await new R(body).text(),
                    tee: String(left instanceof RS && right instanceof RS),
                    readable: String(pair.readable instanceof RS),
                    writable: String(pair.writable instanceof WS),
                    clone: String(copy instanceof R),
                    blob: String(new Blob(["z"]).stream() instanceof RS),
                };
            }
            "#),
            "test",
            "[]",
        )
        .expect("the construction script settles");
        let pairs = outcomes(&result.json);
        let wrong: Vec<_> = pairs
            .iter()
            .filter(|(label, outcome)| {
                let expected = if label == "text" { "x" } else { "true" };
                outcome != expected
            })
            .collect();
        assert!(wrong.is_empty(), "internal construction followed the swapped globals: {wrong:?}");
    });
}

/// A BYOB read into a `DataView` or a `Float16Array` builds its result with
/// the realm's own constructor, so a creator function put on the global in
/// its place is never what the read resolves with.
#[test]
fn swapped_global_view_constructors_do_not_change_byob_reads() {
    in_own_process!(swapped_global_view_constructors_do_not_change_byob_reads, {
        let result = dispatch(
            m(r#"
            export async function test() {
                const DV = DataView, F16 = Float16Array;
                const decoy = new DV(new ArrayBuffer(8));
                globalThis.DataView = function () { return decoy; };
                globalThis.Float16Array = function () { return decoy; };
                async function readInto(view) {
                    const stream = new ReadableStream({
                        type: "bytes",
                        pull(c) { c.enqueue(new Uint8Array([1, 2, 3, 4])); },
                    });
                    const { value } = await stream.getReader({ mode: "byob" }).read(view);
                    return value;
                }
                const dv = await readInto(new DV(new ArrayBuffer(4)));
                const f16 = await readInto(new F16(2));
                return {
                    dataView: String(dv instanceof DV && dv !== decoy && dv.byteLength === 4 && dv.getUint8(3) === 4),
                    float16: String(f16 instanceof F16 && f16 !== decoy && f16.byteLength === 4),
                };
            }
            "#),
            "test",
            "[]",
        )
        .expect("the BYOB script settles");
        let pairs = outcomes(&result.json);
        assert_eq!(pairs, labelled(&[("dataView", "true"), ("float16", "true")]), "{}", result.json);
    });
}

// ---------------------------------------------------------------------------
// D. A terminated isolate answers instead of aborting the process
// ---------------------------------------------------------------------------

/// Drive one dispatch to settlement and report `(status, body)`. A terminated
/// isolate answers synchronously with a 503 whose body is redacted, or fails
/// its pending dispatch with the termination's cause as the message.
fn settle(runtime: &Runtime) -> (u16, String) {
    let env = EnvSnapshot::empty();
    let ctx = RequestCtx::new(CancelFlag::new());
    answer(runtime, runtime.call_fetch_handler("GET", "http://localhost/", &[], "", &env, ctx))
}

/// Report a dispatch's `(status, body)`. A pending dispatch is driven on a
/// pump started only now, so whatever the dispatch left queued (a zero-delay
/// timer, a cancellation) is the first work that pump finds.
fn answer(runtime: &Runtime, outcome: FetchOutcome) -> (u16, String) {
    match outcome {
        FetchOutcome::Response { status, body, .. } => {
            (status, String::from_utf8_lossy(&body).into_owned())
        }
        FetchOutcome::Stream { status, .. } => (status, "<stream>".to_owned()),
        FetchOutcome::WebSocketUpgrade { .. } => panic!("unexpected WebSocketUpgrade"),
        FetchOutcome::Pending { rx, cancel: _ } => compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(async {
                runtime.start_pump();
                let settled = compio::time::timeout(Duration::from_mins(1), rx.recv())
                    .await
                    .expect("the terminated dispatch never settled");
                match settled {
                    Ok(SettledFetch::Response { status, body, .. }) => {
                        (status, String::from_utf8_lossy(&body).into_owned())
                    }
                    Ok(SettledFetch::Stream { status, .. }) => (status, "<stream>".to_owned()),
                    Ok(SettledFetch::WebSocketUpgrade { .. }) => {
                        panic!("unexpected WebSocketUpgrade")
                    }
                    Err(e) => (e.status, e.message),
                }
            }),
    }
}

/// Require `outcome` to be the runtime's refusal of a terminated isolate, so a
/// handler that failed some other way (a thrown `RangeError`, say) cannot pass
/// for a termination that never happened.
fn assert_terminated(outcome: &(u16, String), cause: &str) {
    let (status, body) = outcome;
    assert!(
        *status == 503 || body == cause,
        "expected the {cause:?} refusal, got {status}: {body}",
    );
}

/// Require that no native callback panicked since `before`. The panic
/// boundary would turn such a panic into a thrown error and the dispatch
/// would still answer, so the answer alone cannot show that every V8 call
/// that refused (a terminated isolate's, or one given text past the string
/// limit) was handled where it was made.
fn assert_no_panic_was_caught(before: u64) {
    assert_eq!(
        zeroship_runtime::callback::caught_panics() - before,
        0,
        "a native callback panicked instead of handling a refused V8 call",
    );
}

/// An app at its own heap limit while constructing streams: the near-heap-
/// limit callback requests termination during an allocation inside a stream
/// constructor, and the constructor's next call into V8 returns empty. The
/// constructor must leave the termination pending instead of unwrapping it.
#[test]
fn heap_limit_termination_inside_stream_construction_does_not_abort() {
    in_own_process!(heap_limit_termination_inside_stream_construction_does_not_abort, {
        init_v8();
        let runtime = Runtime::builder()
            .modules(m(r"
            export default {
                fetch() {
                    const held = [];
                    for (;;) {
                        held.push(new ReadableStream({ start() {} }), new WritableStream());
                    }
                }
            };
            "))
            .heap_limit_mb(16)
            .build();
        let hits_before = zeroship_runtime::heap_limit_callback_hits();
        let panics_before = zeroship_runtime::callback::caught_panics();
        let outcome = settle(&runtime);
        assert!(
            zeroship_runtime::heap_limit_callback_hits() > hits_before,
            "the handler never reached its heap limit, so termination was not exercised",
        );
        assert_terminated(&outcome, "memory limit exceeded");
        assert_no_panic_was_caught(panics_before);
    });
}

/// The CPU limit firing while the isolate is inside native stream
/// construction: the timer requests termination from its own thread, the
/// constructor's next call into V8 returns empty, and the constructor must
/// leave the termination pending instead of unwrapping it. Nearly all of the
/// loop's time is spent inside the constructors, so that is where the timer
/// usually lands. Nothing is retained, so the heap limit cannot be what
/// refuses it.
///
/// Where the timer lands is up to the scheduler, so this case binds any one
/// call site only some of the time. The deterministic binding for the
/// constructors' handling of a refused V8 call is
/// `heap_limit_termination_inside_stream_construction_does_not_abort`, whose
/// termination comes from an allocation inside the constructor itself; this
/// case covers the same handling under a termination requested from another
/// thread.
#[test]
fn cpu_limit_termination_inside_stream_construction_does_not_abort() {
    in_own_process!(cpu_limit_termination_inside_stream_construction_does_not_abort, {
        init_v8();
        let runtime = Runtime::builder()
            .modules(m(r"
            export default {
                fetch() {
                    for (;;) {
                        new ReadableStream({ start() {} });
                        new WritableStream({ start() {} });
                    }
                }
            };
            "))
            .cpu_limit(Duration::from_millis(200))
            .build();
        let panics_before = zeroship_runtime::callback::caught_panics();
        assert_terminated(&settle(&runtime), "CPU time limit exceeded");
        assert_no_panic_was_caught(panics_before);
    });
}

// ---------------------------------------------------------------------------
// D. Text longer than V8's string limit is refused, not unwrapped
// ---------------------------------------------------------------------------

/// Run `cases` (statements that record outcomes through `probe`, with `MAX`
/// bound to V8's string limit in bytes) as an app's fetch handler under a
/// `heap_limit_mb` heap, and return the outcomes, requiring that no native
/// callback panicked meanwhile: the panic boundary would turn an unwrapped
/// refusal into an `Error` the probes could not tell from a thrown one.
fn oversized_text_outcomes(heap_limit_mb: u32, cases: &str) -> Vec<(String, String)> {
    init_v8();
    let source = format!(
        "const MAX = {max};\n{PROBE}\nexport default {{\n    async fetch() {{\n{cases}\n        return new Response(JSON.stringify(out));\n    }}\n}};",
        max = v8::String::MAX_LENGTH,
    );
    let runtime = Runtime::builder().modules(m(&source)).heap_limit_mb(heap_limit_mb).build();
    let panics_before = zeroship_runtime::callback::caught_panics();
    let (status, body) = settle(&runtime);
    assert_no_panic_was_caught(panics_before);
    assert_eq!(status, 200, "the handler must answer: {body}");
    outcomes(&body)
}

fn labelled(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

/// Native results past V8's string limit, built from zeroed buffers the OS
/// has not backed with memory: V8 checks a length before it reads any of
/// the text, so these cost address space, not memory.
mod oversized {
    use zeroship_runtime::byte_string::ByteString;
    use zeroship_runtime::state::OpError;
    use zeroship_runtime_macros::v8_class;

    fn bytes() -> Vec<u8> {
        vec![0u8; v8::String::MAX_LENGTH + 1]
    }

    fn text() -> String {
        String::from_utf8(bytes()).expect("zero bytes are UTF-8")
    }

    /// One method per shape the macro turns into a JS string.
    #[derive(Default)]
    pub struct Oversized;

    #[v8_class]
    impl Oversized {
        #[v8_method]
        fn text(&self) -> String {
            text()
        }

        #[v8_method]
        fn checked(&self) -> Result<String, OpError> {
            Ok(text())
        }

        #[v8_method]
        fn texts(&self) -> Vec<String> {
            vec!["short".to_owned(), text()]
        }

        #[v8_method]
        fn byte_string(&self) -> Option<Vec<u8>> {
            Some(bytes())
        }

        #[v8_method]
        fn byte_strings(&self) -> Vec<Vec<u8>> {
            vec![bytes()]
        }

        #[v8_method]
        fn short(&self) -> String {
            "short".to_owned()
        }

        #[v8_method]
        fn refused(&self) -> Result<u32, OpError> {
            Err(OpError::type_error(text()))
        }

        #[v8_getter]
        fn label(&self) -> String {
            text()
        }
    }

    /// A pair iterable whose one entry has an over-long key or value. The
    /// entries are built fresh for `forEach`, which consumes them without a
    /// copy.
    pub struct Entries {
        long_key: bool,
    }

    #[v8_class]
    #[v8_iterable(key = ByteString, value = String)]
    impl Entries {
        #[v8_constructor]
        fn new(long_key: bool) -> Entries {
            Entries { long_key }
        }

        fn value_pairs(&self) -> Vec<(ByteString, String)> {
            if self.long_key {
                vec![(ByteString::from(bytes()), "value".to_owned())]
            } else {
                vec![(ByteString::from(b"key".to_vec()), text())]
            }
        }
    }
}

/// Every shape in which a `#[v8_class]` hands native text to script (a
/// `String`, `Result<String>`, `Vec<String>`, `ByteString` and
/// `Vec<ByteString>` result, a getter, an iterable's keys and values)
/// answers text past V8's limit with `RangeError: Invalid string length`,
/// and an error message quoting such text is cut to a bounded length; short
/// text still comes through.
#[test]
fn oversized_native_text_is_a_range_error_in_every_macro_shape() {
    in_own_process!(oversized_native_text_is_a_range_error_in_every_macro_shape, {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, v8::ContextOptions::default());
        let scope = &mut v8::ContextScope::new(handles, context);
        let global = scope.get_current_context().global(scope);
        for (name, template) in [
            ("Oversized", oversized::Oversized::install(scope)),
            ("Entries", oversized::Entries::install(scope)),
        ] {
            let class = template.get_function(scope).expect("the class instantiates");
            let key = v8::String::new(scope, name).expect("class name");
            global.set(scope, key.into(), class.into());
        }
        let source = v8::String::new(
            scope,
            r#"
            const out = {};
            function probe(label, call) {
                try { out[label] = String(call()); }
                catch (e) { out[label] = `${e.name}: ${e.message.length > 64 ? `${e.message.length} chars, cut: ${e.message.endsWith("...")}` : e.message}`; }
            }
            const o = new Oversized();
            probe("text", () => o.text());
            probe("checked", () => o.checked());
            probe("texts", () => o.texts());
            probe("byte string", () => o.byte_string());
            probe("byte strings", () => o.byte_strings());
            probe("getter", () => o.label);
            probe("short", () => o.short());
            probe("refused", () => o.refused());
            probe("long key", () => new Entries(true).forEach(() => {}));
            probe("long value", () => new Entries(false).forEach(() => {}));
            JSON.stringify(out)
            "#,
        )
        .expect("script source");
        let script = v8::Script::compile(scope, source, None).expect("the script compiles");
        let panics_before = zeroship_runtime::callback::caught_panics();
        let json = script.run(scope).expect("the script completes").to_rust_string_lossy(scope);
        assert_no_panic_was_caught(panics_before);
        let refused = "RangeError: Invalid string length";
        assert_eq!(
            outcomes(&json),
            labelled(&[
                ("byte string", refused),
                ("byte strings", refused),
                ("checked", refused),
                ("getter", refused),
                ("long key", refused),
                ("long value", refused),
                ("refused", "TypeError: 16387 chars, cut: true"),
                ("short", "short"),
                ("text", refused),
                ("texts", refused),
            ]),
            "{json}"
        );
    });
}

/// A body read whose text is longer than V8 allows rejects with
/// `RangeError`, through the same promise the read returned. Each invalid
/// byte decodes to a three-byte U+FFFD, so a third of the limit in bytes
/// decodes past it.
#[test]
fn an_oversized_body_text_rejects_with_a_range_error() {
    in_own_process!(an_oversized_body_text_rejects_with_a_range_error, {
        let pairs = oversized_text_outcomes(
            128,
            r#"
            await probe("small", async () => {
                if (await new Blob([new Uint8Array([0xff])]).text() !== "\ufffd") throw new Error("wrong text");
            });
            await probe("oversized", () => new Blob([new Uint8Array(Math.floor(MAX / 3) + 1).fill(0xff)]).text());
            "#,
        );
        assert_eq!(
            pairs,
            labelled(&[("oversized", "RangeError: Invalid string length"), ("small", "completed")]),
        );
    });
}

/// A dynamic import of a specifier too long to quote in a V8 string still
/// rejects with the `TypeError` an unknown specifier gets. The host import
/// hook is not a function callback, so nothing but its own handling stands
/// between a refused message and a process abort. As above, each `\u00e9`
/// is within V8's limit as a string and past it as text.
#[test]
fn an_oversized_import_specifier_rejects_with_a_type_error() {
    in_own_process!(an_oversized_import_specifier_rejects_with_a_type_error, {
        let pairs = oversized_text_outcomes(
            2048,
            r#"
            await probe("unknown", () => import("zeroship-no-such-module"));
            await probe("oversized", () => import("\u00e9".repeat(Math.floor(MAX / 2) + 1)));
            "#,
        );
        assert_eq!(pairs, labelled(&[("oversized", "TypeError"), ("unknown", "TypeError")]));
    });
}

// ---------------------------------------------------------------------------
// D. The panic boundary every callback runs under
// ---------------------------------------------------------------------------

mod panicking {
    use zeroship_runtime_macros::v8_class;

    /// A class whose one method panics, standing in for any native method
    /// with a defect a creator can reach.
    #[derive(Default)]
    pub struct Panicker;

    #[v8_class]
    impl Panicker {
        #[v8_method]
        fn explode(&self) -> u32 {
            panic!("a #[v8_class] method panicked");
        }
    }
}

/// A hand-registered callback with the same defect.
fn panicking_callback(
    _scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    panic!("a hand-registered callback panicked");
}

/// A panic inside a native callback becomes a thrown `Error` in the isolate
/// that caused it, for a `#[v8_class]` method and for a callback registered by
/// hand through `callback::function`, and the isolate keeps running script
/// afterwards.
#[test]
fn a_panicking_callback_throws_instead_of_aborting() {
    in_own_process!(a_panicking_callback_throws_instead_of_aborting, {
        init_v8();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default());
        v8::scope!(let handles, &mut isolate);
        let context = v8::Context::new(handles, v8::ContextOptions::default());
        let scope = &mut v8::ContextScope::new(handles, context);
        let global = scope.get_current_context().global(scope);

        let class = panicking::Panicker::install(scope)
            .get_function(scope)
            .expect("the Panicker class instantiates");
        let key = v8::String::new(scope, "Panicker").expect("class name");
        global.set(scope, key.into(), class.into());
        let explode = zeroship_runtime::callback::function(scope, panicking_callback)
            .expect("the hand-registered callback instantiates");
        let key = v8::String::new(scope, "explode").expect("function name");
        global.set(scope, key.into(), explode.into());

        let source = v8::String::new(
            scope,
            r#"
            const outcome = (call) => {
                try { call(); return "returned"; }
                catch (e) { return `${e.name}: ${e.message}`; }
            };
            JSON.stringify({
                method: outcome(() => new Panicker().explode()),
                function: outcome(() => explode()),
                after: String(1 + 1),
            })
            "#,
        )
        .expect("script source");
        let script = v8::Script::compile(scope, source, None).expect("the script compiles");
        let panics_before = zeroship_runtime::callback::caught_panics();
        let result = script.run(scope).expect("the script completes");
        let json = result.to_rust_string_lossy(scope);
        assert_eq!(
            zeroship_runtime::callback::caught_panics() - panics_before,
            2,
            "both panics must be counted where an operator can see them",
        );
        assert_eq!(
            outcomes(&json),
            vec![
                ("after".to_owned(), "2".to_owned()),
                ("function".to_owned(), "Error: internal error".to_owned()),
                ("method".to_owned(), "Error: internal error".to_owned()),
            ],
            "{json}"
        );
    });
}

// ---------------------------------------------------------------------------
// D. An isolate a callback panicked in serves nothing more
// ---------------------------------------------------------------------------

/// A plugin whose one callback panics, reachable from creator code as
/// `env.boom.explode()`, standing in for any native callback with a defect.
struct PanickingPlugin;

impl zeroship_runtime::NativePlugin for PanickingPlugin {
    fn namespace(&self) -> &str {
        "boom"
    }

    fn register(&self, r: &mut zeroship_runtime::NativeRegistrar) {
        r.add("explode", panicking_callback);
    }
}

fn panicking_runtime(source: &str) -> Runtime {
    init_v8();
    Runtime::builder().plugin(PanickingPlugin).modules(m(source)).build()
}

/// A callback that panics during dispatch is answered inside the isolate,
/// and the isolate is then quarantined: whatever the callback was updating
/// may be half done, so its host replaces it before the next request.
#[test]
fn a_panic_during_dispatch_quarantines_the_isolate() {
    in_own_process!(a_panic_during_dispatch_quarantines_the_isolate, {
        let runtime = panicking_runtime(
            r#"
            export default {
                fetch(request, env) {
                    let caught = "nothing thrown";
                    try { env.boom.explode(); } catch (e) { caught = `${e.name}: ${e.message}`; }
                    return new Response(caught);
                }
            };
            "#,
        );
        assert!(!runtime.is_quarantined(), "a fresh isolate serves");
        let panics_before = zeroship_runtime::callback::caught_panics();
        let answered = settle(&runtime);
        assert_eq!(answered, (200, "Error: internal error".to_owned()), "the panic is thrown into the handler");
        assert_eq!(zeroship_runtime::callback::caught_panics() - panics_before, 1);
        assert!(runtime.is_quarantined(), "the isolate the panic happened in is quarantined");
    });
}

/// A callback that panics on the event pump, with a request still pending,
/// fails that request with `internal error` and quarantines the isolate,
/// rather than leaving the request to whatever the half-updated isolate does
/// next.
#[test]
fn a_panic_on_the_pump_fails_the_pending_requests_and_quarantines_the_isolate() {
    in_own_process!(a_panic_on_the_pump_fails_the_pending_requests_and_quarantines_the_isolate, {
        let runtime = panicking_runtime(
            r"
            export default {
                fetch(request, env) {
                    return new Promise(() => {
                        setTimeout(() => { try { env.boom.explode(); } catch {} }, 1);
                    });
                }
            };
            ",
        );
        let answered = settle(&runtime);
        assert_eq!(answered, (500, "internal error".to_owned()), "the pending request is failed, not stranded");
        assert!(runtime.is_quarantined(), "the isolate the panic happened in is quarantined");
    });
}

/// A callback that panics in a zero-delay timer, which the pump fires as soon
/// as it drains the timers a dispatch queued, stops the isolate in that drain.
/// The request still pending on it is failed with `internal error`; a later
/// timer on the half-updated isolate does not get to answer it.
#[test]
fn a_panic_in_a_zero_delay_timer_stops_the_isolate_before_a_later_timer_answers() {
    in_own_process!(a_panic_in_a_zero_delay_timer_stops_the_isolate_before_a_later_timer_answers, {
        let runtime = panicking_runtime(
            r#"
            export default {
                fetch(request, env) {
                    return new Promise((resolve) => {
                        setTimeout(() => { try { env.boom.explode(); } catch {} }, 0);
                        setTimeout(() => resolve(new Response("answered after the panic")), 20);
                    });
                }
            };
            "#,
        );
        let panics_before = zeroship_runtime::callback::caught_panics();
        let answered = settle(&runtime);
        assert_eq!(zeroship_runtime::callback::caught_panics() - panics_before, 1, "the zero-delay timer reached the callback");
        assert_eq!(answered, (500, "internal error".to_owned()), "the pending request is failed by the stop");
        assert!(runtime.is_quarantined(), "the isolate the panic happened in is quarantined");
    });
}

/// An RPC procedure that leaves an `abort` listener behind, and whose listener
/// reaches a callback that panics.
const ABORT_LISTENER_APP: &str = r#"
    import { currentSignal, env } from "zeroship";
    export async function pending() {
        currentSignal().addEventListener("abort", () => {
            try { env.boom.explode(); } catch {}
        });
        await new Promise(() => {});
    }
    export default { rpc: { pending } };
"#;

/// Start the `pending` procedure of [`ABORT_LISTENER_APP`]; it is still
/// running when this returns.
fn start_pending_procedure(runtime: &Runtime) -> FetchOutcome {
    let outcome = runtime.call_fetch_handler(
        "POST",
        "http://localhost/__zeroship/v1/pending",
        &[("content-type".into(), "application/json".into())],
        "{}",
        &EnvSnapshot::empty(),
        RequestCtx::new(CancelFlag::new()),
    );
    assert!(
        matches!(outcome, FetchOutcome::Pending { .. }),
        "the procedure is still running once its listener is registered"
    );
    outcome
}

/// A callback that panics in an `abort` listener, which the pump runs when it
/// settles a cancelled RPC, stops the isolate before the pump does anything
/// else: by the time the cancelled request has its answer, the isolate is
/// quarantined, so the host serves the next request on a fresh one.
#[test]
fn a_panic_in_an_abort_listener_quarantines_the_isolate_before_the_next_dispatch() {
    in_own_process!(a_panic_in_an_abort_listener_quarantines_the_isolate_before_the_next_dispatch, {
        let runtime = panicking_runtime(ABORT_LISTENER_APP);
        let outcome = start_pending_procedure(&runtime);
        let FetchOutcome::Pending { cancel, .. } = &outcome else {
            unreachable!("start_pending_procedure checked the outcome");
        };
        cancel.cancel();
        assert!(!runtime.is_quarantined(), "the premise: nothing has panicked yet");
        let panics_before = zeroship_runtime::callback::caught_panics();
        let answered = answer(&runtime, outcome);
        assert_eq!(
            zeroship_runtime::callback::caught_panics() - panics_before,
            1,
            "the cancellation ran the listener, which reached the callback: {answered:?}"
        );
        assert!(
            runtime.is_quarantined(),
            "the isolate is stopped before the cancelled request is answered: {answered:?}"
        );
    });
}

/// A pump window in which a callback panicked, and which also overran the
/// pump's CPU share, stops the isolate for the panic: the pending request is
/// failed with `internal error`, the platform's defect, rather than blamed on
/// the creator as a CPU overrun.
#[test]
fn a_panic_in_a_pump_window_that_overruns_the_pump_share_stops_for_the_panic() {
    in_own_process!(a_panic_in_a_pump_window_that_overruns_the_pump_share_stops_for_the_panic, {
        init_v8();
        let runtime = Runtime::builder()
            .plugin(PanickingPlugin)
            .pump_cpu_budget(Duration::from_millis(10), 0.5)
            .modules(m(
                r"
                export default {
                    fetch(request, env) {
                        return new Promise(() => {
                            setTimeout(() => {
                                const end = Date.now() + 150;
                                while (Date.now() < end) {}
                                try { env.boom.explode(); } catch {}
                            }, 1);
                        });
                    }
                };
                ",
            ))
            .build();
        let panics_before = zeroship_runtime::callback::caught_panics();
        let answered = settle(&runtime);
        assert_eq!(zeroship_runtime::callback::caught_panics() - panics_before, 1, "the timer reached the callback");
        assert_eq!(answered, (500, "internal error".to_owned()), "the stop names the panic");
        assert!(runtime.is_quarantined(), "the isolate the panic happened in is quarantined");
    });
}

/// A callback that panics inside `Runtime::with_scope` quarantines the isolate
/// once the scope is released. The worker's eviction runs every in-flight
/// RPC's `abort` listeners through that door, so the case drives it there.
#[test]
fn a_panic_inside_with_scope_quarantines_the_isolate() {
    in_own_process!(a_panic_inside_with_scope_quarantines_the_isolate, {
        let runtime = panicking_runtime(ABORT_LISTENER_APP);
        let _pending = start_pending_procedure(&runtime);
        assert_eq!(runtime.abort_registry_len(), 1, "the premise: one signal to abort");
        let panics_before = zeroship_runtime::callback::caught_panics();
        runtime.entered_for_eviction();
        assert_eq!(
            zeroship_runtime::callback::caught_panics() - panics_before,
            1,
            "the eviction ran the listener, which reached the callback"
        );
        assert!(runtime.is_quarantined(), "the isolate the panic happened in is quarantined");
    });
}

/// A callback that panics while the module graph evaluates fails the startup
/// and quarantines the isolate, even though the module caught the error the
/// panic became. A host that loads an app through `Runtime::initialize` is
/// refused, so it never keeps an isolate a callback panicked in.
#[test]
fn a_panic_while_the_modules_evaluate_fails_startup_and_quarantines_the_isolate() {
    in_own_process!(a_panic_while_the_modules_evaluate_fails_startup_and_quarantines_the_isolate, {
        let runtime = panicking_runtime(
            r#"
            import { env } from "zeroship";
            try { env.boom.explode(); } catch {}
            export default { fetch() { return new Response("served"); } };
            "#,
        );
        runtime.exit_isolate();
        let panics_before = zeroship_runtime::callback::caught_panics();
        let started = compio::runtime::Runtime::new()
            .expect("compio runtime")
            .block_on(runtime.initialize(&EnvSnapshot::empty()));
        assert_eq!(
            zeroship_runtime::callback::caught_panics() - panics_before,
            1,
            "the module's top level reached the callback"
        );
        let error = started.expect_err("a startup a callback panicked in is refused");
        assert!(error.contains("internal error"), "the stop is the cause: {error}");
        assert!(runtime.is_quarantined(), "the isolate the panic happened in is quarantined");
    });
}

/// Every function the runtime puts on the global object (and one level of
/// namespaces below it, such as `console`), called with the isolate's
/// runtime-state slot taken away. The runtime's own callbacks that read that
/// slot panic, which stands in for any defect a creator can reach; each of
/// those panics must come back as a thrown `Error("internal error")`, and the
/// walk must finish rather than abort the process. Registration goes through
/// `zeroship_runtime::callback`, which always installs that boundary, so a
/// builtin registered around it would abort this walk.
#[test]
fn every_global_builtin_answers_a_forced_panic_with_a_thrown_error() {
    in_own_process!(every_global_builtin_answers_a_forced_panic_with_a_thrown_error, {
        init_v8();
        let runtime = Runtime::builder()
            .modules(m(r#"export default { fetch() { return new Response("ready"); } };"#))
            .build();
        assert_eq!(settle(&runtime), (200, "ready".to_owned()), "the runtime installs its globals");
        let (json, panics) = runtime.with_scope(|scope| {
            let state = scope
                .remove_slot::<zeroship_runtime::state::SharedState>()
                .expect("the runtime installs its state slot");
            let source = v8::String::new(
                scope,
                r#"
                const out = {};
                const seen = new Set([globalThis]);
                function walk(holder, path, depth) {
                    for (const name of Object.getOwnPropertyNames(holder)) {
                        let value;
                        try { value = holder[name]; } catch { continue; }
                        if (seen.has(value)) continue;
                        const label = path ? `${path}.${name}` : name;
                        if (typeof value === "function") {
                            seen.add(value);
                            try { value.call(holder); out[label] = "returned"; }
                            catch (e) { out[label] = `${e && e.name}: ${e && e.message}`; }
                        } else if (value && typeof value === "object" && depth === 0) {
                            seen.add(value);
                            walk(value, label, 1);
                        }
                    }
                }
                walk(globalThis, "", 0);
                JSON.stringify(out)
                "#,
            )
            .expect("script source");
            let script = v8::Script::compile(scope, source, None).expect("the walk compiles");
            let before = zeroship_runtime::callback::caught_panics();
            let json = script.run(scope).expect("the walk completes").to_rust_string_lossy(scope);
            let panics = zeroship_runtime::callback::caught_panics() - before;
            scope.set_slot(state);
            (json, panics)
        });
        let pairs = outcomes(&json);
        let answered: Vec<&str> = pairs
            .iter()
            .filter(|(_, outcome)| outcome == "Error: internal error")
            .map(|(label, _)| label.as_str())
            .collect();
        for builtin in ["clearInterval", "clearTimeout", "console.log", "setInterval", "setTimeout"] {
            assert!(answered.contains(&builtin), "{builtin} must answer its forced panic: {json}");
        }
        assert_eq!(
            panics,
            u64::try_from(answered.len()).expect("a count"),
            "every internal error is a caught panic, and every caught panic was answered: {json}",
        );
    });
}
