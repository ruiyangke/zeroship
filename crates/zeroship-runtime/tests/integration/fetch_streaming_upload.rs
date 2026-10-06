//! `fetch()` with a `ReadableStream` request body, driven from JS through a
//! real `Runtime` against a loopback server that records what arrives and
//! when. The same suite covers the request-body rules a streamed upload
//! depends on: the Request constructor's proxy for an inherited stream body,
//! `Request.clone()` keeping a byte body's source, and an abort while the
//! request is being sent.
//!
//! The server never sleeps. A handler that waits for an event (a release,
//! the first bytes of a body) waits on a condition variable with a deadline,
//! and the deadline is reached only when the behaviour under test is broken.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use futures::channel::oneshot;
use futures::stream::StreamExt;
use zeroship_runtime::channel::CancelFlag;
use zeroship_runtime::plugin::{NativePlugin, NativeRegistrar};
use zeroship_runtime::state::{SharedState, MAX_UPLOAD_BUFFER_BYTES};
use zeroship_runtime::streams::stream_forwarder::UploadReader;
use zeroship_runtime::{
    init_v8, EnvSnapshot, FetchOutcome, ModuleEntry, RequestCtx, Runtime, SettledFetch,
};

/// How long a server handler waits for an event only a failing run lacks.
const HOLD_MS: u64 = 20_000;
const HOLD: Duration = Duration::from_millis(HOLD_MS);

/// How long the JS handler may take to settle.
const SETTLE: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Loopback server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Upload {
    chunked: bool,
    chunks: usize,
    bytes: usize,
    terminated: bool,
    pattern_ok: Option<bool>,
    tail: String,
}

impl Upload {
    fn json(&self) -> String {
        let pattern = match self.pattern_ok {
            Some(ok) => ok.to_string(),
            None => "null".to_string(),
        };
        format!(
            r#"{{"chunked":{},"chunks":{},"bytes":{},"terminated":{},"pattern_ok":{},"tail":"{}"}}"#,
            self.chunked,
            self.chunks,
            self.bytes,
            self.terminated,
            pattern,
            json_text(&self.tail),
        )
    }
}

#[derive(Default)]
struct Observed {
    /// Body bytes received so far by the upload in progress.
    progress: usize,
    /// Every upload that ended, cleanly or not.
    uploads: Vec<Upload>,
    released: bool,
    hang_seen: bool,
    hang_answered: bool,
    /// What `/early-keepalive` saw after answering: bytes it discarded,
    /// and whether the client then closed the connection.
    early: Option<(usize, bool)>,
    /// Set once a request waits in `/wait-released`.
    waiting_release: bool,
    /// Reads `/early-stepped` may make, and has made, one per step.
    steps: usize,
    consumed: usize,
}

type Waiter = (Box<dyn Fn(&Observed) -> bool + Send>, oneshot::Sender<()>);

struct Shared {
    observed: Mutex<Observed>,
    changed: Condvar,
    /// Rust-side waiters, fired from the server thread once their condition
    /// holds, so an async test can wait without blocking its runtime.
    waiters: Mutex<Vec<Waiter>>,
}

impl Shared {
    fn update(&self, f: impl FnOnce(&mut Observed)) {
        let mut observed = self.observed.lock().unwrap();
        f(&mut observed);
        let mut waiters = self.waiters.lock().unwrap();
        let mut pending = Vec::new();
        for (ready, tx) in waiters.drain(..) {
            if ready(&observed) {
                let _ = tx.send(());
            } else {
                pending.push((ready, tx));
            }
        }
        *waiters = pending;
        drop(waiters);
        self.changed.notify_all();
    }

    /// Wait until `ready` holds or `deadline` passes, then read the state along
    /// with whether `ready` held.
    fn wait_until<R>(
        &self,
        deadline: Duration,
        ready: impl Fn(&Observed) -> bool,
        read: impl FnOnce(&Observed, bool) -> R,
    ) -> R {
        let observed = self.observed.lock().unwrap();
        let (observed, _) = self.changed.wait_timeout_while(observed, deadline, |o| !ready(o)).unwrap();
        let held = ready(&observed);
        read(&observed, held)
    }
}

struct Server {
    base: String,
    shared: Arc<Shared>,
}

impl Server {
    /// Resolves once `ready` holds for the server's observations.
    fn when(&self, ready: impl Fn(&Observed) -> bool + Send + 'static) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        let observed = self.shared.observed.lock().unwrap();
        if ready(&observed) {
            let _ = tx.send(());
        } else {
            self.shared.waiters.lock().unwrap().push((Box::new(ready), tx));
        }
        rx
    }

    fn observe<R>(&self, read: impl FnOnce(&Observed) -> R) -> R {
        read(&self.shared.observed.lock().unwrap())
    }
}

fn start_server() -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let shared = Arc::new(Shared {
        observed: Mutex::new(Observed::default()),
        changed: Condvar::new(),
        waiters: Mutex::new(Vec::new()),
    });
    let accept_shared = shared.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { break };
            let shared = accept_shared.clone();
            std::thread::spawn(move || handle(stream, &shared));
        }
    });
    Server { base, shared }
}

struct Head {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    /// Every header line as sent, duplicates included.
    lines: Vec<(String, String)>,
}

impl Head {
    fn query_usize(&self, key: &str) -> Option<usize> {
        self.query.get(key).and_then(|v| v.parse().ok())
    }

    fn hold(&self) -> Duration {
        self.query_usize("ms").map_or(HOLD, |ms| Duration::from_millis(ms as u64))
    }
}

fn read_head(reader: &mut BufReader<TcpStream>) -> Option<Head> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = HashMap::new();
    let mut lines = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).ok()? == 0 {
            return None;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            lines.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    let (path, query_text) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = query_text
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Some(Head { method, path: path.to_string(), query, headers, lines })
}

/// Read the request body, passing each piece to `on_bytes` as it arrives.
/// Returns whether the body ended as framed (the chunked terminator, or the
/// declared length) and the number of chunked-coding chunks.
fn read_body(
    reader: &mut BufReader<TcpStream>,
    head: &Head,
    mut on_bytes: impl FnMut(&[u8]),
) -> (bool, usize) {
    let mut buf = vec![0u8; 64 * 1024];
    let chunked = head
        .headers
        .get("transfer-encoding")
        .is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
    if chunked {
        let mut chunks = 0;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return (false, chunks),
                Ok(_) => {}
            }
            let size_text = line.trim_end().split(';').next().unwrap_or("");
            let Ok(size) = usize::from_str_radix(size_text, 16) else {
                return (false, chunks);
            };
            if size == 0 {
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return (false, chunks),
                        Ok(_) if line == "\r\n" => return (true, chunks),
                        Ok(_) => {}
                    }
                }
            }
            let mut left = size;
            while left > 0 {
                let want = left.min(buf.len());
                let n = match reader.read(&mut buf[..want]) {
                    Ok(0) | Err(_) => return (false, chunks),
                    Ok(n) => n,
                };
                on_bytes(&buf[..n]);
                left -= n;
            }
            let mut crlf = [0u8; 2];
            if reader.read_exact(&mut crlf).is_err() {
                return (false, chunks);
            }
            chunks += 1;
        }
    }
    let mut left = head.headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0usize);
    while left > 0 {
        let want = left.min(buf.len());
        let n = match reader.read(&mut buf[..want]) {
            Ok(0) | Err(_) => return (false, 0),
            Ok(n) => n,
        };
        on_bytes(&buf[..n]);
        left -= n;
    }
    (true, 0)
}

fn respond(stream: &mut TcpStream, status: u16, extra: &[(&str, &str)], body: &str) {
    let mut head = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

const JSON: (&str, &str) = ("Content-Type", "application/json");

fn json_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '"' | '\\' => format!("\\{c}"),
            c if c.is_ascii_graphic() || c == ' ' => c.to_string(),
            _ => ".".to_string(),
        })
        .collect()
}

fn header_json(head: &Head, name: &str) -> String {
    match head.headers.get(name) {
        Some(value) => format!("\"{}\"", json_text(value)),
        None => "null".to_string(),
    }
}

