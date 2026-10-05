use super::*;
use crate::cache::fixture::Kernel;
use ntex::http::body::{Body, MessageBody, ResponseBody};
use zeroship_runtime::core::channel::{stream_buffer_with_cap, StreamPushResult};

async fn next_chunk(body: &mut ResponseBody<Body>) -> Option<Bytes> {
    compio::time::timeout(
        Duration::from_secs(10),
        std::future::poll_fn(|cx| body.poll_next_chunk(cx)),
    )
    .await
    .expect("stream responds before the deadline")
    .map(|chunk| chunk.expect("successful response chunk"))
}

#[compio::test]
async fn long_stream_accrues_egress_before_close() {
    let meter = Arc::new(zeroship_metering::Meter::new());
    let _kernel = Kernel::install(10, empty_kernel(meter.clone()));
    let app_id = AppId::mint();
    let (writer, reader) = stream_buffer_with_cap(16 * 1024 * 1024);
    let mut body =
        stream_response(200, &[], reader, app_id.clone(), crate::cache::hold(&app_id)).take_body();

    let big = vec![b'x'; (STREAM_FLUSH_BYTES + 4096) as usize];
    assert!(matches!(writer.push(big.clone()), StreamPushResult::Ok));
    assert_eq!(next_chunk(&mut body).await.unwrap().as_ref(), big);

    // Receiving a chunk yields to the drain, whose threshold flush runs
    // before it waits for more input. The writer is still open here.
    let events = meter.drain();
    assert_eq!(
        usage_value(&events, &app_id, "egress_bytes"),
        Some(big.len() as u64)
    );
    assert!(usage_value(&events, &app_id, "stream_wall_us").is_some());
    assert_eq!(usage_value(&events, &app_id, "requests"), None);

    let more = vec![b'y'; 2048];
    assert!(matches!(writer.push(more.clone()), StreamPushResult::Ok));
    writer.close();
    assert_eq!(next_chunk(&mut body).await.unwrap().as_ref(), more);
    assert!(next_chunk(&mut body).await.is_none(), "drain completes");

    let final_events = meter.drain();
    assert_eq!(
        usage_value(&final_events, &app_id, "egress_bytes"),
        Some(more.len() as u64)
    );
    assert_eq!(usage_value(&final_events, &app_id, "requests"), None);
}

#[compio::test]
async fn streaming_counts_request_exactly_once() {
    let meter = Arc::new(zeroship_metering::Meter::new());
    let _kernel = Kernel::install(10, empty_kernel(meter.clone()));
    let app_id = AppId::mint();
    record_stream_unary(&app_id, 123, 456, std::time::Instant::now());
    let (writer, reader) = stream_buffer_with_cap(16 * 1024 * 1024);
    let mut body =
        stream_response(200, &[], reader, app_id.clone(), crate::cache::hold(&app_id)).take_body();

    let mut total_bytes = 0;
    for _ in 0..2 {
        let chunk = vec![b'z'; (STREAM_FLUSH_BYTES + 1) as usize];
        total_bytes += chunk.len() as u64;
        assert!(matches!(writer.push(chunk.clone()), StreamPushResult::Ok));
        assert_eq!(next_chunk(&mut body).await.unwrap().as_ref(), chunk);
    }
    writer.close();
    assert!(next_chunk(&mut body).await.is_none(), "drain completes");

    let events = meter.drain();
    assert_eq!(usage_value(&events, &app_id, "requests"), Some(1));
    assert_eq!(usage_value(&events, &app_id, "cpu_us"), Some(123));
    assert_eq!(usage_value(&events, &app_id, "ingress_bytes"), Some(456));
    assert_eq!(
        usage_value(&events, &app_id, "egress_bytes"),
        Some(total_bytes)
    );
    assert!(usage_value(&events, &app_id, "stream_wall_us").is_some());
}

