//! Regression for the gateway's pooled OP connection outliving the auth
//! server's keep-alive.
//!
//! The auth server retires an idle keep-alive connection after
//! `zeroship_core::op_link::AUTH_KEEP_ALIVE`. The gateway pools one
//! `cyper::Client` per worker thread. A pool that is not retired before that
//! window can hand a request a connection the server has already retired: the
//! request is written, the server closes without answering, and the client
//! reads EOF — an `IncompleteMessage` transport error. On a non-idempotent
//! `POST /oauth2/token` that error also counts against the breaker, and it
//! cannot be retried away (the request was already on the wire).
//!
//! This drives the REAL `OidcRp` refresh path (`refresh_token_public` →
//! `post_token` → `op_client::call`). The client's reuse bound is derived from
//! the server keep-alive through `op_link::op_client_idle_timeout`, so the
//! pooled client is rebuilt before it can reuse a retired connection.
//!
//! The mock OP models the race deterministically: its first connection answers
//! one request, then holds the socket open past the keep-alive and closes
//! WITHOUT answering if a second request arrives on it (the server retired the
//! connection as the stale request raced in). Later connections answer
//! normally. It is the same shape as the gateway's own
//! `pooled_retry_tests::MockBehavior::DrainThenClose`, which exists because a
//! close that races the request makes the failure land on the read with the
//! request already consumed.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use compio::BufResult;
use compio::io::{AsyncRead, AsyncWriteExt};
use compio::net::{TcpListener, TcpStream};

use zeroship_gateway::oidc_rp::{BrokerSecret, OidcRp};

const CLIENT_ID: &str = "oac_myapp";
const TEST_BROKER_MASTER: &[u8] = b"gateway-pool-idle-test-broker-32bytes";

/// The mock OP's keep-alive window. Seconds-granular because the real auth
/// server's ntex keep-alive is too; the client bound and the idle gap are both
/// derived from this one value so the test has a single source of timing.
const SERVER_KEEP_ALIVE: Duration = Duration::from_secs(1);

const TOKEN_BODY: &str = r#"{"access_token":"at_ok","token_type":"Bearer","expires_in":3600,"refresh_token":"rt_next","scope":"openid email profile offline_access"}"#;

/// True once `buf` holds a complete HTTP request (headers plus the body length
/// its `Content-Length` declares).
fn request_complete(buf: &[u8]) -> bool {
    let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..pos]);
    let content_length = head
        .lines()
        .find_map(|line| {
            let (k, v) = line.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    buf.len() >= pos + 4 + content_length
}

/// Read one complete request. `false` means the peer closed first.
async fn read_one_request(stream: &mut TcpStream) -> bool {
    let mut acc: Vec<u8> = Vec::new();
    while !request_complete(&acc) {
        let buf = vec![0u8; 4096];
        let BufResult(r, buf) = AsyncRead::read(stream, buf).await;
        match r {
            Ok(0) | Err(_) => return false,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
        }
    }
    true
}

async fn write_token(stream: &mut TcpStream) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        TOKEN_BODY.len(),
        TOKEN_BODY
    );
    let _ = stream.write_all(response.into_bytes()).await;
}

/// Serve the first accepted connection: answer one request, then hold the
/// socket open past the keep-alive and close on any racing second request
/// without answering it (the retired pooled connection).
async fn serve_retired_connection(mut stream: TcpStream, calls: Arc<AtomicU32>) {
    if !read_one_request(&mut stream).await {
        return;
    }
    calls.fetch_add(1, Ordering::SeqCst);
    write_token(&mut stream).await;
    compio::time::sleep(SERVER_KEEP_ALIVE).await;
    // The keep-alive has elapsed. If a stale request now arrives, drop without
    // answering: the request is written, so the client cannot safely retry it.
    let _ = read_one_request(&mut stream).await;
}

/// Serve every later connection: answer each request and count it.
async fn serve_live_connection(mut stream: TcpStream, calls: Arc<AtomicU32>) {
    while read_one_request(&mut stream).await {
        calls.fetch_add(1, Ordering::SeqCst);
        write_token(&mut stream).await;
    }
}

/// Start the mock OP. Returns `(base_url, request_count)`.
async fn start_mock_op() -> (String, Arc<AtomicU32>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock op");
    let addr = listener.local_addr().expect("mock op local addr");
    let calls = Arc::new(AtomicU32::new(0));
    let accept_calls = calls.clone();

    compio::runtime::spawn(async move {
        let mut conn_idx = 0usize;
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                break;
            };
            if conn_idx == 0 {
                compio::runtime::spawn(serve_retired_connection(stream, accept_calls.clone())).detach();
            } else {
                compio::runtime::spawn(serve_live_connection(stream, accept_calls.clone())).detach();
            }
            conn_idx += 1;
        }
    })
    .detach();

    (format!("http://{addr}"), calls)
}

#[compio::test]
async fn post_after_the_server_keep_alive_succeeds_on_a_retired_pool() {
    let idle_timeout = zeroship_core::op_link::op_client_idle_timeout(SERVER_KEEP_ALIVE);
    let (base, calls) = start_mock_op().await;
    let rp = OidcRp::new(
        base,
        BrokerSecret::from_bytes(TEST_BROKER_MASTER.to_vec()).expect("broker secret"),
        b"k".repeat(32),
    )
    .with_op_timeout(Duration::from_secs(5))
    .with_op_client_idle_timeout(idle_timeout);

    // First POST pools a connection to the mock's first (retired) connection.
    let first = rp.refresh_token_public(CLIENT_ID, "rt_seed").await;
    assert!(first.is_ok(), "the first POST must succeed: {first:?}");

    // Hold the connection idle past the server's keep-alive. The client's own
    // bound (half the server window) has elapsed too, so it must not reuse the
    // retired connection.
    compio::time::sleep(SERVER_KEEP_ALIVE + Duration::from_millis(300)).await;

    // A second POST on the pooled connection would be written and then answered
    // with EOF. The retired pool forces a fresh connection.
    let second = rp.refresh_token_public(CLIENT_ID, "rt_seed").await;
    assert!(
        second.is_ok(),
        "a POST after the server keep-alive retired the pooled connection must not fail: {second:?}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "both POSTs must reach a live connection"
    );
}