fn handle(stream: TcpStream, shared: &Shared) {
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let Some(head) = read_head(&mut reader) else { return };
    match head.path.as_str() {
        // A recorded upload. `chunk=N` checks the byte pattern the JS
        // producers use: the byte at offset o is ((o / N) * 7 + 1) & 0xff.
        // `/upload-gated` reads nothing until released; `/upload-hang`
        // reads the body and answers only once released.
        "/upload" | "/upload-gated" | "/upload-hang" => {
            if head.path == "/upload-gated" {
                shared.wait_until(head.hold(), |o| o.released, |_, _| ());
            }
            shared.update(|o| o.progress = 0);
            let pattern_chunk = head.query_usize("chunk");
            let positional = head.query.get("pattern").is_some_and(|p| p == "pos");
            let mut offset = 0usize;
            let mut pattern_ok = true;
            let mut tail: Vec<u8> = Vec::new();
            let (terminated, chunks) = read_body(&mut reader, &head, |bytes| {
                for (i, byte) in bytes.iter().enumerate() {
                    let o = offset + i;
                    let expected = if positional {
                        Some(positional_byte(o))
                    } else {
                        pattern_chunk.map(|chunk| u8::try_from(((o / chunk) * 7 + 1) & 0xff).unwrap())
                    };
                    if expected.is_some_and(|expected| *byte != expected) {
                        pattern_ok = false;
                    }
                }
                offset += bytes.len();
                tail.extend_from_slice(bytes);
                if tail.len() > 64 {
                    tail.drain(..tail.len() - 64);
                }
                shared.update(|o| o.progress = offset);
            });
            let upload = Upload {
                chunked: head
                    .headers
                    .get("transfer-encoding")
                    .is_some_and(|v| v.eq_ignore_ascii_case("chunked")),
                chunks,
                bytes: offset,
                terminated,
                pattern_ok: (positional || pattern_chunk.is_some()).then_some(pattern_ok),
                tail: String::from_utf8_lossy(&tail).into_owned(),
            };
            let json = upload.json();
            shared.update(|o| o.uploads.push(upload));
            if !terminated {
                return;
            }
            if head.path == "/upload-hang" {
                shared.wait_until(head.hold(), |o| o.released, |_, _| ());
            }
            respond(&mut writer, 200, &[JSON], &json);
        }
        // Long-poll: answers once the upload in progress has `bytes` bytes.
        "/wait" => {
            let bytes = head.query_usize("bytes").unwrap_or(1);
            let json = shared.wait_until(head.hold(), |o| o.progress >= bytes, |o, held| {
                format!(r#"{{"reached":{held},"bytes":{}}}"#, o.progress)
            });
            respond(&mut writer, 200, &[JSON], &json);
        }
        // Answers 413 before reading the body, keeps the connection open,
        // then makes one read per step granted until released, and records
        // the body it got.
        "/early-stepped" => {
            let body = "too large";
            let head_text = format!("HTTP/1.1 413 X\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            let _ = writer.write_all(head_text.as_bytes());
            let mut bytes = 0usize;
            let mut granted = 0usize;
            let (terminated, _) = read_body(&mut reader, &head, |b| {
                bytes += b.len();
                granted += 1;
                shared.update(|o| o.consumed = granted);
                shared.wait_until(head.hold(), |o| o.released || o.steps > granted, |_, _| ());
            });
            shared.update(|o| o.early = Some((bytes, terminated)));
        }
        // Long-poll: what `/early-keepalive` or `/early-stepped` saw.
        "/early-outcome" => {
            let json = shared.wait_until(head.hold(), |o| o.early.is_some(), |o, _| match o.early {
                Some((discarded, closed)) => format!(r#"{{"discarded":{discarded},"closed":{closed}}}"#),
                None => "{}".to_string(),
            });
            respond(&mut writer, 200, &[JSON], &json);
        }
        // Long-poll: the last upload that ended.
        "/outcome" => {
            let json = shared.wait_until(head.hold(), |o| !o.uploads.is_empty(), |o, held| {
                if held { o.uploads.last().unwrap().json() } else { "{}".to_string() }
            });
            respond(&mut writer, 200, &[JSON], &json);
        }
        // Long-poll: answers once released.
        "/wait-released" => {
            shared.update(|o| o.waiting_release = true);
            let json = shared.wait_until(head.hold(), |o| o.released, |_, held| format!(r#"{{"released":{held}}}"#));
            respond(&mut writer, 200, &[JSON], &json);
        }
        "/release" => {
            shared.update(|o| o.released = true);
            respond(&mut writer, 200, &[], "released");
        }
        "/ping" => respond(&mut writer, 200, &[], "pong"),
        "/echo" => {
            let mut body = Vec::new();
            let (terminated, _) = read_body(&mut reader, &head, |b| body.extend_from_slice(b));
            let framing: Vec<String> = head
                .lines
                .iter()
                .filter(|(name, _)| name == "content-length" || name == "transfer-encoding")
                .map(|(name, value)| format!("\"{}: {}\"", json_text(name), json_text(value)))
                .collect();
            let json = format!(
                r#"{{"method":"{}","contentLength":{},"transferEncoding":{},"framing":[{}],"body":"{}","terminated":{}}}"#,
                head.method,
                header_json(&head, "content-length"),
                header_json(&head, "transfer-encoding"),
                framing.join(","),
                json_text(&String::from_utf8_lossy(&body)),
                terminated,
            );
            respond(&mut writer, 200, &[JSON], &json);
        }
        path if path.starts_with("/redirect/") => {
            read_body(&mut reader, &head, |_| {});
            let status = path["/redirect/".len()..].parse().unwrap_or(500);
            respond(&mut writer, status, &[("Location", "/echo")], "");
        }
        // Reads the whole request, then answers only once released.
        "/hang" => {
            read_body(&mut reader, &head, |_| {});
            shared.update(|o| o.hang_seen = true);
            shared.wait_until(head.hold(), |o| o.released, |_, _| ());
            shared.update(|o| o.hang_answered = true);
            respond(&mut writer, 200, &[], "late");
        }
        "/hang-seen" => {
            let json = shared.wait_until(head.hold(), |o| o.hang_seen, |_, held| {
                format!(r#"{{"seen":{held}}}"#)
            });
            respond(&mut writer, 200, &[JSON], &json);
        }
        // Whether `/hang` has answered yet; never waits.
        "/hang-status" => {
            let answered = shared.observed.lock().unwrap().hang_answered;
            respond(&mut writer, 200, &[JSON], &format!(r#"{{"answered":{answered}}}"#));
        }
        // Reads `bytes` raw body bytes, then drops the connection without
        // answering.
        "/drop" => {
            let want = head.query_usize("bytes").unwrap_or(1);
            let mut buf = [0u8; 4096];
            let mut got = 0;
            while got < want {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got += n,
                }
            }
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
        // Answers 413 before reading any of the body, keeps the connection
        // open, then reads and discards whatever the client still sends and
        // records whether the client closed the connection.
        "/early-keepalive" => {
            let body = "too large";
            let head_text = format!("HTTP/1.1 413 X\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            let _ = writer.write_all(head_text.as_bytes());
            let mut buf = vec![0u8; 64 * 1024];
            let mut discarded = 0usize;
            let closed = loop {
                match reader.read(&mut buf) {
                    Ok(0) => break true,
                    Ok(n) => {
                        discarded += n;
                        shared.update(|o| o.progress = discarded);
                    }
                    Err(_) => break true,
                }
            };
            shared.update(|o| o.early = Some((discarded, closed)));
        }
        // Answers 413 before reading any of the body, closes its sending side,
        // and then reads (and discards) whatever the client still sends.
        "/early" => {
            let body = "too large";
            let head_text = format!(
                "HTTP/1.1 413 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = writer.write_all(head_text.as_bytes());
            let _ = writer.shutdown(std::net::Shutdown::Write);
            let mut buf = vec![0u8; 64 * 1024];
            while matches!(reader.read(&mut buf), Ok(n) if n > 0) {}
        }
        _ => respond(&mut writer, 404, &[], "not found"),
    }
}

// ---------------------------------------------------------------------------
// JS harness
// ---------------------------------------------------------------------------

/// Run `body` (the statements of an async function returning a JSON-able
/// value) as a fetch handler, with `BASE` bound to the server's origin.
async fn run_app(server: &Server, body: &str) -> serde_json::Value {
    run_app_with_heap(server, body, None).await
}

/// [`run_app`] with an explicit V8 heap limit, for a handler that holds as
/// many streams as the isolate's stream budget allows: under the default
/// heap V8 terminates the isolate first.
async fn run_app_with_heap(server: &Server, body: &str, heap_limit_mb: Option<u32>) -> serde_json::Value {
    let (_runtime, outcome) = launch(server, body, heap_limit_mb, CancelFlag::new());
    settle(outcome).await
}

/// `env.probe.uploadBytes()`: the bytes this runtime's uploads hold right
/// now (their channels and parked chunks), read from the runtime's upload
/// share. `env.probe.inFlightFetches()`: the fetches the runtime counts as in
/// flight. `env.probe.mark(name)` records that `name` happened, for a test
/// that outlives its handler.
struct ProbePlugin;

thread_local! {
    static MARKS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    static UPLOADS: std::cell::RefCell<Vec<zeroship_runtime::streams::stream_forwarder::UploadReader>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `env.probe.startUpload(stream, signal)`: start an upload of `stream`
/// that follows `signal`, the way `fetch` starts one, and keep its reader
/// alive. Returns the refusal message, or undefined.
fn start_upload_callback(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let (Ok(stream), Ok(signal)) =
        (v8::Local::<v8::Object>::try_from(args.get(0)), v8::Local::<v8::Object>::try_from(args.get(1)))
    else {
        return;
    };
    // The upload belongs to the request whose handler started it, the same
    // as a `fetch` request body: the request ending must end the upload.
    let owner_request = scope
        .get_slot::<SharedState>()
        .and_then(|state| state.borrow().executing_request_id);
    let options = zeroship_runtime::streams::stream_forwarder::UploadOptions {
        buffer_cap: zeroship_runtime::channel::DEFAULT_STREAM_BUFFER_CAP,
        chunk_policy: zeroship_runtime::streams::stream_forwarder::ChunkPolicy::Uint8Array,
        abort_signal: Some(signal),
        owner_request,
    };
    match zeroship_runtime::streams::stream_forwarder::forward_upload(scope, stream, options) {
        Ok(reader) => UPLOADS.with(|uploads| uploads.borrow_mut().push(reader)),
        Err(message) => {
            if let Some(text) = v8::String::new(scope, &message) {
                rv.set(text.into());
            }
        }
    }
}

fn upload_bytes_callback(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let used = scope.get_slot::<SharedState>().map_or(0, |state| state.borrow().upload_share.used());
    rv.set(v8::Number::new(scope, used as f64).into());
}

fn in_flight_fetches_callback(scope: &mut v8::PinScope, _args: v8::FunctionCallbackArguments, mut rv: v8::ReturnValue) {
    let count = scope.get_slot::<SharedState>().map_or(0, |state| state.borrow().in_flight_fetches.get());
    rv.set(v8::Number::new(scope, count as f64).into());
}

fn mark_callback(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue) {
    let name = args.get(0).to_rust_string_lossy(scope);
    MARKS.with(|marks| marks.borrow_mut().push(name));
}

fn marks() -> Vec<String> {
    MARKS.with(|marks| marks.borrow().clone())
}

impl NativePlugin for ProbePlugin {
    fn namespace(&self) -> &str {
        "probe"
    }

    fn register(&self, r: &mut NativeRegistrar) {
        r.add("uploadBytes", upload_bytes_callback);
        r.add("mark", mark_callback);
        r.add("startUpload", start_upload_callback);
        r.add("inFlightFetches", in_flight_fetches_callback);
    }
}

/// The byte at offset `o` of a `pattern=pos` body: changes within a chunk,
/// so a reordered or shifted slice of one chunk is detected.
fn positional_byte(o: usize) -> u8 {
    u8::try_from((o * 7 + o / 4096) & 0xff).unwrap()
}

/// The JS twin of [`positional_byte`], for handler bodies.
const POSITIONAL_JS: &str = "function positionalByte(o) { return (o * 7 + Math.floor(o / 4096)) & 0xff; }";

/// Start `body` as a fetch handler in a fresh runtime whose request carries
/// `cancel`. `BASE` is the server's origin and `env` the handler's env.
fn launch(
    server: &Server,
    body: &str,
    heap_limit_mb: Option<u32>,
    cancel: CancelFlag,
) -> (Runtime, FetchOutcome) {
    // Loopback is reachable only in dev mode; every module in this target
    // that fetches sets it one-way.
    zeroship_runtime::set_dev_mode(true);
    init_v8();
    let source = format!(
        "const BASE = {:?};\n\
         {POSITIONAL_JS}\n\
         export default {{\n\
           async fetch(_request, env) {{\n\
             let out;\n\
             try {{ out = await (async () => {{ {body} }})(); }}\n\
             catch (e) {{ out = {{ uncaught: String(e && e.stack || e) }}; }}\n\
             return new Response(JSON.stringify(out), {{ headers: {{ \"content-type\": \"application/json\" }} }});\n\
           }}\n\
         }};\n",
        server.base,
    );
    let mut builder = Runtime::builder()
        .modules(vec![ModuleEntry { specifier: "index.js".to_string(), source }])
        .plugins(vec![Arc::new(ProbePlugin) as Arc<dyn NativePlugin>]);
    if let Some(mb) = heap_limit_mb {
        builder = builder.heap_limit_mb(mb);
    }
    let runtime = builder.build();
    runtime.start_pump();
    let outcome = runtime.call_fetch_handler(
        "GET",
        "http://localhost/",
        &[],
        "",
        &EnvSnapshot::empty(),
        RequestCtx::new(cancel),
    );
    (runtime, outcome)
}

/// The handler's JSON output; fails the test if it threw.
async fn settle(outcome: FetchOutcome) -> serde_json::Value {
    let text = match outcome {
        FetchOutcome::Response { body, .. } => String::from_utf8_lossy(&body).into_owned(),
        FetchOutcome::Pending { rx, .. } => match compio::time::timeout(SETTLE, rx.recv()).await {
            Ok(Ok(SettledFetch::Response { body, .. })) => String::from_utf8_lossy(&body).into_owned(),
            Ok(Ok(_)) => panic!("handler settled as a stream or upgrade, expected a Response"),
            Ok(Err(_)) => panic!("settled-fetch channel closed before a Response arrived"),
            Err(elapsed) => panic!("handler did not settle within {SETTLE:?}: {elapsed}"),
        },
        _ => panic!("handler neither returned nor pended a Response"),
    };
    let value: serde_json::Value =
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("handler output is not JSON ({e}): {text}"));
    assert!(value.get("uncaught").is_none(), "handler threw: {value}");
    value
}

// ---------------------------------------------------------------------------
// Streamed upload
// ---------------------------------------------------------------------------

/// The second chunk is produced only after the server has answered whether
/// it already holds the first, so a buffered body could only report
/// "UNSEEN".
fn first_chunk_app(threshold: usize, wait_ms: u64) -> String {
    format!(
        r#"
        let n = 0;
        const s = new ReadableStream({{
            async pull(c) {{
                if (n === 0) {{ n++; c.enqueue(new Uint8Array(1000).fill(97)); return; }}
                if (n === 1) {{
                    n++;
                    const w = await (await fetch(BASE + "/wait?bytes={threshold}&ms={wait_ms}")).json();
                    c.enqueue(new TextEncoder().encode(w.reached ? "SEEN" : "UNSEEN"));
                    return;
                }}
                c.close();
            }}
        }});
        const resp = await fetch(BASE + "/upload", {{ method: "POST", body: s, duplex: "half" }});
        return {{ status: resp.status, upload: await resp.json() }};
        "#
    )
}

#[compio::test]
async fn fetch_streaming_upload_delivers_first_chunk_before_body_ends() {
    let server = start_server();
    let v = run_app(&server, &first_chunk_app(1000, HOLD_MS)).await;
    assert_eq!(v["status"], 200, "{v}");
    let upload = &v["upload"];
    assert_eq!(upload["chunked"], true, "a stream body is sent with chunked coding: {v}");
    assert_eq!(upload["terminated"], true, "{v}");
    assert_eq!(upload["bytes"], 1004, "{v}");
    let tail = upload["tail"].as_str().unwrap();
    assert!(tail.ends_with("aSEEN"), "server held chunk 1 before chunk 2 existed: {v}");
}

#[compio::test]
async fn fetch_streaming_upload_first_chunk_probe_reports_a_miss() {
    // Control: a threshold no body reaches before its end makes the same
    // probe answer UNSEEN, so the probe above can fail.
    let server = start_server();
    let v = run_app(&server, &first_chunk_app(1_000_000, 200)).await;
    let tail = v["upload"]["tail"].as_str().unwrap();
    assert!(tail.ends_with("aUNSEEN"), "{v}");
}

#[compio::test]
async fn fetch_streaming_upload_is_bounded_while_the_server_does_not_read() {
    // The server holds the body unread. With backpressure the bytes the
    // platform holds for the upload stop at its channel's cap plus one parked
    // chunk; without it, the producer runs to the end of the stream inside
    // one pump turn and the platform holds all of it.
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const CHUNK = 256 * 1024, TOTAL = 64 * 1024 * 1024;
        let produced = 0, i = 0;
        const s = new ReadableStream({
            pull(c) {
                if (produced >= TOTAL) { c.close(); return; }
                c.enqueue(new Uint8Array(CHUNK).fill((i * 7 + 1) & 0xff));
                i++; produced += CHUNK;
            }
        });
        const upload = fetch(BASE + "/upload-gated?chunk=" + CHUNK, { method: "POST", body: s, duplex: "half" });
        await (await fetch(BASE + "/ping")).text();
        await (await fetch(BASE + "/ping")).text();
        const heldBytes = env.probe.uploadBytes();
        const producedWhileHeld = produced;
        await (await fetch(BASE + "/release")).text();
        const resp = await upload;
        return { heldBytes, producedWhileHeld, produced, total: TOTAL, chunk: CHUNK, upload: await resp.json() };
        "#,
    )
    .await;
    let held_bytes = v["heldBytes"].as_u64().unwrap();
    let chunk = v["chunk"].as_u64().unwrap();
    let total = v["total"].as_u64().unwrap();
    let cap = zeroship_runtime::channel::DEFAULT_STREAM_BUFFER_CAP as u64;
    // At rest the channel sits between the producer's resume mark (a
    // quarter of the cap) and its pause mark (half) plus one chunk.
    assert!(held_bytes >= cap / 4, "the upload filled its channel while held: {v}");
    assert!(
        held_bytes <= cap + chunk,
        "the platform holds at most the channel cap plus one parked chunk: {v}"
    );
    assert!(v["producedWhileHeld"].as_u64().unwrap() < total, "the producer stopped before the end: {v}");
    assert_eq!(v["produced"].as_u64().unwrap(), total, "{v}");
    assert_eq!(v["upload"]["bytes"].as_u64().unwrap(), total, "{v}");
    assert_eq!(v["upload"]["pattern_ok"], true, "bytes arrived in order, none lost: {v}");
    assert_eq!(v["upload"]["terminated"], true, "{v}");
}

#[compio::test]
async fn fetch_streaming_upload_accepts_a_chunk_larger_than_its_buffer() {
    // Blob.stream() yields the whole blob as one chunk. The upload slices it
    // to the buffer's free space instead of refusing it.
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const SIZE = 10 * 1024 * 1024;
        const bytes = new Uint8Array(SIZE);
        for (let o = 0; o < SIZE; o++) bytes[o] = positionalByte(o);
        const blob = new Blob([bytes]);
        const resp = await fetch(BASE + "/upload?pattern=pos", { method: "POST", body: blob.stream(), duplex: "half" });
        return { size: SIZE, status: resp.status, upload: await resp.json() };
        "#,
    )
    .await;
    assert_eq!(v["status"], 200, "{v}");
    assert_eq!(v["upload"]["bytes"], v["size"], "{v}");
    assert_eq!(v["upload"]["pattern_ok"], true, "{v}");
    assert_eq!(v["upload"]["terminated"], true, "{v}");
}

#[compio::test]
async fn fetch_streaming_upload_follows_only_303_redirects() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const out = {};
        for (const code of [301, 302, 303, 307, 308]) {
            const s = new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode("streamed")); c.close(); } });
            try {
                const r = await fetch(BASE + "/redirect/" + code, { method: "POST", body: s, duplex: "half" });
                out[code] = { ok: true, redirected: r.redirected, echo: await r.json() };
            } catch (e) {
                out[code] = { ok: false, name: e.name, message: String(e.message) };
            }
        }
        const r = await fetch(BASE + "/redirect/307", { method: "POST", body: "payload" });
        out.bytes307 = await r.json();
        return out;
        "#,
    )
    .await;
    for code in ["301", "302", "307", "308"] {
        let result = &v[code];
        assert_eq!(result["ok"], false, "{code} must not follow with a stream body: {v}");
        assert_eq!(result["name"], "TypeError", "{v}");
        assert!(
            result["message"].as_str().unwrap().contains("cannot resend a ReadableStream request body"),
            "{code} fails on the redirect rule: {v}"
        );
    }
    let see_other = &v["303"];
    assert_eq!(see_other["ok"], true, "{v}");
    assert_eq!(see_other["redirected"], true, "{v}");
    assert_eq!(see_other["echo"]["method"], "GET", "{v}");
    assert_eq!(see_other["echo"]["body"], "", "{v}");
    // Control: a byte body is replayed across a 307.
    assert_eq!(v["bytes307"]["method"], "POST", "{v}");
    assert_eq!(v["bytes307"]["body"], "payload", "{v}");
}

#[compio::test]
async fn fetch_streaming_upload_abort_cancels_the_source_with_the_signal_reason() {
    let server = start_server();
    let v = run_app(
        &server,
        &format!(
            r#"
        const reason = new Error("stop the upload");
        const ac = new AbortController();
        let resolveCancel;
        const cancelled = new Promise(r => {{ resolveCancel = r; }});
        let n = 0;
        const s = new ReadableStream({{
            pull(c) {{
                if (n++ === 0) {{ c.enqueue(new Uint8Array(500).fill(98)); return; }}
                return new Promise(() => {{}});
            }},
            cancel(r) {{ resolveCancel(r); }}
        }});
        const upload = fetch(BASE + "/upload-hang", {{ method: "POST", body: s, duplex: "half", signal: ac.signal }});
        const w = await (await fetch(BASE + "/wait?bytes=500&ms={hold}")).json();
        ac.abort(reason);
        let outcome;
        try {{ await upload; outcome = "resolved"; }}
        catch (e) {{ outcome = e === reason ? "abort-reason" : "other: " + String(e); }}
        const cancelReason = await cancelled;

        // Control: the same upload without an abort completes.
        const done = new ReadableStream({{ start(c) {{ c.enqueue(new Uint8Array(500).fill(98)); c.close(); }} }});
        const resp = await fetch(BASE + "/upload", {{ method: "POST", body: done, duplex: "half", signal: new AbortController().signal }});
        return {{ reached: w.reached, outcome, cancelIsReason: cancelReason === reason, control: resp.status }};
        "#,
            hold = HOLD_MS
        ),
    )
    .await;
    assert_eq!(v["reached"], true, "the server held the first chunk before the abort: {v}");
    assert_eq!(v["outcome"], "abort-reason", "fetch rejects with signal.reason: {v}");
    assert_eq!(v["cancelIsReason"], true, "the source is cancelled with signal.reason: {v}");
    assert_eq!(v["control"], 200, "{v}");
}

#[compio::test]
async fn fetch_streaming_upload_source_error_fails_the_fetch_without_ending_the_body() {
    let server = start_server();
    let v = run_app(
        &server,
        &format!(
            r#"
        let n = 0;
        const s = new ReadableStream({{
            async pull(c) {{
                if (n++ === 0) {{ c.enqueue(new Uint8Array(300).fill(99)); return; }}
                await (await fetch(BASE + "/wait?bytes=300&ms={hold}")).json();
                c.error(new Error("source failed"));
            }}
        }});
        let outcome;
        try {{
            await fetch(BASE + "/upload", {{ method: "POST", body: s, duplex: "half" }});
            outcome = "resolved";
        }} catch (e) {{
            outcome = {{ name: e.name, message: String(e.message) }};
        }}
        const record = await (await fetch(BASE + "/outcome")).json();
        return {{ outcome, record }};
        "#,
            hold = HOLD_MS
        ),
    )
    .await;
    assert_eq!(v["outcome"]["name"], "TypeError", "{v}");
    assert!(v["outcome"]["message"].as_str().unwrap().contains("source failed"), "{v}");
    assert_eq!(v["record"]["bytes"], 300, "the server received the first chunk: {v}");
    assert_eq!(v["record"]["terminated"], false, "a failed source never ends the body: {v}");
}

#[compio::test]
async fn fetch_streaming_upload_rejects_a_chunk_that_is_not_a_uint8array() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        async function attempt(chunk) {
            let resolveCancel;
            const cancelled = new Promise(r => { resolveCancel = r; });
            let pulls = 0;
            const s = new ReadableStream({
                start(c) { c.enqueue(chunk); },
                async pull(c) {
                    // Ends the body a while later, so an accepted chunk
                    // completes the upload instead of leaving it open.
                    if (pulls++ > 0) return;
                    await (await fetch(BASE + "/wait?bytes=" + Number.MAX_SAFE_INTEGER + "&ms=300")).json();
                    try { c.close(); } catch (_) {}
                },
                cancel(r) { resolveCancel(r); }
            });
            let outcome;
            try {
                await fetch(BASE + "/upload", { method: "POST", body: s, duplex: "half" });
                outcome = { name: "resolved" };
            } catch (e) {
                outcome = { name: e.name, message: String(e.message) };
            }
            const reason = outcome.name === "TypeError" ? await cancelled : null;
            return { outcome, cancelName: reason && reason.name };
        }
        const bytes = new Uint8Array([7, 7, 7, 7, 7, 7, 7, 7]);
        return {
            string: await attempt("text"),
            arrayBuffer: await attempt(new ArrayBuffer(8)),
            dataView: await attempt(new DataView(new ArrayBuffer(8))),
            uint8: await attempt(bytes.subarray(2)),
        };
        "#,
    )
    .await;
    for case in ["string", "arrayBuffer", "dataView"] {
        let result = &v[case];
        assert_eq!(result["outcome"]["name"], "TypeError", "{case}: {v}");
        assert!(
            result["outcome"]["message"].as_str().unwrap().contains("not a Uint8Array"),
            "{case}: {v}"
        );
        assert_eq!(result["cancelName"], "TypeError", "{case}: the source is cancelled with the TypeError: {v}");
    }
    // Control: a Uint8Array view is sent.
    assert_eq!(v["uint8"]["outcome"]["name"], "resolved", "{v}");
}

