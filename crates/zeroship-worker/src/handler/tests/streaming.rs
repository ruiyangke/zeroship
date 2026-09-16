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
    let mut body = stream_response(200, &[], reader, app_id.clone()).take_body();

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
    let mut body = stream_response(200, &[], reader, app_id.clone()).take_body();

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
