use crate::worker_fixture::{dispatch_frame, Response, Worker};
use zeroship_bundle::Manifest;
use zeroship_core::types::AppRuntimeLimits;

pub async fn run_pre_dispatch_reject(payload: Vec<u8>) -> Response {
    Worker::new().dispatch(payload).await
}

pub async fn run_metered_dispatch(
    source: &[u8],
    limits: AppRuntimeLimits,
    body: &[u8],
) -> Response {
    let worker = Worker::new();
    worker.load(source, limits, &Manifest::default()).await;
    worker
        .dispatch(dispatch_frame(
            "POST",
            "http://example.test/generated-error",
            body,
        ))
        .await
}
