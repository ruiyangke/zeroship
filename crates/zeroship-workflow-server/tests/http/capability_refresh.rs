use super::*;
use std::{collections::VecDeque, sync::Mutex};
use zeroship_core::service_identity::{endpoints, verify_service_call};
use zeroship_workflow::service::capability::{
    CapabilityToken, IssuedAppCapability, APP_CAPABILITY_MAX_LIFETIME_SECONDS,
};

#[derive(Clone)]
enum Reply {
    Token(CapabilityToken, i64),
    Status(StatusCode),
    Oversized,
}
struct Replies {
    queued: VecDeque<Reply>,
    fallback: Reply,
    requested_apps: Vec<AppId>,
}
struct Issuer {
    verifier: ServiceAssertionVerifier,
    replies: Mutex<Replies>,
}
impl Issuer {
    fn set(&self, queued: impl IntoIterator<Item = Reply>, fallback: Reply) {
        let mut replies = self.replies.lock().unwrap();
        replies.queued = queued.into_iter().collect();
        replies.fallback = fallback;
    }
    fn requests(&self) -> usize {
        self.replies.lock().unwrap().requested_apps.len()
    }
}

async fn issue(
    request: web::HttpRequest,
    state: web::types::State<Arc<Issuer>>,
    app: web::types::Path<String>,
) -> web::HttpResponse {
    verify_service_call(
        &state.verifier,
        request
            .headers()
            .get("authorization")
            .and_then(|header| header.to_str().ok()),
        "spiffe://zeroship.ai/svc/control",
        endpoints::CONTROL_WORKFLOW_CAPABILITY,
    )
    .await
    .expect("every refresh must carry a fresh assertion from the enrolled worker");
    let reply = {
        let mut replies = state.replies.lock().unwrap();
        replies.requested_apps.push(AppId::parse(&app).unwrap());
        replies
            .queued
            .pop_front()
            .unwrap_or_else(|| replies.fallback.clone())
    };
    match reply {
        Reply::Token(token, remaining) => web::HttpResponse::Ok().json(&IssuedAppCapability {
            token,
            expires_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                + remaining,
        }),
        Reply::Status(status) => web::HttpResponse::build(status).finish(),
        Reply::Oversized => web::HttpResponse::Ok().body(vec![b'x'; 128 * 1024]),
    }
}