// ---------------------------------------------------------------------------
// Request bodies the upload depends on
// ---------------------------------------------------------------------------

#[compio::test]
async fn fetch_of_a_request_proxies_its_stream_body_and_locks_the_input() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const s = new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode("proxied body")); c.close(); } });
        const req = new Request(BASE + "/upload", { method: "POST", body: s, duplex: "half" });
        const before = { locked: req.body.locked, used: req.bodyUsed };
        const pending = fetch(req);
        const after = { locked: req.body.locked, used: req.bodyUsed, same: req.body === s };
        const resp = await pending;
        const upload = await resp.json();

        // The constructor alone: the new Request reads through the proxy.
        const s2 = new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode("inherited")); c.close(); } });
        const first = new Request(BASE + "/upload", { method: "POST", body: s2, duplex: "half" });
        const second = new Request(first);
        const firstLocked = first.body.locked;
        const text = await second.text();
        return { before, after, upload, firstLocked, firstUsed: first.bodyUsed, text };
        "#,
    )
    .await;
    assert_eq!(v["before"]["locked"], false, "{v}");
    assert_eq!(v["before"]["used"], false, "{v}");
    assert_eq!(v["after"]["locked"], true, "the input's stream is locked by the proxy: {v}");
    assert_eq!(v["after"]["used"], true, "the input's body is disturbed: {v}");
    assert_eq!(v["after"]["same"], true, "the input keeps its own stream: {v}");
    assert_eq!(v["upload"]["tail"], "proxied body", "{v}");
    assert_eq!(v["firstLocked"], true, "{v}");
    assert_eq!(v["firstUsed"], true, "{v}");
    assert_eq!(v["text"], "inherited", "{v}");
}

