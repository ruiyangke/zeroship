// Phase 4 — Vercel AI-SDK Data Stream Protocol wire tests.
//
// Each line of the SSE response is `<typeId>:<json>\n`:
//
//   0:"text"               — text part (when output is string)
//   2:[<json>]             — typed object yield (when output is object)
//   3:"err msg"            — error message string
//   e:{...}                — structured error envelope (zeroship extension)
//   d:{}                   — done
//
// These tests drive the bootstrap's `sseFromAsyncGen` directly via the
// `/_rpc/<method>` fast path. They're the contract the synthetic entry
// (in @zeroship/vite-plugin) and the client SDK both honor.

mod common;
use common::*;

#[test]
fn stream_string_output_emits_zero_lines_then_done() {
    // An async generator yielding strings produces `0:` lines per yield
    // and a final `d:{}` (no return-value emission per the proposal).
    let r = dispatch(
        m(r#"
            export async function* sayHello() {
                yield "Hi";
                yield " there";
            }
        "#),
        "sayHello",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains("0:\"Hi\"\n"),
        "expected 0:\"Hi\" line, got: {}",
        r.json
    );
    assert!(
        r.json.contains("0:\" there\"\n"),
        "expected 0:\" there\" line, got: {}",
        r.json
    );
    assert!(r.json.contains("d:{}\n"), "expected d:{{}} done, got: {}", r.json);
    // The legacy `event: yield` shape must not appear.
    assert!(
        !r.json.contains("event: yield"),
        "legacy SSE shape leaked into wire: {}",
        r.json
    );
}

#[test]
fn stream_object_output_emits_two_lines() {
    // Object yields are framed as `2:[<json>]\n`.
    let r = dispatch(
        m(r#"
            export async function* todos() {
                yield { id: 1, text: "first" };
                yield { id: 2, text: "second" };
            }
        "#),
        "todos",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains("2:[{\"id\":1,\"text\":\"first\"}]\n"),
        "expected 2:[{{...}}] for first yield, got: {}",
        r.json
    );
    assert!(
        r.json.contains("2:[{\"id\":2,\"text\":\"second\"}]\n"),
        "expected 2:[{{...}}] for second yield, got: {}",
        r.json
    );
    assert!(r.json.contains("d:{}\n"), "expected d:{{}} done, got: {}", r.json);
}

#[test]
fn stream_mid_error_emits_envelope_then_done() {
    // A mid-stream throw emits an `e:` envelope with the structured
    // error metadata, followed by a final `d:{}`. The envelope must
    // carry message/name; code/details/retryable are optional.
    let r = dispatch(
        m(r#"
            export async function* boom() {
                yield { id: 1 };
                const err = new Error("kaboom");
                err.code = "INTERNAL";
                err.details = { hint: "demo" };
                err.retryable = false;
                throw err;
            }
        "#),
        "boom",
        "[]",
    )
    .unwrap();
    assert!(
        r.json.contains("2:[{\"id\":1}]\n"),
        "expected first yield, got: {}",
        r.json
    );
    // The error frame is `e:` (zeroship-extension envelope).
    let e_idx = r
        .json
        .find("e:")
        .unwrap_or_else(|| panic!("expected `e:` envelope in: {}", r.json));
    let e_line_end = r.json[e_idx..].find('\n').unwrap();
    let e_line = &r.json[e_idx..e_idx + e_line_end];
    let env_json = &e_line[2..]; // strip `e:`
    let parsed: serde_json::Value = serde_json::from_str(env_json)
        .unwrap_or_else(|_| panic!("e: envelope not JSON: {}", env_json));
    assert_eq!(parsed["message"], "kaboom");
    assert_eq!(parsed["code"], "INTERNAL");
    assert_eq!(parsed["details"]["hint"], "demo");
    assert_eq!(parsed["retryable"], false);
    // After the error envelope, a `d:{}` always closes the stream.
    let after_err = &r.json[e_idx + e_line_end..];
    assert!(
        after_err.contains("d:{}\n"),
        "expected d:{{}} after error, got: {}",
        r.json
    );
}

#[test]
fn stream_string_output_yields_non_string_still_uses_zero() {
    // The runtime's heuristic (when no schema is declared): if the
    // first yield is a string, use `0:`; if it's an object, use `2:`.
    // For uniformity within a stream we use the per-value typeof check,
    // matching the encoder spec in §6 of the proposal.
    let r = dispatch(
        m(r#"
            export async function* mixed() {
                yield "alpha";
                yield 42;
            }
        "#),
        "mixed",
        "[]",
    )
    .unwrap();
    // First yield (string) → `0:"alpha"`.
    assert!(
        r.json.contains("0:\"alpha\"\n"),
        "expected 0:\"alpha\" line, got: {}",
        r.json
    );
    // Second yield (number) → fall back to `2:[42]`.
    assert!(
        r.json.contains("2:[42]\n"),
        "expected 2:[42] line for non-string, got: {}",
        r.json
    );
    assert!(r.json.contains("d:{}\n"), "expected d:{{}} done, got: {}", r.json);
}
