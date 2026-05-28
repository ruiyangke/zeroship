use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ntex::http::StatusCode;
use ntex::web::{self, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};
use zeroship_core::hydra::HydraIntrospector;

#[derive(Debug, Clone)]
enum MockMode {
    Fixed { status: u16, body: Value },
    EchoActive,
}

#[derive(Debug)]
struct MockState {
    calls: AtomicUsize,
    mode: MockMode,
}

struct MockHydra {
    base: String,
    state: Arc<MockState>,
    shutdown: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockHydra {
    fn fixed(status: u16, body: Value) -> Self {
        Self::start(MockMode::Fixed { status, body })
    }

    fn echo_active() -> Self {
        Self::start(MockMode::EchoActive)
    }

    fn start(mode: MockMode) -> Self {
        let state = Arc::new(MockState {
            calls: AtomicUsize::new(0),
            mode,
        });
        let factory_state = state.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            ntex::rt::System::build()
                .name("hydra-introspect-mock")
                .testing()
                .build(ntex::rt::DefaultRuntime)
                .block_on(async move {
                    let server = web::test::server(move || {
                        let state = factory_state.clone();
                        async move {
                            web::App::new().state(state).service(
                                web::resource("/admin/oauth2/introspect")
                                    .route(web::post().to(introspect_handler)),
                            )
                        }
                    })
                    .await;
                    let addr = server.addr();
                    started_tx.send(addr).expect("send mock server addr");
                    let _ = shutdown_rx.recv();
                    drop(server);
                });
        });
        let addr = started_rx.recv().expect("mock server starts");
        let base = format!("http://{addr}");
        Self {
            base,
            state,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        }
    }

    fn calls(&self) -> usize {
        self.state.calls.load(Ordering::SeqCst)
    }
}

impl Drop for MockHydra {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, Deserialize)]
struct IntrospectForm {
    token: String,
}

async fn introspect_handler(
    state: web::types::State<Arc<MockState>>,
    form: web::types::Form<IntrospectForm>,
) -> HttpResponse {
    state.calls.fetch_add(1, Ordering::SeqCst);
    match &state.mode {
        MockMode::Fixed { status, body } => {
            let status = StatusCode::from_u16(*status).expect("valid status");
            HttpResponse::build(status).json(body)
        }
        MockMode::EchoActive => HttpResponse::Ok().json(&json!({
            "active": true,
            "sub": form.token,
            "scope": "apps:read",
            "aud": ["https://api.zeroship.ai"],
            "client_id": "echo-client",
            "exp": unix_now_secs() + 3600,
        })),
    }
}

#[compio::test]
async fn active_token_parses_subject_and_scopes() {
    let exp = unix_now_secs() + 3600;
    let hydra = MockHydra::fixed(
        200,
        json!({
            "active": true,
            "sub": "usr_01HABC",
            "scope": "apps:read apps:deploy",
            "aud": ["https://api.zeroship.ai"],
            "client_id": "acme-ci",
            "exp": exp,
        }),
    );

    let client = HydraIntrospector::new(hydra.base.as_str());
    let result = client.introspect("bearer-one").await.expect("introspect");

    assert!(result.active);
    assert_eq!(result.sub.as_deref(), Some("usr_01HABC"));
    assert_eq!(result.scope.as_deref(), Some("apps:read apps:deploy"));
    assert_eq!(
        result.aud.as_deref(),
        Some(&["https://api.zeroship.ai".to_string()][..])
    );
    assert_eq!(result.client_id.as_deref(), Some("acme-ci"));
    assert_eq!(result.exp, Some(exp));
    assert_eq!(hydra.calls(), 1);
}

#[compio::test]
async fn inactive_token_returns_active_false_no_error() {
    let hydra = MockHydra::fixed(200, json!({ "active": false }));

    let client = HydraIntrospector::new(hydra.base.as_str());
    let result = client.introspect("inactive-token").await.expect("introspect");

    assert!(!result.active);
    assert_eq!(result.sub, None);
    assert_eq!(result.scope, None);
    assert_eq!(hydra.calls(), 1);
}

#[compio::test]
async fn cache_hit_does_not_call_hydra() {
    let hydra = MockHydra::fixed(
        200,
        json!({
            "active": true,
            "sub": "usr_cache",
            "scope": "apps:read",
            "exp": unix_now_secs() + 3600,
        }),
    );

    let client = HydraIntrospector::new(hydra.base.as_str());
    let first = client.introspect("same-token").await.expect("first");
    let second = client.introspect("same-token").await.expect("second");

    assert_eq!(first, second);
    assert_eq!(hydra.calls(), 1);
}

#[compio::test]
async fn cache_expires_at_ttl() {
    let hydra = MockHydra::fixed(
        200,
        json!({
            "active": true,
            "sub": "usr_ttl",
            "scope": "apps:read",
            "exp": unix_now_secs() + 3600,
        }),
    );

    let client = HydraIntrospector::new(hydra.base.as_str())
        .with_ttl(Duration::from_millis(10));
    client.introspect("ttl-token").await.expect("first");
    compio::time::sleep(Duration::from_millis(20)).await;
    client.introspect("ttl-token").await.expect("second");

    assert_eq!(hydra.calls(), 2);
}

#[compio::test]
async fn cache_caps_at_lru_size() {
    let hydra = MockHydra::echo_active();
    let client = HydraIntrospector::new(hydra.base.as_str());

    for i in 0..=4096 {
        let token = format!("lru-token-{i}");
        client.introspect(&token).await.expect("fill cache");
    }
    client
        .introspect("lru-token-0")
        .await
        .expect("oldest should be re-fetched after eviction");

    assert_eq!(hydra.calls(), 4098);
}

#[compio::test]
async fn network_error_returns_err_not_cached() {
    let hydra = MockHydra::fixed(500, json!({ "error": "boom" }));

    let client = HydraIntrospector::new(hydra.base.as_str());
    let first = client.introspect("transport-error").await;
    let second = client.introspect("transport-error").await;

    assert!(first.is_err(), "first 500 should return Err");
    assert!(second.is_err(), "second 500 should return Err");
    assert_eq!(hydra.calls(), 2);
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
