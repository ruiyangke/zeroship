use super::*;
use futures::{pin_mut, select, FutureExt};
use std::{cell::Cell, time::Duration};

#[compio::test]
async fn control_request_times_out_when_the_peer_never_responds() {
    let listener = compio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!(
        "http://{}/internal/versions",
        listener.local_addr().unwrap()
    );
    let accepted = Cell::new(false);
    let server = async {
        let (_connection, _) = listener.accept().await.expect("accept control request");
        accepted.set(true);
        std::future::pending::<()>().await;
    }
    .fuse();
    let request = http_get_bytes(&url, None).fuse();
    pin_mut!(server, request);
    let result = compio::time::timeout(CONTROL_REQUEST_TIMEOUT + Duration::from_secs(5), async {
        select! {
            result = request => result,
            () = server => unreachable!("server holds the connection without responding"),
        }
    })
    .await
    .expect("control request has a bounded deadline");
    assert!(accepted.get(), "timeout must follow an accepted connection");
    assert_eq!(result.unwrap_err(), control_timeout_error());
}