#[compio::test]
async fn request_proxy_at_the_stream_cap_throws_range_error_and_leaves_the_input_usable() {
    // The proxy's identity TransformStream is charged to the isolate's
    // stream budget before the input's stream is touched: at the cap,
    // `new Request(request)` throws the budget's RangeError and the input
    // keeps an unlocked, unread body.
    let server = start_server();
    let v = run_app_with_heap(
        &server,
        r#"
        const s = new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode("kept")); c.close(); } });
        const req = new Request(BASE + "/upload", { method: "POST", body: s, duplex: "half" });
        const held = [];
        let full = false;
        for (;;) {
            try { held.push(new WritableStream()); }
            catch (e) { if (e instanceof RangeError) { full = true; break; } throw e; }
        }
        let outcome;
        try { new Request(req); outcome = "constructed"; }
        catch (e) { outcome = e.name; }
        return { full, held: held.length, outcome, locked: req.body.locked, used: req.bodyUsed };
        "#,
        Some(512),
    )
    .await;
    assert_eq!(v["full"], true, "{v}");
    assert!(v["held"].as_u64().unwrap() > 0, "{v}");
    assert_eq!(v["outcome"], "RangeError", "{v}");
    assert_eq!(v["locked"], false, "a refused proxy leaves the input unlocked: {v}");
    assert_eq!(v["used"], false, "{v}");
}

#[compio::test]
async fn fetch_of_a_cloned_request_sends_and_replays_its_byte_body() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const r = new Request(BASE + "/echo", { method: "POST", body: "payload" });
        const echo = await (await fetch(r.clone())).json();
        const r307 = new Request(BASE + "/redirect/307", { method: "POST", body: "payload" });
        const replay = await (await fetch(r307.clone())).json();
        return { echo, replay, original: await r.text() };
        "#,
    )
    .await;
    assert_eq!(v["echo"]["method"], "POST", "{v}");
    assert_eq!(v["echo"]["body"], "payload", "{v}");
    assert_eq!(v["echo"]["contentLength"], "7", "a cloned byte body keeps its length: {v}");
    assert_eq!(v["echo"]["transferEncoding"], serde_json::Value::Null, "{v}");
    assert_eq!(v["replay"]["method"], "POST", "{v}");
    assert_eq!(v["replay"]["body"], "payload", "a cloned byte body is replayed on 307: {v}");
    assert_eq!(v["original"], "payload", "the original stays readable: {v}");
}

#[compio::test]
async fn fetch_abort_while_waiting_for_the_response_rejects_with_the_reason() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const reason = new Error("give up waiting");
        const ac = new AbortController();
        const pending = fetch(BASE + "/hang?ms=5000", { method: "POST", body: "x", signal: ac.signal });
        const seen = await (await fetch(BASE + "/hang-seen")).json();
        ac.abort(reason);
        let outcome;
        try { await pending; outcome = "resolved"; }
        catch (e) { outcome = e === reason ? "abort-reason" : "other: " + String(e); }
        const status = await (await fetch(BASE + "/hang-status")).json();
        await (await fetch(BASE + "/release")).text();
        return { seen: seen.seen, outcome, answeredBeforeSettle: status.answered };
        "#,
    )
    .await;
    assert_eq!(v["seen"], true, "the server holds the whole request: {v}");
    assert_eq!(v["outcome"], "abort-reason", "{v}");
    assert_eq!(
        v["answeredBeforeSettle"], false,
        "the abort settled the fetch while the server had not answered: {v}"
    );
}

#[compio::test]
async fn fetch_streaming_upload_honours_a_declared_content_length() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        function body(parts) {
            return new ReadableStream({ start(c) { for (const p of parts) c.enqueue(new TextEncoder().encode(p)); c.close(); } });
        }
        const exact = await (await fetch(BASE + "/echo", {
            method: "PUT", body: body(["abc", "def"]), duplex: "half", headers: { "content-length": "6" },
        })).json();
        let short;
        try {
            await fetch(BASE + "/echo", { method: "PUT", body: body(["abc"]), duplex: "half", headers: { "content-length": "6" } });
            short = { name: "resolved" };
        } catch (e) {
            short = { name: e.name };
        }
        return { exact, short };
        "#,
    )
    .await;
    assert_eq!(v["exact"]["contentLength"], "6", "{v}");
    assert_eq!(v["exact"]["transferEncoding"], serde_json::Value::Null, "{v}");
    assert_eq!(v["exact"]["body"], "abcdef", "{v}");
    assert_eq!(v["short"]["name"], "TypeError", "a stream shorter than its Content-Length fails: {v}");
}

#[compio::test]
async fn fetch_streaming_upload_connection_loss_fails_the_fetch_and_cancels_the_source() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        let resolveCancel;
        const cancelled = new Promise(r => { resolveCancel = r; });
        const s = new ReadableStream({
            pull(c) { c.enqueue(new Uint8Array(64 * 1024).fill(100)); },
            cancel(r) { resolveCancel(r); }
        });
        let outcome;
        try {
            await fetch(BASE + "/drop?bytes=1000", { method: "POST", body: s, duplex: "half" });
            outcome = { name: "resolved" };
        } catch (e) {
            outcome = { name: e.name };
        }
        const reason = await cancelled;
        return { outcome, cancelled: reason !== undefined };
        "#,
    )
    .await;
    assert_eq!(v["outcome"]["name"], "TypeError", "{v}");
    assert_eq!(v["cancelled"], true, "{v}");
}

#[compio::test]
async fn fetch_streaming_upload_answered_early_resolves_and_cancels_the_source() {
    // The server answers before reading the body and stops reading after
    // its answer: the fetch resolves with that answer, and the stream the
    // body would have kept reading is cancelled.
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        let resolveCancel;
        const cancelled = new Promise(r => { resolveCancel = r; });
        const s = new ReadableStream({
            pull(c) { c.enqueue(new Uint8Array(64 * 1024).fill(101)); },
            cancel(r) { resolveCancel(r); }
        });
        let outcome;
        try {
            const resp = await fetch(BASE + "/early", { method: "POST", body: s, duplex: "half" });
            outcome = { status: resp.status, text: await resp.text() };
        } catch (e) {
            outcome = { name: e.name, message: String(e.message) };
        }
        const reason = await cancelled;
        return { outcome, cancelled: reason !== undefined };
        "#,
    )
    .await;
    assert_eq!(v["outcome"]["status"], 413, "{v}");
    assert_eq!(v["outcome"]["text"], "too large", "{v}");
    assert_eq!(v["cancelled"], true, "{v}");
}

