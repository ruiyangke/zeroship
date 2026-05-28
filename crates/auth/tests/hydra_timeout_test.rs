use std::net::TcpListener;
use std::time::{Duration, Instant};

use zeroship_auth::error::AuthError;
use zeroship_auth::hydra_client::HydraAdmin;

#[compio::test]
async fn hydra_admin_get_times_out_when_transport_hangs() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind hang listener");
    let addr = listener.local_addr().expect("listener addr");
    std::thread::spawn(move || {
        if let Ok((_stream, _peer)) = listener.accept() {
            std::thread::sleep(Duration::from_secs(2));
        }
    });

    let admin = HydraAdmin::new(format!("http://{addr}"))
        .with_timeout(Duration::from_millis(200));

    let started = Instant::now();
    let err = admin
        .get_jwks("hydra.openid.id-token")
        .await
        .expect_err("silent hydra admin transport should timeout");
    let elapsed = started.elapsed();

    assert!(
        matches!(err, AuthError::Hydra(ref msg) if msg.contains("timeout")),
        "expected hydra timeout error, got: {err}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "hydra timeout should return promptly; elapsed: {elapsed:?}"
    );
}