/// A streamed body holds its app exactly until the stream ends.
///
/// The dispatch returns as soon as the response head is built, but the code
/// producing the body keeps running and may read the app's encrypted data, so
/// the app's credentials must outlive the dispatch by exactly the stream.
#[compio::test]
async fn a_streamed_body_holds_its_app_until_the_stream_ends() {
    let envs = crate::sync::SharedEnvs::default();
    let registry = crate::residency::AppResidency::new(None, None, envs.clone());
    let mut kernel = empty_kernel(Arc::new(zeroship_metering::Meter::new()));
    kernel.residency = Some(registry.clone());
    let _kernel = Kernel::install(10, kernel);
    let app_id = AppId::mint();
    let hold = crate::cache::hold(&app_id);
    crate::sync::put_env_from_json(
        &envs,
        app_id.clone(),
        r#"{"vars":{},"secrets":{},"expose":[]}"#,
        1,
    )
    .expect("the environment parses");

    let (writer, reader) = stream_buffer_with_cap(16 * 1024 * 1024);
    let mut body = stream_response(200, &[], reader, app_id.clone(), hold).take_body();
    assert_eq!(registry.holders(&app_id), 1, "the open stream holds the app");
    let chunk = vec![b's'; 1024];
    assert!(matches!(writer.push(chunk.clone()), StreamPushResult::Ok));
    assert_eq!(next_chunk(&mut body).await.unwrap().as_ref(), chunk);
    assert_eq!(registry.holders(&app_id), 1, "and still holds it mid-stream");
    assert!(crate::sync::get_env(&envs, &app_id).is_some());

    writer.close();
    assert!(next_chunk(&mut body).await.is_none(), "drain completes");
    compio::time::timeout(Duration::from_secs(5), async {
        while registry.holders(&app_id) != 0 {
            compio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the finished stream releases its hold");
    assert!(
        crate::sync::get_env(&envs, &app_id).is_none(),
        "with the stream gone nothing holds the app"
    );
}

/// A streamed body, built by the worker's own `stream_response`, whose
/// producer pushed one chunk and then either failed or closed.
fn ended_body(failed: bool) -> web::HttpResponse {
    let (writer, reader) = stream_buffer_with_cap(1024);
    assert!(matches!(writer.push(b"hello".to_vec()), StreamPushResult::Ok));
    if failed {
        writer.abort("producer failed");
    } else {
        writer.close();
    }
    let app_id = AppId::mint();
    stream_response(200, &[], reader, app_id.clone(), crate::cache::hold(&app_id))
}

/// The bytes a client reads off the wire for that body, from the worker's
/// HTTP server: a real listener, a raw request, everything until the server
/// closes the connection.
async fn wire_bytes(path: &str) -> String {
    let server = test::server(|| async {
        web::App::new()
            .route("/failed", web::get().to(|| async { ended_body(true) }))
            .route("/closed", web::get().to(|| async { ended_body(false) }))
    })
    .await;
    let mut client = compio::net::TcpStream::connect(server.addr())
        .await
        .expect("connect to the worker");
    let request = format!("GET {path} HTTP/1.1\r\nhost: worker\r\nconnection: close\r\n\r\n");
    let compio::buf::BufResult(written, _) =
        compio::io::AsyncWriteExt::write_all(&mut client, request.into_bytes()).await;
    written.expect("send the request");
    compio::time::timeout(Duration::from_secs(10), async {
        let mut received = Vec::new();
        loop {
            let compio::buf::BufResult(read, buf) =
                compio::io::AsyncRead::read(&mut client, vec![0u8; 4096]).await;
            match read {
                Ok(0) | Err(_) => return String::from_utf8_lossy(&received).into_owned(),
                Ok(n) => received.extend_from_slice(&buf[..n]),
            }
        }
    })
    .await
    .expect("the worker closes the connection")
}

/// A body whose producer failed leaves the worker without its terminating
/// chunk: the client reads the chunk that streamed, then bytes that are no
/// chunk at all, then the close - a chunked read that fails, not a body that
/// ended. (ntex writes its own error response head where the next chunk would
/// go; whatever it writes there, no client can read it as the body's end.)
#[ntex::test]
async fn a_failed_stream_leaves_the_worker_without_its_terminating_chunk() {
    let response = wire_bytes("/failed").await;
    let (head, body) = response.split_once("\r\n\r\n").expect("the head went out");
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let rest = body
        .strip_prefix("5\r\nhello\r\n")
        .unwrap_or_else(|| panic!("the chunk that streamed went out first: {body:?}"));
    let next_size = rest.split("\r\n").next().unwrap_or_default();
    assert!(
        rest.is_empty() || usize::from_str_radix(next_size.trim(), 16).is_err(),
        "nothing after the chunk reads as another chunk, terminating or not: {rest:?}"
    );
}

/// The rejection control: the same body closed normally carries its
/// terminating chunk, so the case above is about the failure alone.
#[ntex::test]
async fn a_closed_stream_leaves_the_worker_with_its_terminating_chunk() {
    let response = wire_bytes("/closed").await;
    let (_, body) = response.split_once("\r\n\r\n").expect("the head went out");
    assert_eq!(body, "5\r\nhello\r\n0\r\n\r\n");
}