pub(super) async fn check(
    endpoint: &WorkflowEndpoint,
    app: &AppId,
    control_key: &ServiceSigningKey,
    worker: Arc<ServiceAuth>,
    output: &zeroship_workflow::service::TaskAssignment,
    data: &[u8],
) {
    let grant = AppGrant {
        app_id: app.clone(),
        operations: [
            AppOperation::Start,
            AppOperation::Status,
            AppOperation::ReadOutput,
        ]
        .into(),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let current = mint_app_capability(
        control_key,
        grant.clone(),
        now,
        APP_CAPABILITY_MAX_LIFETIME_SECONDS,
    )
    .unwrap();
    let stale = mint_app_capability(
        &ServiceSigningKey::generate(),
        grant,
        now,
        APP_CAPABILITY_MAX_LIFETIME_SECONDS,
    )
    .unwrap();
    let valid = Reply::Token(current.clone(), APP_CAPABILITY_MAX_LIFETIME_SECONDS);
    let invalid = Reply::Token(stale.clone(), APP_CAPABILITY_MAX_LIFETIME_SECONDS);
    let (worker_issuer, key) = worker.signing_identity().unwrap();
    let mut keys = ServiceTrustBundle::new();
    keys.trust_signing_key(worker_issuer, key.key_id(), key)
        .unwrap();
    let issuer = Arc::new(Issuer {
        verifier: ServiceAssertionVerifier::new(keys, Arc::new(InMemoryReplayStore::new())),
        replies: Mutex::new(Replies {
            queued: [invalid.clone()].into(),
            fallback: valid.clone(),
            requested_apps: Vec::new(),
        }),
    });
    let state = issuer.clone();
    let control = web::test::server(move || {
        let state = state.clone();
        async move {
            web::App::new().state(state).service(
                web::resource(endpoints::CONTROL_WORKFLOW_CAPABILITY.path_template())
                    .route(web::post().to(issue)),
            )
        }
    })
    .await;
    let bind = |app: AppId| {
        endpoint
            .for_worker_app(app, &control.url(""), worker.clone())
            .unwrap()
    };
    let request = RequestId::mint();
    let expected = endpoint
        .for_app(app.clone(), current.clone())
        .start(&request, "Example", StartOptions::default())
        .await
        .unwrap();
    let managed = bind(app.clone());
    assert_eq!(
        managed
            .start(&request, "Example", StartOptions::default())
            .await
            .unwrap(),
        expected,
        "credential refresh must retain the accepted mutation identity"
    );
    assert_eq!(issuer.requests(), 2);
    managed.clone().status(&expected.id).await.unwrap();
    managed.status(&expected.id).await.unwrap();
    assert_eq!(issuer.requests(), 2, "clones share the app-bound cache");
    assert!(!format!("{managed:?}").contains(current.as_str()));
    let denied = managed
        .broadcast(
            &RequestId::mint(),
            "news",
            SignalOptions {
                signal_type: "ready".into(),
                payload: json!(null),
            },
        )
        .await;
    assert!(matches!(
        denied,
        Err(WorkflowServiceError::PermissionDenied)
    ));
    assert_eq!(
        issuer.requests(),
        2,
        "authorization refusal must not renew credentials"
    );

    let foreign = AppId::mint();
    assert!(matches!(
        bind(foreign.clone()).status(&expected.id).await,
        Err(WorkflowServiceError::NotFound(_))
    ));
    assert_eq!(
        issuer.replies.lock().unwrap().requested_apps.last(),
        Some(&foreign)
    );

    issuer.set([invalid.clone()], valid.clone());
    let before = issuer.requests();
    let reader = bind(app.clone());
    let mut payload = reader
        .read_payload(
            &output.invocation.run_id,
            output.generation,
            zeroship_workflow::service::PayloadSlot::Output,
        )
        .await
        .unwrap();
    let mut received = Vec::new();
    while let Some(chunk) = payload.body.next_chunk().await {
        received.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(received, data);
    assert_eq!(
        issuer.requests() - before,
        2,
        "payload authorization refreshes before streaming"
    );

    issuer.set(
        [],
        Reply::Token(current.clone(), APP_CAPABILITY_MAX_LIFETIME_SECONDS / 20),
    );
    let expiring = bind(app.clone());
    let before = issuer.requests();
    expiring.status(&expected.id).await.unwrap();
    expiring.status(&expected.id).await.unwrap();
    assert_eq!(
        issuer.requests() - before,
        2,
        "renew before the credential expires"
    );
    issuer.set([], Reply::Status(StatusCode::SERVICE_UNAVAILABLE));
    assert!(matches!(
        expiring.status(&expected.id).await,
        Err(WorkflowServiceError::Unavailable(_))
    ));
    issuer.set([], Reply::Status(StatusCode::UNAUTHORIZED));
    assert!(matches!(
        expiring.status(&expected.id).await,
        Err(WorkflowServiceError::Unauthenticated)
    ));

    for remaining in [0, -1, APP_CAPABILITY_MAX_LIFETIME_SECONDS + 60] {
        issuer.set([], Reply::Token(current.clone(), remaining));
        assert!(matches!(
            bind(app.clone()).status(&expected.id).await,
            Err(WorkflowServiceError::Unavailable(_))
        ));
    }
    issuer.set([], Reply::Oversized);
    assert!(matches!(
        bind(app.clone()).status(&expected.id).await,
        Err(WorkflowServiceError::PayloadTooLarge)
    ));
    issuer.set([], invalid);
    let before = issuer.requests();
    assert!(matches!(
        bind(app.clone()).status(&expected.id).await,
        Err(WorkflowServiceError::Unauthenticated)
    ));
    assert_eq!(
        issuer.requests() - before,
        2,
        "a rejected refreshed token must terminate the request"
    );
    assert!(matches!(
        endpoint
            .for_app(app.clone(), stale)
            .status(&expected.id)
            .await,
        Err(WorkflowServiceError::Unauthenticated)
    ));
    assert_eq!(
        issuer.requests() - before,
        2,
        "fixed tokens have no issuer to refresh"
    );

    let role = Arc::new(peer(
        "spiffe://zeroship.ai/svc/worker",
        ServiceSigningKey::generate(),
        ServiceTrustBundle::new(),
    ));
    assert!(matches!(
        endpoint.for_worker_app(app.clone(), &control.url(""), role),
        Err(WorkflowServiceError::Unauthenticated)
    ));
}