// ---------------------------------------------------------------------------
// Request.clone() builds from the intrinsic class
// ---------------------------------------------------------------------------

#[compio::test]
async fn request_clone_ignores_a_replaced_global_request_class() {
    // The clone was built with whatever `globalThis.Request` held, and the
    // result was then written through as a RequestState: a Response here.
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const r = new Request(BASE + "/echo", { method: "POST", body: "payload" });
        const OriginalRequest = Request;
        globalThis.Request = Response;
        let c;
        try { c = r.clone(); } finally { globalThis.Request = OriginalRequest; }
        return {
            isRequest: c instanceof OriginalRequest,
            method: c.method,
            text: await c.text(),
            original: await r.text(),
        };
        "#,
    )
    .await;
    assert_eq!(v["isRequest"], true, "{v}");
    assert_eq!(v["method"], "POST", "{v}");
    assert_eq!(v["text"], "payload", "{v}");
    assert_eq!(v["original"], "payload", "{v}");
}

#[compio::test]
async fn request_clone_ignores_a_global_request_that_returns_the_original() {
    // A replaced constructor returning the receiver made the clone the
    // original, and writing its body while it was still borrowed panicked
    // inside the V8 callback, which aborts the process. Each case runs in its
    // own process under the CI runner, so a regression fails this case alone.
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const r = new Request(BASE + "/echo", { method: "POST", body: "payload" });
        const OriginalRequest = Request;
        globalThis.Request = function () { return r; };
        let c;
        try { c = r.clone(); } finally { globalThis.Request = OriginalRequest; }
        return {
            distinct: c !== r,
            isRequest: c instanceof OriginalRequest,
            text: await c.text(),
            original: await r.text(),
        };
        "#,
    )
    .await;
    assert_eq!(v["distinct"], true, "{v}");
    assert_eq!(v["isRequest"], true, "{v}");
    assert_eq!(v["text"], "payload", "{v}");
    assert_eq!(v["original"], "payload", "{v}");
}

// ---------------------------------------------------------------------------
// An upload belongs to its request and its fetch
// ---------------------------------------------------------------------------

#[compio::test]
async fn an_upload_ends_with_the_request_that_started_it() {
    // The worker's wall timeout cancels the request (and answers 504). The
    // upload the request started must stop reading its source and stop
    // sending, instead of running on after the request is gone.
    MARKS.with(|marks| marks.borrow_mut().clear());
    let server = start_server();
    let first_chunk = server.when(|o| o.progress >= 500);
    let upload_ended = server.when(|o| !o.uploads.is_empty());
    let cancel = CancelFlag::new();
    let (runtime, outcome) = launch(
        &server,
        r#"
        let n = 0;
        const s = new ReadableStream({
            pull(c) {
                if (n++ === 0) { c.enqueue(new Uint8Array(500).fill(98)); return; }
                return new Promise(() => {});
            },
            cancel(reason) { env.probe.mark("source cancelled: " + (reason && reason.name)); }
        });
        await fetch(BASE + "/upload-hang", { method: "POST", body: s, duplex: "half" });
        return { settled: true };
        "#,
        None,
        cancel.clone(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the handler must be waiting on its upload");
    };
    compio::time::timeout(SETTLE, first_chunk)
        .await
        .expect("the server never held the first chunk")
        .expect("the server stopped");

    // What the worker does when the wall timeout fires.
    cancel.cancel();
    runtime.notify_pump();

    let reply = compio::time::timeout(SETTLE, rx.recv()).await.expect("no reply after the cancel");
    let error = match reply {
        Err(error) => error,
        Ok(_) => panic!("a cancelled request must not settle with its handler's response"),
    };
    assert_eq!(error.message, "Request timed out", "{error:?}");

    compio::time::timeout(SETTLE, upload_ended)
        .await
        .expect("the upload kept its connection open after its request ended")
        .expect("the server stopped");
    let upload = server.observe(|o| o.uploads.last().cloned()).expect("the upload was recorded");
    assert!(!upload.terminated, "a cut upload never ends its body: {}", upload.json());
    assert_eq!(upload.bytes, 500, "no bytes reached the server after the cancel: {}", upload.json());
    assert_eq!(marks(), vec!["source cancelled: AbortError".to_string()]);
}

#[compio::test]
async fn an_early_complete_answer_on_a_kept_alive_connection_ends_the_upload() {
    // The server answers before reading the body and keeps the connection
    // open, discarding what it is sent. The HTTP client would go on sending
    // into it; once the answer is complete the upload stops instead.
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        let resolveCancel;
        const cancelled = new Promise(r => { resolveCancel = r; });
        const s = new ReadableStream({
            pull(c) { c.enqueue(new Uint8Array(64 * 1024).fill(101)); },
            cancel(r) { resolveCancel(r); }
        });
        const resp = await fetch(BASE + "/early-keepalive", { method: "POST", body: s, duplex: "half" });
        const text = await resp.text();
        const reason = await cancelled;
        const early = await (await fetch(BASE + "/early-outcome")).json();
        return { status: resp.status, text, cancelled: reason !== undefined, early };
        "#,
    )
    .await;
    assert_eq!(v["status"], 413, "{v}");
    assert_eq!(v["text"], "too large", "{v}");
    assert_eq!(v["cancelled"], true, "the source is cancelled: {v}");
    assert_eq!(v["early"]["closed"], true, "the client ended the connection: {v}");
}

/// JS that starts `K` uploads through `env.probe.startUpload`, each from a
/// source of `PER_UPLOAD` bytes in 64 KiB chunks, and counts what each source
/// produced and whether it was cancelled. The readers are held outside the
/// runtime and never read, so nothing drains the uploads: what they hold is
/// bounded by the platform alone. Each upload wants more than its channel's
/// pause mark, and together they want more than the runtime's upload share;
/// they start together and read in turn, so the share runs out while every
/// channel is still below its pause mark.
const SHARE_SATURATING_UPLOADS: &str = r#"
        const CHUNK = 64 * 1024, K = 32, PER_UPLOAD = 3 * 1024 * 1024;
        const produced = new Array(K).fill(0);
        const cancelled = new Array(K).fill(false);
        for (let k = 0; k < K; k++) {
            const s = new ReadableStream({
                pull(c) {
                    if (produced[k] >= PER_UPLOAD) { c.close(); return; }
                    c.enqueue(new Uint8Array(CHUNK));
                    produced[k] += CHUNK;
                },
                cancel() { cancelled[k] = true; }
            });
            const refusal = env.probe.startUpload(s, new AbortController().signal);
            if (refusal !== undefined) throw new Error(refusal);
        }
        await (await fetch(BASE + "/ping")).text();
        await (await fetch(BASE + "/ping")).text();
        const atHold = produced.slice();
        const heldAtHold = env.probe.uploadBytes();
"#;

#[compio::test]
async fn one_runtimes_uploads_stay_within_its_share_and_leave_another_runtime_unaffected() {
    // Uploads nothing reads. Each would fill its own channel to the pause
    // mark, which together is more than the runtime's upload share; they
    // stop at the share instead, so they cannot take the process-wide stream
    // budget from another runtime on the thread, which uploads normally
    // meanwhile.
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let waiting = server.when(|o| o.waiting_release);
    let (_saturated, outcome) = launch(
        &server,
        &format!(
            r#"{SHARE_SATURATING_UPLOADS}
        await (await fetch(BASE + "/wait-released")).text();
        return {{ heldAtHold, atHold, chunk: CHUNK, uploads: K, perUpload: PER_UPLOAD }};
        "#
        ),
        None,
        CancelFlag::new(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the saturating handler must be waiting");
    };
    compio::time::timeout(SETTLE, waiting)
        .await
        .expect("the saturating handler never reached its hold")
        .expect("the server stopped");

    let other = start_server();
    let neighbour = run_app(
        &other,
        r#"
        const SIZE = 8 * 1024 * 1024;
        const bytes = new Uint8Array(SIZE);
        for (let o = 0; o < SIZE; o++) bytes[o] = positionalByte(o);
        const resp = await fetch(BASE + "/upload?pattern=pos", { method: "POST", body: new Blob([bytes]).stream(), duplex: "half" });
        return { size: SIZE, upload: await resp.json(), held: env.probe.uploadBytes() };
        "#,
    )
    .await;

    server.shared.update(|o| o.released = true);
    let saturated = settle(FetchOutcome::Pending { rx, cancel: CancelFlag::new() }).await;
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());

    let held_bytes = saturated["heldAtHold"].as_u64().unwrap();
    let chunk = saturated["chunk"].as_u64().unwrap();
    let uploads = saturated["uploads"].as_u64().unwrap();
    let per_upload = saturated["perUpload"].as_u64().unwrap();
    let share = MAX_UPLOAD_BUFFER_BYTES as u64;
    let produced: Vec<u64> = saturated["atHold"].as_array().unwrap().iter().map(|p| p.as_u64().unwrap()).collect();
    assert_eq!(produced.len() as u64, uploads, "{saturated}");
    assert!(held_bytes >= share, "the uploads filled the share: {saturated}");
    assert!(
        held_bytes <= share + uploads * chunk,
        "one runtime's uploads stop at its share (one parked chunk each at most): {saturated}"
    );
    assert!(produced.iter().all(|&p| p < per_upload), "every source waits before its end: {saturated}");
    assert_eq!(neighbour["upload"]["bytes"], neighbour["size"], "{neighbour}");
    assert_eq!(neighbour["upload"]["pattern_ok"], true, "{neighbour}");
    assert_eq!(neighbour["held"], 0, "the neighbour's share is its own: {neighbour}");
}

