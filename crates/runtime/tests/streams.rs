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