#[compio::test]
async fn an_upload_waiting_for_share_room_goes_on_once_another_upload_frees_it() {
    // The share is full and every upload waits with one chunk parked; their
    // own channels are not read. Ending half of the uploads frees share
    // room, and the others go on reading their sources without anything
    // reading their channels.
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let waiting = server.when(|o| o.waiting_release);
    let (_runtime, outcome) = launch(
        &server,
        &format!(
            r#"{SHARE_SATURATING_UPLOADS}
        await (await fetch(BASE + "/wait-released")).text();
        await (await fetch(BASE + "/ping")).text();
        await (await fetch(BASE + "/ping")).text();
        return {{
            heldAtHold, atHold, after: produced.slice(), cancelled,
            heldAfter: env.probe.uploadBytes(), chunk: CHUNK, uploads: K, perUpload: PER_UPLOAD,
        }};
        "#
        ),
        None,
        CancelFlag::new(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the handler must be waiting");
    };
    compio::time::timeout(SETTLE, waiting)
        .await
        .expect("the handler never reached its hold")
        .expect("the server stopped");
    let ended = UPLOADS.with(|uploads| {
        let mut uploads = uploads.borrow_mut();
        let half = uploads.len() / 2;
        uploads.drain(..half).count()
    });
    server.shared.update(|o| o.released = true);
    let v = settle(FetchOutcome::Pending { rx, cancel: CancelFlag::new() }).await;
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());

    let numbers = |key: &str| -> Vec<u64> { v[key].as_array().unwrap().iter().map(|p| p.as_u64().unwrap()).collect() };
    let (at_hold, after) = (numbers("atHold"), numbers("after"));
    let cancelled: Vec<bool> = v["cancelled"].as_array().unwrap().iter().map(|c| c.as_bool().unwrap()).collect();
    let uploads = v["uploads"].as_u64().unwrap();
    let chunk = v["chunk"].as_u64().unwrap();
    let per_upload = v["perUpload"].as_u64().unwrap();
    let share = MAX_UPLOAD_BUFFER_BYTES as u64;
    assert_eq!(ended as u64, uploads / 2, "{v}");
    assert_eq!(at_hold.len() as u64, uploads, "{v}");
    assert!(v["heldAtHold"].as_u64().unwrap() >= share, "the uploads filled the share: {v}");
    assert!(at_hold.iter().all(|&p| p < per_upload), "every source waited before its end: {v}");
    assert!(cancelled[..ended].iter().all(|&c| c), "an ended upload cancels its source: {v}");
    assert!(cancelled[ended..].iter().all(|&c| !c), "{v}");
    for k in ended..at_hold.len() {
        assert!(after[k] > at_hold[k], "upload {k} went on reading once share room was freed: {v}");
    }
    assert!(v["heldAfter"].as_u64().unwrap() <= share + uploads * chunk, "{v}");
}

#[compio::test]
async fn one_chunk_several_times_the_buffer_is_held_once_and_sent_in_order() {
    // A chunk five times the channel's cap is sliced; the platform holds the
    // channel and the parked rest of that one chunk, and reads nothing more
    // from the source until the rest is sent.
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const CAP = 4 * 1024 * 1024, BIG = 5 * CAP + 1234, TAIL = 1000;
        let pulls = 0;
        const s = new ReadableStream({
            pull(c) {
                pulls++;
                if (pulls === 1) {
                    const b = new Uint8Array(BIG);
                    for (let o = 0; o < BIG; o++) b[o] = positionalByte(o);
                    c.enqueue(b);
                    return;
                }
                if (pulls === 2) {
                    const b = new Uint8Array(TAIL);
                    for (let i = 0; i < TAIL; i++) b[i] = positionalByte(BIG + i);
                    c.enqueue(b);
                    return;
                }
                c.close();
            }
        });
        const upload = fetch(BASE + "/upload-gated?pattern=pos", { method: "POST", body: s, duplex: "half" });
        await (await fetch(BASE + "/ping")).text();
        await (await fetch(BASE + "/ping")).text();
        const heldBytes = env.probe.uploadBytes();
        const pullsWhileHeld = pulls;
        await (await fetch(BASE + "/release")).text();
        const resp = await upload;
        return { cap: CAP, big: BIG, total: BIG + TAIL, heldBytes, pullsWhileHeld, upload: await resp.json() };
        "#,
    )
    .await;
    let cap = v["cap"].as_u64().unwrap();
    let big = v["big"].as_u64().unwrap();
    let held_bytes = v["heldBytes"].as_u64().unwrap();
    assert!(
        v["pullsWhileHeld"].as_u64().unwrap() <= 2,
        "nothing past the parked chunk is read (the stream refills its own queue once): {v}"
    );
    assert!(held_bytes <= big, "the platform holds no more than the one chunk: {v}");
    assert!(held_bytes > cap, "the parked rest of the chunk counts as held: {v}");
    assert_eq!(v["upload"]["bytes"], v["total"], "{v}");
    assert_eq!(v["upload"]["pattern_ok"], true, "the slices arrive in order: {v}");
}

// ---------------------------------------------------------------------------
// The upload share drains when its uploads end
// ---------------------------------------------------------------------------

/// How many uploads the share-saturating tests start together. Their channels
/// pause at half the channel cap, so together they want more than the
/// runtime's upload share.
const SHARE_UPLOADS: usize = 20;
/// The chunks one share-saturating source enqueues. The channel pauses at half
/// the channel cap, so the source must want more than that to stop short of
/// its end at the hold; the slack above the pause mark absorbs the one chunk
/// the stream buffers ahead of the read loop.
const SHARE_CHUNK: usize = 64 * 1024;
const SHARE_PER_UPLOAD: usize = 3 * 1024 * 1024;

/// JS opening that starts `SHARE_UPLOADS` uploads through
/// `env.probe.startUpload`, each from a source that wants `SHARE_PER_UPLOAD`
/// bytes in `SHARE_CHUNK` chunks. Together the sources want more than the
/// runtime's upload share and they start together, so the share runs out
/// while every source is still short of its end. `signal` is the JS
/// expression each upload follows; `end` is the statement run once a source
/// has produced `SHARE_PER_UPLOAD`; `on_cancel` records that a source was
/// cancelled. The block ends with `heldAtHold`, read through
/// `env.probe.uploadBytes()` after two pump passes.
fn saturating_uploads(signal: &str, end: &str, on_cancel: &str) -> String {
    format!(
        r#"
        const CHUNK = {SHARE_CHUNK}, K = {SHARE_UPLOADS}, PER_UPLOAD = {SHARE_PER_UPLOAD};
        const produced = new Array(K).fill(0);
        const cancelled = new Array(K).fill(false);
        for (let k = 0; k < K; k++) {{
            const s = new ReadableStream({{
                pull(c) {{
                    if (produced[k] >= PER_UPLOAD) {{ {end} return; }}
                    c.enqueue(new Uint8Array(CHUNK));
                    produced[k] += CHUNK;
                }},
                cancel(r) {{ {on_cancel} }}
            }});
            const refusal = env.probe.startUpload(s, {signal});
            if (refusal !== undefined) throw new Error(refusal);
        }}
        await (await fetch(BASE + "/ping")).text();
        await (await fetch(BASE + "/ping")).text();
        const atHold = produced.slice();
        const heldAtHold = env.probe.uploadBytes();
        "#
    )
}

/// Read one held upload to its end the way a body consumer reads it: a source
/// failure ends the upload. Returns the bytes it yielded and whether it
/// failed.
async fn drain_held_upload(mut reader: UploadReader) -> (usize, bool) {
    let mut bytes = 0usize;
    let mut failed = false;
    while let Some(item) = reader.next_chunk().await {
        if let Ok(chunk) = item {
            bytes += chunk.len();
        } else {
            failed = true;
            break;
        }
    }
    (bytes, failed)
}

/// The bytes this runtime's uploads hold, read from its upload share: the
/// value `env.probe.uploadBytes()` reports.
fn held_upload_bytes(runtime: &Runtime) -> usize {
    runtime.state().borrow().upload_share.used()
}

/// Assert the saturating fixture's control: every source was still short of
/// its end while the share was held. Without it a fixture that let every
/// source reach its end before the hold would still read as saturated.
fn assert_sources_short_of_their_end(v: &serde_json::Value) {
    let produced: Vec<u64> = v["atHold"].as_array().unwrap().iter().map(|p| p.as_u64().unwrap()).collect();
    assert_eq!(
        produced.len() as u64,
        v["uploads"].as_u64().unwrap(),
        "every source reported its production: {v}"
    );
    assert!(
        produced.iter().all(|&p| p < v["perUpload"].as_u64().unwrap()),
        "every source is still short of its end while the share is held: {v}"
    );
}

/// Drain every held upload and collect what each yielded.
async fn drain_all_held_uploads() -> Vec<(usize, bool)> {
    let readers: Vec<UploadReader> = UPLOADS.with(|uploads| uploads.borrow_mut().drain(..).collect());
    assert!(!readers.is_empty(), "the uploads are held outside the runtime");
    let mut drains = futures::stream::FuturesUnordered::new();
    for reader in readers {
        drains.push(drain_held_upload(reader));
    }
    let mut results = Vec::new();
    while let Some(result) = drains.next().await {
        results.push(result);
    }
    results
}

#[compio::test]
async fn fetch_streaming_upload_share_returns_to_zero_when_uploads_complete() {
    // The share is filled with uploads nothing reads, then every upload is
    // read to its source's end. The channel bytes and the parked bytes must
    // all be released, not held until the runtime is dropped.
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let waiting = server.when(|o| o.waiting_release);
    let (runtime, outcome) = launch(
        &server,
        &format!(
            r#"{}
        await (await fetch(BASE + "/wait-released")).text();
        return {{ heldAtHold, atHold, heldAfterDrain: env.probe.uploadBytes(), uploads: K, perUpload: PER_UPLOAD }};
        "#,
            saturating_uploads("new AbortController().signal", "c.close();", "cancelled[k] = true;")
        ),
        None,
        CancelFlag::new(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the saturating handler must be waiting");
    };
    compio::time::timeout(SETTLE, waiting)
        .await
        .expect("the saturating handler never reached its hold")
        .expect("the server stopped");

    let share = MAX_UPLOAD_BUFFER_BYTES;
    let held_at_hold = held_upload_bytes(&runtime);
    assert!(held_at_hold >= share, "the uploads filled the share before draining: {held_at_hold} of {share}");

    let drained = drain_all_held_uploads().await;
    assert_eq!(drained.len(), SHARE_UPLOADS, "every upload is held outside the runtime");
    let bytes: usize = drained.iter().map(|(bytes, _)| *bytes).sum();
    let failures = drained.iter().filter(|(_, failed)| *failed).count();
    assert_eq!(failures, 0, "a completed source does not fail");
    assert_eq!(
        bytes,
        drained.len() * SHARE_PER_UPLOAD,
        "every source delivers the bytes it produced"
    );
    assert_eq!(held_upload_bytes(&runtime), 0, "the share drains to zero when the uploads end");

    server.shared.update(|o| o.released = true);
    let v = settle(FetchOutcome::Pending { rx, cancel: CancelFlag::new() }).await;
    assert!(v["heldAtHold"].as_u64().unwrap() >= share as u64, "the handler saw the share filled: {v}");
    assert_sources_short_of_their_end(&v);
    assert_eq!(v["heldAfterDrain"].as_u64().unwrap(), 0, "uploadBytes() is zero after the uploads end: {v}");
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
}

#[compio::test]
async fn fetch_streaming_upload_share_returns_to_zero_when_uploads_error() {
    // The same, where every source throws at its end. A failed upload must
    // release its channel and parked bytes just as a completed one does.
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let waiting = server.when(|o| o.waiting_release);
    let (runtime, outcome) = launch(
        &server,
        &format!(
            r#"{}
        await (await fetch(BASE + "/wait-released")).text();
        return {{ heldAtHold, atHold, heldAfterDrain: env.probe.uploadBytes(), uploads: K, perUpload: PER_UPLOAD }};
        "#,
            saturating_uploads(
                "new AbortController().signal",
                "c.error(new Error(\"source failed\"));",
                "cancelled[k] = true;",
            )
        ),
        None,
        CancelFlag::new(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the saturating handler must be waiting");
    };
    compio::time::timeout(SETTLE, waiting)
        .await
        .expect("the saturating handler never reached its hold")
        .expect("the server stopped");

    let share = MAX_UPLOAD_BUFFER_BYTES;
    let held_at_hold = held_upload_bytes(&runtime);
    assert!(held_at_hold >= share, "the uploads filled the share before failing: {held_at_hold} of {share}");

    let drained = drain_all_held_uploads().await;
    assert_eq!(drained.len(), SHARE_UPLOADS, "every upload is held outside the runtime");
    let bytes: usize = drained.iter().map(|(bytes, _)| *bytes).sum();
    let failures = drained.iter().filter(|(_, failed)| *failed).count();
    assert_eq!(failures, drained.len(), "every failing source ends its upload as a failure");
    assert_eq!(
        bytes,
        drained.len() * SHARE_PER_UPLOAD,
        "every source delivers the bytes it produced before failing"
    );
    assert_eq!(held_upload_bytes(&runtime), 0, "the share drains to zero when the uploads fail");

    server.shared.update(|o| o.released = true);
    let v = settle(FetchOutcome::Pending { rx, cancel: CancelFlag::new() }).await;
    assert!(v["heldAtHold"].as_u64().unwrap() >= share as u64, "the handler saw the share filled: {v}");
    assert_sources_short_of_their_end(&v);
    assert_eq!(v["heldAfterDrain"].as_u64().unwrap(), 0, "uploadBytes() is zero after the uploads fail: {v}");
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
}

#[compio::test]
async fn fetch_streaming_upload_share_returns_to_zero_when_an_aborted_signal_cancels_uploads() {
    // The same, where every upload follows one signal and the signal aborts.
    // Cancelling releases the parked bytes at once; the channel bytes go when
    // the reader that holds them is dropped.
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let waiting = server.when(|o| o.waiting_release);
    let (runtime, outcome) = launch(
        &server,
        &format!(
            r#"
        const reason = new Error("stop the uploads");
        const ac = new AbortController();
        {}
        ac.abort(reason);
        await (await fetch(BASE + "/ping")).text();
        await (await fetch(BASE + "/ping")).text();
        const heldAfterCancel = env.probe.uploadBytes();
        await (await fetch(BASE + "/wait-released")).text();
        return {{
            heldAtHold, atHold, heldAfterCancel, heldAfterDrain: env.probe.uploadBytes(),
            cancelled, uploads: K, perUpload: PER_UPLOAD,
        }};
        "#,
            saturating_uploads(
                "ac.signal",
                "return new Promise(() => {});",
                "cancelled[k] = (r === reason);",
            )
        ),
        None,
        CancelFlag::new(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the saturating handler must be waiting");
    };
    compio::time::timeout(SETTLE, waiting)
        .await
        .expect("the saturating handler never reached its hold")
        .expect("the server stopped");

    let held_after_cancel = held_upload_bytes(&runtime);
    assert!(held_after_cancel > 0, "the readers still hold the channel bytes");

    let _ = drain_all_held_uploads().await;
    server.shared.update(|o| o.released = true);
    let v = settle(FetchOutcome::Pending { rx, cancel: CancelFlag::new() }).await;

    let cancelled: Vec<bool> = v["cancelled"].as_array().unwrap().iter().map(|c| c.as_bool().unwrap()).collect();
    assert!(cancelled.iter().all(|&c| c), "every source is cancelled with the signal's reason: {v}");
    assert!(v["heldAtHold"].as_u64().unwrap() >= MAX_UPLOAD_BUFFER_BYTES as u64, "the uploads filled the share: {v}");
    assert_sources_short_of_their_end(&v);
    assert_eq!(
        v["heldAfterCancel"].as_u64().unwrap(),
        held_after_cancel as u64,
        "the abort released the parked bytes while the readers held the channels: {v}"
    );
    assert!(
        v["heldAfterCancel"].as_u64().unwrap() < v["heldAtHold"].as_u64().unwrap(),
        "the abort released bytes the share held: {v}"
    );
    assert_eq!(v["heldAfterDrain"].as_u64().unwrap(), 0, "uploadBytes() is zero after the channels are released: {v}");
    assert_eq!(held_upload_bytes(&runtime), 0, "the share drains to zero");
}

#[compio::test]
async fn fetch_streaming_upload_share_returns_to_zero_when_the_owning_request_times_out() {
    // An upload belongs to the request that started it. The worker's wall
    // timeout cancels the request; the request's uploads must release their
    // channel and parked bytes with it.
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let waiting = server.when(|o| o.waiting_release);
    let cancel = CancelFlag::new();
    let (runtime, outcome) = launch(
        &server,
        &format!(
            r#"{}
        await (await fetch(BASE + "/wait-released")).text();
        return {{ heldAtHold, uploads: K, perUpload: PER_UPLOAD }};
        "#,
            saturating_uploads("new AbortController().signal", "c.close();", "cancelled[k] = true;")
        ),
        None,
        cancel.clone(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the saturating handler must be waiting");
    };
    compio::time::timeout(SETTLE, waiting)
        .await
        .expect("the saturating handler never reached its hold")
        .expect("the server stopped");

    let share = MAX_UPLOAD_BUFFER_BYTES;
    let held_at_hold = held_upload_bytes(&runtime);
    assert!(held_at_hold >= share, "the uploads filled the share before the timeout: {held_at_hold} of {share}");

    // What the worker does when the wall timeout fires.
    cancel.cancel();
    runtime.notify_pump();

    let reply = compio::time::timeout(SETTLE, rx.recv()).await.expect("no reply after the cancel");
    let Err(error) = reply else {
        panic!("a cancelled request must not settle with its handler's response");
    };
    assert_eq!(error.message, "Request timed out", "{error:?}");
    assert!(
        runtime.state().borrow().stream_forwarders.is_empty(),
        "the timeout cancelled the request's uploads"
    );

    let _ = drain_all_held_uploads().await;
    assert_eq!(held_upload_bytes(&runtime), 0, "the share drains to zero when the request ends");
}

#[compio::test]
async fn an_upload_owned_by_a_timed_out_request_releases_its_share() {
    // A real `fetch` stream body the gated server never reads: the platform
    // holds the upload's channel and the parked rest of its first chunk, and
    // the production `fetch` wiring gives the upload its owning request. The
    // request is cancelled the way the worker's wall timeout cancels it, and
    // the share must return to zero.
    let server = start_server();
    let waiting = server.when(|o| o.waiting_release);
    let cancel = CancelFlag::new();
    let (runtime, outcome) = launch(
        &server,
        r#"
        const BIG = 40 * 1024 * 1024;
        let pulls = 0;
        const s = new ReadableStream({
            pull(c) { if (pulls++ === 0) c.enqueue(new Uint8Array(BIG)); }
        });
        const upload = fetch(BASE + "/upload-gated", { method: "POST", body: s, duplex: "half" });
        let held = 0;
        for (let i = 0; i < 1000 && held === 0; i++) {
            await (await fetch(BASE + "/ping")).text();
            held = env.probe.uploadBytes();
        }
        if (held === 0) throw new Error("the upload never charged the share");
        await (await fetch(BASE + "/wait-released")).text();
        return { held };
        "#,
        None,
        cancel.clone(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the handler must be waiting on its gated upload");
    };
    compio::time::timeout(SETTLE, waiting)
        .await
        .expect("the handler never reached its hold")
        .expect("the server stopped");
    let held = held_upload_bytes(&runtime);
    assert!(held > 0, "the timed-out request's upload charged the share: held {held}");

    cancel.cancel();
    runtime.notify_pump();
    let reply = compio::time::timeout(SETTLE, rx.recv()).await.expect("no reply after the cancel");
    let Err(error) = reply else {
        panic!("a cancelled request must not settle with its handler's response");
    };
    assert_eq!(error.message, "Request timed out", "{error:?}");
    // The timeout cancels the request's upload: the forwarder is removed, and
    // the HTTP client drops the reader it held, releasing channel and parked
    // bytes alike.
    assert!(
        runtime.state().borrow().stream_forwarders.is_empty(),
        "the timed-out request cancelled its upload"
    );
    assert_eq!(
        held_upload_bytes(&runtime),
        0,
        "the timed-out request's upload released its share"
    );
}

#[compio::test]
async fn fetch_streaming_upload_share_releases_a_parked_chunk_when_the_upload_is_cancelled() {
    // One chunk larger than the channel is sliced: the channel's part and the
    // parked rest are both charged. Cancelling the upload releases the parked
    // part at once; the channel's part goes when the reader is dropped.
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let (runtime, outcome) = launch(
        &server,
        r#"
        const CAP = 4 * 1024 * 1024, BIG = 5 * CAP + 1234;
        const reason = new Error("stop the parked upload");
        const ac = new AbortController();
        let cancelled = false;
        const s = new ReadableStream({
            pull(c) { c.enqueue(new Uint8Array(BIG)); return new Promise(() => {}); },
            cancel(r) { cancelled = (r === reason); }
        });
        const refusal = env.probe.startUpload(s, ac.signal);
        if (refusal !== undefined) throw new Error(refusal);
        await (await fetch(BASE + "/ping")).text();
        await (await fetch(BASE + "/ping")).text();
        const heldAtHold = env.probe.uploadBytes();
        ac.abort(reason);
        await (await fetch(BASE + "/ping")).text();
        await (await fetch(BASE + "/ping")).text();
        return { heldAtHold, heldAfterCancel: env.probe.uploadBytes(), cap: CAP, big: BIG, cancelled };
        "#,
        None,
        CancelFlag::new(),
    );
    let v = settle(outcome).await;
    let cap = zeroship_runtime::channel::DEFAULT_STREAM_BUFFER_CAP as u64;
    assert_eq!(v["cap"].as_u64().unwrap(), cap, "the probe starts uploads at the channel cap: {v}");
    assert!(v["big"].as_u64().unwrap() > cap, "the chunk is larger than its channel: {v}");
    assert_eq!(
        v["heldAtHold"].as_u64().unwrap(),
        v["big"].as_u64().unwrap(),
        "the whole sliced chunk counts against the share: {v}"
    );
    assert_eq!(
        v["heldAfterCancel"].as_u64().unwrap(),
        cap,
        "cancelling releases the parked part and leaves the channel's part: {v}"
    );
    assert_eq!(v["cancelled"], true, "the source is cancelled with the signal's reason: {v}");
    assert_eq!(held_upload_bytes(&runtime) as u64, cap, "the parked bytes are gone, the channel's remain");
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    assert_eq!(held_upload_bytes(&runtime), 0, "dropping the reader releases the channel's part: {v}");
}

// ---------------------------------------------------------------------------
// The upload reads its own stream, and only for a URL that can be sent
// ---------------------------------------------------------------------------

#[compio::test]
async fn an_upload_reads_its_stream_without_the_public_reader_methods() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const readerProto = Object.getPrototypeOf(new ReadableStream().getReader());
        const original = {
            getReader: ReadableStream.prototype.getReader,
            read: readerProto.read,
            cancel: readerProto.cancel,
        };
        let substituted = 0;
        ReadableStream.prototype.getReader = function (...a) { substituted++; return original.getReader.apply(this, a); };
        readerProto.read = function (...a) { substituted++; return original.read.apply(this, a); };
        readerProto.cancel = function (...a) { substituted++; return original.cancel.apply(this, a); };
        let upload;
        try {
            const s = new ReadableStream({ start(c) { c.enqueue(new TextEncoder().encode("own reader")); c.close(); } });
            upload = await (await fetch(BASE + "/upload", { method: "POST", body: s, duplex: "half" })).json();
        } finally {
            ReadableStream.prototype.getReader = original.getReader;
            readerProto.read = original.read;
            readerProto.cancel = original.cancel;
        }
        return { substituted, upload };
        "#,
    )
    .await;
    assert_eq!(v["upload"]["tail"], "own reader", "{v}");
    assert_eq!(v["substituted"], 0, "no creator-replaceable reader method ran: {v}");
}

#[compio::test]
async fn a_refused_url_never_reads_the_stream_body() {
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        let pulls = 0;
        // A high-water mark of 0: nothing pulls this stream but a reader.
        const s = new ReadableStream({ pull(c) { pulls++; c.enqueue(new Uint8Array(10)); } }, { highWaterMark: 0 });
        const before = pulls;
        let outcome;
        try {
            await fetch("http://127.0.0.1:25/", { method: "POST", body: s, duplex: "half" });
            outcome = "resolved";
        } catch (e) {
            outcome = e.name;
        }
        // Control: the same stream shape to a URL that can be sent is read.
        const t = new ReadableStream({ start(c) { c.enqueue(new Uint8Array(10)); c.close(); } });
        const sent = await (await fetch(BASE + "/upload", { method: "POST", body: t, duplex: "half" })).json();
        return { outcome, pullsAfter: pulls - before, locked: s.locked, sent: sent.bytes };
        "#,
    )
    .await;
    assert_eq!(v["outcome"], "TypeError", "{v}");
    assert_eq!(v["pullsAfter"], 0, "the refused fetch read nothing: {v}");
    assert_eq!(v["locked"], false, "the refused fetch never locked the stream: {v}");
    assert_eq!(v["sent"], 10, "{v}");
}

#[compio::test]
async fn abort_through_a_proxied_request_body_cancels_the_source_with_the_reason() {
    let server = start_server();
    let v = run_app(
        &server,
        &format!(
            r#"
        const reason = new Error("stop the proxied upload");
        const ac = new AbortController();
        let resolveCancel;
        const cancelled = new Promise(r => {{ resolveCancel = r; }});
        let n = 0;
        const s = new ReadableStream({{
            pull(c) {{
                if (n++ === 0) {{ c.enqueue(new Uint8Array(500).fill(98)); return; }}
                return new Promise(() => {{}});
            }},
            cancel(r) {{ resolveCancel(r); }}
        }});
        const req = new Request(BASE + "/upload-hang", {{ method: "POST", body: s, duplex: "half", signal: ac.signal }});
        const upload = fetch(req);
        const w = await (await fetch(BASE + "/wait?bytes=500&ms={hold}")).json();
        ac.abort(reason);
        let outcome;
        try {{ await upload; outcome = "resolved"; }}
        catch (e) {{ outcome = e === reason ? "abort-reason" : "other: " + String(e); }}
        const cancelReason = await cancelled;
        return {{ reached: w.reached, outcome, cancelIsReason: cancelReason === reason }};
        "#,
            hold = HOLD_MS
        ),
    )
    .await;
    assert_eq!(v["reached"], true, "{v}");
    assert_eq!(v["outcome"], "abort-reason", "{v}");
    assert_eq!(v["cancelIsReason"], true, "the abort reaches the source through the proxy: {v}");
}

#[compio::test]
async fn an_upload_started_with_an_aborted_signal_reads_nothing_and_cancels_with_its_reason() {
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        const reason = new Error("aborted before the upload");
        const ac = new AbortController();
        ac.abort(reason);
        let pulls = 0;
        let resolveCancel;
        const cancelled = new Promise(r => { resolveCancel = r; });
        const s = new ReadableStream(
            { pull(c) { pulls++; c.enqueue(new Uint8Array(8)); }, cancel(r) { resolveCancel(r); } },
            { highWaterMark: 0 },
        );
        const refusal = env.probe.startUpload(s, ac.signal);
        const cancelReason = await cancelled;
        return { refusal: refusal ?? null, pulls, cancelIsReason: cancelReason === reason };
        "#,
    )
    .await;
    assert_eq!(v["refusal"], serde_json::Value::Null, "{v}");
    assert_eq!(v["pulls"], 0, "an aborted upload reads nothing: {v}");
    assert_eq!(v["cancelIsReason"], true, "{v}");
}

#[compio::test]
async fn a_stream_body_never_carries_two_framing_headers() {
    // Content-Length and Transfer-Encoding together frame one body two ways.
    // A creator's Transfer-Encoding is dropped; the body is framed once.
    let server = start_server();
    let v = run_app(
        &server,
        r#"
        function body(parts) {
            return new ReadableStream({ start(c) { for (const p of parts) c.enqueue(new TextEncoder().encode(p)); c.close(); } });
        }
        async function send(headers) {
            return (await fetch(BASE + "/echo", { method: "PUT", body: body(["abc", "def"]), duplex: "half", headers })).json();
        }
        return {
            both: await send({ "transfer-encoding": "chunked", "content-length": "6" }),
            te: await send({ "transfer-encoding": "chunked" }),
            cl: await send({ "content-length": "6" }),
            none: await send({}),
        };
        "#,
    )
    .await;
    assert_eq!(v["both"]["framing"], serde_json::json!(["content-length: 6"]), "{v}");
    assert_eq!(v["te"]["framing"], serde_json::json!(["transfer-encoding: chunked"]), "{v}");
    assert_eq!(v["cl"]["framing"], serde_json::json!(["content-length: 6"]), "{v}");
    assert_eq!(v["none"]["framing"], serde_json::json!(["transfer-encoding: chunked"]), "{v}");
    for case in ["both", "te", "cl", "none"] {
        assert_eq!(v[case]["body"], "abcdef", "{case}: {v}");
    }
}

#[compio::test]
async fn a_body_still_held_by_the_client_after_its_response_keeps_its_fetch_slot() {
    // The server answers before reading the body and then reads only as the
    // test grants it. The response arrives while the HTTP client still holds
    // the body; until the client lets go of it, the fetch still counts
    // against the runtime's limit on concurrent fetches. The handler reads
    // the count as soon as `fetch` resolves: the fetch's own task has ended
    // by then, and the body's end, which the completed response starts,
    // reaches the client only after that continuation has run.
    MARKS.with(|marks| marks.borrow_mut().clear());
    let server = start_server();
    let (_runtime, outcome) = launch(
        &server,
        r#"
        const s = new ReadableStream({ pull(c) { c.enqueue(new Uint8Array(64 * 1024)); } });
        const resp = await fetch(BASE + "/early-stepped", { method: "POST", body: s, duplex: "half" });
        const inFlightAtAnswer = env.probe.inFlightFetches();
        env.probe.mark("answered");
        const text = await resp.text();
        const outcome = await (await fetch(BASE + "/early-outcome")).json();
        return { status: resp.status, text, inFlightAtAnswer, outcome };
        "#,
        None,
        CancelFlag::new(),
    );
    let FetchOutcome::Pending { rx, .. } = outcome else {
        panic!("the handler must be waiting on its upload");
    };
    let mut granted = 0usize;
    while !marks().iter().any(|m| m == "answered") {
        assert!(granted < 4096, "the response never arrived");
        let read = server.when(move |o| o.consumed > granted);
        server.shared.update(|o| o.steps += 1);
        compio::time::timeout(SETTLE, read).await.expect("the server stopped reading").expect("the server stopped");
        granted += 1;
    }
    server.shared.update(|o| o.released = true);
    let v = settle(FetchOutcome::Pending { rx, cancel: CancelFlag::new() }).await;
    assert_eq!(v["status"], 413, "{v}");
    assert_eq!(v["text"], "too large", "{v}");
    assert_eq!(v["inFlightAtAnswer"], 1, "the held body keeps its fetch's slot: {v}");
    assert_eq!(v["outcome"]["closed"], false, "the cut body never reached its end: {v}");
}

#[compio::test]
async fn an_upload_reader_does_not_keep_its_runtime_alive() {
    // The HTTP client can hold an upload's reader in a task of its own after
    // the runtime that started it is gone; the reader holds its runtime
    // weakly, so an evicted runtime's state is freed.
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
    let server = start_server();
    let (runtime, outcome) = launch(
        &server,
        r#"
        const s = new ReadableStream({ pull(c) { c.enqueue(new Uint8Array(8)); } });
        const refusal = env.probe.startUpload(s, new AbortController().signal);
        return { refusal: refusal ?? null };
        "#,
        None,
        CancelFlag::new(),
    );
    let v = settle(outcome).await;
    assert_eq!(v["refusal"], serde_json::Value::Null, "{v}");
    assert_eq!(UPLOADS.with(|uploads| uploads.borrow().len()), 1, "the reader is held outside the runtime");

    let state = std::rc::Rc::downgrade(&runtime.state());
    runtime.shutdown().await;
    let inner = runtime.into_inner_probe_for_test();
    assert_eq!(inner.strong_count(), 0, "the runtime itself is gone");
    assert!(state.upgrade().is_none(), "a held upload reader kept its runtime's state alive");
    UPLOADS.with(|uploads| uploads.borrow_mut().clear());
}
